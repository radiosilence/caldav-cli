//! MCP (Model Context Protocol) server for CalDAV.
//!
//! Exposes calendar functionality via two GraphQL tools:
//! - `calendar_schema` — the SDL and the rules for using it
//! - `calendar` — executes a query/mutation
//!
//! The schema is ~5k tokens, so it stays behind a tool call rather than riding
//! in the always-loaded tool descriptions: a session that never mentions a
//! calendar should pay close to nothing for having this server connected.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use tokio::sync::Mutex;

use crate::caldav::CalDavClient;
use crate::config::{Config, DEFAULT_SERVER_URL};

type ToolResult = std::result::Result<CallToolResult, McpError>;

pub mod graphql;

use graphql::{CalDavSchema, SharedClient};

/// Headers carrying the per-request CalDAV credentials in HTTP transport mode.
///
/// A trusted upstream (the hosted gateway, after authenticating the user) sets
/// these before proxying the request. Over stdio they are absent and the
/// configured credentials are used instead.
///
/// Unlike a single-token API, CalDAV needs three values — which is why the
/// gateway stores a credential *set* for this MCP rather than one secret.
pub const USERNAME_HEADER: &str = "x-caldav-username";
pub const PASSWORD_HEADER: &str = "x-caldav-password";
pub const SERVER_URL_HEADER: &str = "x-caldav-url";
/// The calendar new events land in when the model doesn't name one. Not a
/// credential: it doesn't authenticate anything and must not re-key the client
/// cache, so it is resolved separately from [`Credentials`].
pub const CALENDAR_HEADER: &str = "x-caldav-calendar";

/// One account's CalDAV credentials.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Credentials {
    pub server_url: String,
    pub username: String,
    pub password: String,
}

/// Cache of clients keyed by credential set, so discovery (principal →
/// calendar-home → calendars, three round trips) runs once per account rather
/// than on every tool call. Shared across sessions.
type ClientCache = Arc<Mutex<HashMap<Credentials, SharedClient>>>;

/// A non-empty, trimmed header value, if present.
fn header<'a>(headers: Option<&'a http::HeaderMap>, name: &str) -> Option<&'a str> {
    headers
        .and_then(|h| h.get(name))
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// What one account's requests are answered with.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Resolved {
    credentials: Credentials,
    /// Calendar for new events. `None` defers to the account's own default.
    calendar: Option<String>,
}

/// Everything a request resolves to: the per-request headers (HTTP), else the
/// configured values (stdio). Pure so it can be unit-tested without a live
/// [`RequestContext`].
///
/// Username and password must *both* arrive as headers to be used — a partial
/// header set never mixes with the configured credentials, which would
/// otherwise silently authenticate as the wrong account. The calendar is
/// resolved in the same breath rather than separately, so it can never be
/// paired with credentials from the other source: one account's request would
/// otherwise inherit another's configured calendar.
fn resolve(
    headers: Option<&http::HeaderMap>,
    default: Option<&Credentials>,
    default_calendar: Option<&str>,
) -> Option<Resolved> {
    let header = |name: &str| header(headers, name);

    match (header(USERNAME_HEADER), header(PASSWORD_HEADER)) {
        (Some(username), Some(password)) => Some(Resolved {
            credentials: Credentials {
                server_url: header(SERVER_URL_HEADER)
                    .map(str::to_string)
                    .or_else(|| default.map(|d| d.server_url.clone()))
                    .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string()),
                username: username.to_string(),
                password: password.to_string(),
            },
            calendar: header(CALENDAR_HEADER).map(str::to_string),
        }),
        _ => default.cloned().map(|credentials| Resolved {
            credentials,
            calendar: header(CALENDAR_HEADER)
                .or(default_calendar)
                .map(str::to_string),
        }),
    }
}

/// The credentials to fall back on when a request carries no `X-CalDAV-*`
/// headers, and the calendar that goes with them.
///
/// Best-effort by design. A hosted deployment ships no config and no
/// environment, so this is `None` there and every request must bring its own
/// headers — while running it yourself picks up your own account with no
/// ceremony.
fn local_credentials() -> Option<(Credentials, Option<String>)> {
    let config = Config::load().ok()?;
    Some((
        Credentials {
            server_url: config.get_server_url(),
            username: config.get_username().ok()?,
            password: config.get_app_password().ok()?,
        },
        config.get_calendar(),
    ))
}

// ============ Request Types ============

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GraphqlRequest {
    /// The GraphQL query or mutation string
    pub query: String,
    /// Optional JSON-encoded variables for the query
    #[serde(default)]
    pub variables: Option<String>,
}

// ============ Server Implementation ============

#[derive(Clone)]
pub struct CalDavMcp {
    schema: Arc<CalDavSchema>,
    clients: ClientCache,
    /// Credentials used when a request carries no `X-CalDAV-*` headers. Always
    /// set over stdio; over HTTP it is whatever [`local_credentials`] found, so
    /// `None` in a hosted deployment and every request must bring its own.
    default_credentials: Option<Credentials>,
    /// Configured calendar for new events. Requests carrying their own
    /// credentials use [`CALENDAR_HEADER`] instead, never this.
    default_calendar: Option<String>,
    #[allow(dead_code)] // referenced by #[tool_handler] macro expansion
    tool_router: ToolRouter<Self>,
}

impl CalDavMcp {
    fn build(default_credentials: Option<Credentials>, default_calendar: Option<String>) -> Self {
        Self {
            schema: Arc::new(graphql::build_schema()),
            clients: Arc::new(Mutex::new(HashMap::new())),
            default_credentials,
            default_calendar,
            tool_router: Self::tool_router(),
        }
    }

    /// Construct for stdio use: requires credentials in config/env, used for
    /// every request. Errors if none are configured.
    pub fn new() -> anyhow::Result<Self> {
        let config = Config::load()?;
        Ok(Self::build(
            Some(Credentials {
                server_url: config.get_server_url(),
                username: config.get_username()?,
                password: config.get_app_password()?,
            }),
            config.get_calendar(),
        ))
    }

    /// Construct for HTTP use. A request's own `X-CalDAV-*` headers always win;
    /// [`local_credentials`] is the fallback, which exists when you run this
    /// yourself and not in a hosted deployment.
    pub fn http() -> Self {
        match local_credentials() {
            Some((credentials, calendar)) => Self::build(Some(credentials), calendar),
            None => Self::build(None, None),
        }
    }

    fn headers<'a>(&self, ctx: &'a RequestContext<RoleServer>) -> Option<&'a http::HeaderMap> {
        ctx.extensions
            .get::<http::request::Parts>()
            .map(|p| &p.headers)
    }

    fn resolve(&self, ctx: &RequestContext<RoleServer>) -> Option<Resolved> {
        resolve(
            self.headers(ctx),
            self.default_credentials.as_ref(),
            self.default_calendar.as_deref(),
        )
    }

    /// Get or lazily create a client for `creds`, caching it for reuse.
    async fn client_for(&self, creds: &Credentials) -> SharedClient {
        if let Some(existing) = self.clients.lock().await.get(creds) {
            return existing.clone();
        }
        // Construction is cheap and does no I/O — discovery happens lazily on
        // first use and is then memoised inside the client.
        let client: SharedClient = Arc::new(CalDavClient::new(
            creds.server_url.clone(),
            creds.username.clone(),
            creds.password.clone(),
        ));
        self.clients
            .lock()
            .await
            .entry(creds.clone())
            .or_insert(client)
            .clone()
    }

    fn text_result(text: impl Into<String>) -> ToolResult {
        Ok(CallToolResult::success(vec![Content::text(text.into())]))
    }

    fn error_result(msg: impl Into<String>) -> ToolResult {
        Ok(CallToolResult::error(vec![Content::text(msg.into())]))
    }
}

#[tool_router]
impl CalDavMcp {
    #[tool(
        name = "calendar_schema",
        title = "Calendar Schema",
        description = "The calendar API's GraphQL schema. Call once before the first `calendar` query."
    )]
    async fn calendar_schema(&self, ctx: RequestContext<RoleServer>) -> ToolResult {
        // The chosen calendar is per-request, so it can't live in the static
        // server instructions. Here is the earliest the model sees it — it is
        // told to call this tool first.
        let calendar = match self.resolve(&ctx).and_then(|r| r.calendar) {
            Some(name) => format!(
                "New events go to the user's chosen calendar, {name:?}, unless `createEvent` \
                 is given a `calendar` argument.\n\n"
            ),
            None => String::new(),
        };
        Self::text_result(format!("{calendar}{}", self.schema.sdl()))
    }

    #[tool(
        name = "calendar",
        title = "Calendar",
        description = "Execute a GraphQL query or mutation against the calendar API: calendars, agenda, event search, free/busy, and event writes. Get the schema from `calendar_schema` first. Variables are a JSON string."
    )]
    async fn calendar(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(req): Parameters<GraphqlRequest>,
    ) -> ToolResult {
        let Some(resolved) = self.resolve(&ctx) else {
            return Self::error_result(
                "No CalDAV credentials available. Configure them via `caldav-cli auth` \
                 (stdio) or send the X-CalDAV-Username and X-CalDAV-Password headers (HTTP).",
            );
        };
        let client = self.client_for(&resolved.credentials).await;

        let mut request = graphql::request(&req.query, client, resolved.calendar);

        if let Some(ref vars) = req.variables {
            match serde_json::from_str::<serde_json::Value>(vars) {
                Ok(serde_json::Value::Object(map)) => {
                    request = request.variables(async_graphql::Variables::from_json(
                        serde_json::Value::Object(map),
                    ));
                }
                Ok(_) => {
                    return Self::error_result("Variables must be a JSON object");
                }
                Err(e) => {
                    return Self::error_result(format!("Invalid variables JSON: {e}"));
                }
            }
        }

        let response = self.schema.execute(request).await;
        let json = serde_json::to_string_pretty(&response)
            .unwrap_or_else(|e| format!("{{\"error\": \"Serialization failed: {e}\"}}"));

        Self::text_result(json)
    }
}

#[tool_handler]
impl ServerHandler for CalDavMcp {
    fn get_info(&self) -> ServerInfo {
        let server_info = Implementation::new("caldav-cli", env!("CARGO_PKG_VERSION"))
            .with_title("CalDAV MCP Server")
            .with_website_url("https://github.com/radiosilence/caldav-cli");

        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            // Only the fallback for a client asking for a version the SDK
            // doesn't know; anything known is echoed back during negotiation.
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_server_info(server_info)
            // Deliberately terse: instructions and tool descriptions are loaded
            // into every session, most of which never touch a calendar. The
            // schema and the rules for using it are a tool call away, paid only
            // when a calendar is actually in play.
            .with_instructions(
                "The user's calendars, as a small GraphQL API. Read the schema once with \
                 `calendar_schema`, then run queries and mutations with `calendar`. \
                 Creating an event needs no confirmation; updates and deletes are \
                 preview-then-confirm, and never confirm one the user hasn't seen.",
            )
    }
}

/// Run the MCP server with stdio transport. Credentials come from config/env
/// and are used for every request.
pub async fn run_server() -> anyhow::Result<()> {
    use rmcp::{ServiceExt, transport::stdio};

    let service = CalDavMcp::new()?;
    let server = service
        .serve(stdio())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to start MCP server: {}", e))?;

    server
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("MCP server error: {}", e))?;

    Ok(())
}

/// Body of a GraphQL-over-HTTP request, as GraphiQL sends it.
#[derive(serde::Deserialize)]
struct HttpGraphqlRequest {
    query: String,
    #[serde(default)]
    variables: Option<serde_json::Value>,
    #[serde(default, rename = "operationName")]
    operation_name: Option<String>,
}

/// The GraphiQL IDE page. GraphiQL itself is loaded from a CDN with pinned
/// versions and SRI hashes — see `templates/graphiql.html`.
#[derive(askama::Template)]
#[template(path = "graphiql.html")]
struct GraphiqlPage<'a> {
    title: &'a str,
    endpoint: &'a str,
}

/// Whether every top-level selection is an introspection field, and so can be
/// answered from the schema alone.
///
/// GraphiQL sends exactly this on load to build its docs, autocomplete and
/// explorer. Requiring CalDAV credentials for it would mean bad ones leave you
/// with an IDE that cannot describe the API you are trying to explore. Anything
/// it cannot parse, or that mixes in real fields, is not introspection.
fn is_introspection_only(query: &str) -> bool {
    use async_graphql::parser::types::Selection;

    let Ok(doc) = async_graphql::parser::parse_query(query) else {
        return false;
    };
    doc.operations.iter().all(|(_, op)| {
        op.node
            .selection_set
            .node
            .items
            .iter()
            .all(|item| match &item.node {
                Selection::Field(field) => field.node.name.node.starts_with("__"),
                // Fragments could hide anything; make them take the auth path.
                _ => false,
            })
    })
}

/// Plain GraphQL-over-HTTP, for browsers and anything else that speaks it
/// directly rather than through MCP's JSON-RPC envelope. Shares the server's
/// schema, client cache and credential resolution with the `calendar` tool.
async fn graphql_endpoint(
    axum::extract::State(mcp): axum::extract::State<CalDavMcp>,
    headers: http::HeaderMap,
    axum::Json(req): axum::Json<HttpGraphqlRequest>,
) -> axum::Json<async_graphql::Response> {
    // Introspection is answered from the schema, so it neither needs credentials
    // nor touches the network — the IDE stays usable while they are wrong.
    let mut request = if is_introspection_only(&req.query) {
        async_graphql::Request::new(&req.query)
    } else {
        let Some(resolved) = resolve(
            Some(&headers),
            mcp.default_credentials.as_ref(),
            mcp.default_calendar.as_deref(),
        ) else {
            return axum::Json(async_graphql::Response::from_errors(vec![
                async_graphql::ServerError::new(
                    format!(
                        "No CalDAV credentials available. Configure them via `caldav-cli auth` \
                         or send the {USERNAME_HEADER} and {PASSWORD_HEADER} headers."
                    ),
                    None,
                ),
            ]));
        };
        let client = mcp.client_for(&resolved.credentials).await;
        graphql::request(&req.query, client, resolved.calendar)
    };
    if let Some(vars) = req.variables {
        request = request.variables(async_graphql::Variables::from_json(vars));
    }
    if let Some(name) = req.operation_name {
        request = request.operation_name(name);
    }
    axum::Json(mcp.schema.execute(request).await)
}

/// Where the HTTP server listens when no address is given.
pub const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:8080";

/// Which surfaces [`run_http_server`] mounts. Each is independent: MCP's
/// streamable-HTTP transport and a browsable GraphQL endpoint are different
/// things that happen to share a port.
#[derive(Clone, Copy)]
pub struct HttpSurfaces {
    /// MCP streamable-HTTP at `/mcp`.
    pub mcp: bool,
    /// Plain GraphQL-over-HTTP at `/graphql`.
    pub graphql: bool,
    /// The GraphiQL IDE at `/`. Implies `graphql` — it is the IDE's endpoint.
    pub graphiql: bool,
    /// Open the IDE in the default browser once listening.
    pub browser: bool,
}

/// Run the HTTP server on `addr`, mounting whichever of [`HttpSurfaces`] is
/// enabled.
///
/// A request's own `X-CalDAV-*` headers always win; [`local_credentials`] is the
/// fallback. Running this yourself, that means your own account with no
/// ceremony. In a hosted deployment there is no local config, so every request
/// must carry the headers — set by a trusted upstream after authenticating the
/// caller. Do **not** expose this to the internet without such a layer in
/// front: the headers are trusted unconditionally.
pub async fn run_http_server(addr: &str, surfaces: HttpSurfaces) -> anyhow::Result<()> {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    };

    // One shared instance (shared schema + client cache) cloned into each
    // session and used as the axum state for the GraphQL routes.
    let mcp = CalDavMcp::http();
    let mut router = axum::Router::new();

    if surfaces.mcp {
        // Disable rmcp's DNS-rebinding Host allowlist: this transport is designed
        // to run behind a trusted reverse proxy, which forwards an internal Host
        // (e.g. the service name) that the default allowlist
        // (localhost/127.0.0.1/::1) would reject with 403. Rebinding protection
        // guards browsers hitting a localhost MCP directly — irrelevant for a
        // proxied, non-browser-facing backend; the proxy is the security
        // boundary.
        let config = StreamableHttpServerConfig::default().disable_allowed_hosts();
        let service = StreamableHttpService::new(
            {
                let template = mcp.clone();
                move || Ok(template.clone())
            },
            Arc::new(LocalSessionManager::default()),
            config,
        );
        router = router.nest_service("/mcp", service);
        tracing::info!("MCP streamable-HTTP listening on http://{addr}/mcp");
    }

    if surfaces.graphql || surfaces.graphiql {
        router = router.route("/graphql", axum::routing::post(graphql_endpoint));
        tracing::info!("GraphQL endpoint on http://{addr}/graphql");
    }

    if surfaces.graphiql {
        // Rendered once: nothing in the page varies per request, and a template
        // error should stop the server rather than 500 on every hit.
        let ide = askama::Template::render(&GraphiqlPage {
            title: "CalDAV GraphQL",
            endpoint: "/graphql",
        })?;
        router = router.route(
            "/",
            axum::routing::get(move || {
                let ide = ide.clone();
                async move { axum::response::Html(ide) }
            }),
        );
        tracing::info!("GraphiQL IDE on http://{addr}/");
    }

    let listener = tokio::net::TcpListener::bind(addr).await?;

    // Only once the listener is bound, so the browser cannot beat us to it.
    if surfaces.browser {
        let url = format!("http://{addr}/");
        if let Err(e) = open::that_detached(&url) {
            tracing::warn!("Could not open a browser at {url}: {e}");
        }
    }

    axum::serve(listener, router.with_state(mcp))
        .await
        .map_err(|e| anyhow::anyhow!("MCP HTTP server error: {}", e))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(user: &str) -> Credentials {
        Credentials {
            server_url: "https://caldav.example.com".into(),
            username: user.into(),
            password: "pw".into(),
        }
    }

    /// Most cases below only care about the credential half.
    fn resolved_credentials(
        headers: Option<&http::HeaderMap>,
        default: Option<&Credentials>,
    ) -> Option<Credentials> {
        resolve(headers, default, None).map(|r| r.credentials)
    }

    fn headers_with(headers: &[(&str, &str)]) -> http::HeaderMap {
        let mut map = http::HeaderMap::new();
        for (k, v) in headers {
            map.insert(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        map
    }

    #[test]
    fn headers_win_over_default() {
        let headers = headers_with(&[
            (USERNAME_HEADER, "header@x.test"),
            (PASSWORD_HEADER, "header-pw"),
            (SERVER_URL_HEADER, "https://caldav.fastmail.com"),
        ]);
        let got = resolved_credentials(Some(&headers), Some(&creds("default@x.test"))).unwrap();
        assert_eq!(got.username, "header@x.test");
        assert_eq!(got.password, "header-pw");
        assert_eq!(got.server_url, "https://caldav.fastmail.com");
    }

    #[test]
    fn url_header_is_optional_and_falls_back_to_icloud() {
        let headers = headers_with(&[(USERNAME_HEADER, "me@icloud.com"), (PASSWORD_HEADER, "pw")]);
        let got = resolved_credentials(Some(&headers), None).unwrap();
        assert_eq!(got.server_url, DEFAULT_SERVER_URL);
    }

    #[test]
    fn url_header_absent_inherits_the_default_server() {
        let headers = headers_with(&[(USERNAME_HEADER, "me@x.test"), (PASSWORD_HEADER, "pw")]);
        let got = resolved_credentials(Some(&headers), Some(&creds("other@x.test"))).unwrap();
        assert_eq!(got.server_url, "https://caldav.example.com");
        assert_eq!(got.username, "me@x.test");
    }

    #[test]
    fn falls_back_to_default_when_no_headers() {
        let got = resolved_credentials(Some(&headers_with(&[])), Some(&creds("default@x.test")));
        assert_eq!(got.unwrap().username, "default@x.test");
    }

    #[test]
    fn falls_back_to_default_when_no_request_context() {
        // stdio: no HTTP headers in the request context at all.
        let got = resolved_credentials(None, Some(&creds("default@x.test")));
        assert_eq!(got.unwrap().username, "default@x.test");
    }

    #[test]
    fn partial_headers_never_mix_with_the_default() {
        // A username header with no password must not authenticate as the
        // configured account under someone else's name.
        let headers = headers_with(&[(USERNAME_HEADER, "attacker@x.test")]);
        let got = resolved_credentials(Some(&headers), Some(&creds("owner@x.test"))).unwrap();
        assert_eq!(got.username, "owner@x.test");
    }

    #[test]
    fn blank_headers_are_treated_as_absent() {
        let headers = headers_with(&[(USERNAME_HEADER, "  "), (PASSWORD_HEADER, "pw")]);
        let got = resolved_credentials(Some(&headers), Some(&creds("owner@x.test"))).unwrap();
        assert_eq!(got.username, "owner@x.test");
    }

    #[test]
    fn calendar_header_overrides_the_configured_one() {
        let headers = headers_with(&[(CALENDAR_HEADER, "Work")]);
        let got = resolve(Some(&headers), Some(&creds("me@x.test")), Some("Personal")).unwrap();
        assert_eq!(got.calendar.as_deref(), Some("Work"));
    }

    #[test]
    fn header_credentials_never_inherit_the_configured_calendar() {
        // Another account's request must not land events in this one's calendar.
        let headers = headers_with(&[(USERNAME_HEADER, "other@x.test"), (PASSWORD_HEADER, "pw")]);
        let got = resolve(Some(&headers), Some(&creds("owner@x.test")), Some("Owner")).unwrap();
        assert_eq!(got.credentials.username, "other@x.test");
        assert_eq!(got.calendar, None);
    }

    #[test]
    fn no_calendar_anywhere_defers_to_the_server() {
        let default = creds("me@x.test");
        let plain = resolve(Some(&headers_with(&[])), Some(&default), None).unwrap();
        assert_eq!(plain.calendar, None);
        assert_eq!(resolve(None, Some(&default), None).unwrap().calendar, None);
        // A blank header is not a choice — it must not shadow the config.
        let headers = headers_with(&[(CALENDAR_HEADER, "   ")]);
        let got = resolve(Some(&headers), Some(&default), Some("Personal")).unwrap();
        assert_eq!(got.calendar.as_deref(), Some("Personal"));
    }

    #[test]
    fn introspection_needs_no_credentials() {
        // What GraphiQL sends on load, plus the shapes around it.
        assert!(is_introspection_only("{ __schema { queryType { name } } }"));
        assert!(is_introspection_only(
            "query IntrospectionQuery { __schema { types { name } } }"
        ));
        assert!(is_introspection_only(
            "{ __type(name: \"Event\") { name } }"
        ));
        assert!(is_introspection_only("{ __typename }"));
    }

    #[test]
    fn real_fields_still_need_credentials() {
        assert!(!is_introspection_only("{ calendars { nodes { name } } }"));
        // Mixed with introspection, and nested below it, still count as real.
        assert!(!is_introspection_only(
            "{ __typename calendars { nodes { name } } }"
        ));
        assert!(!is_introspection_only(
            "mutation { deleteEvent(action: PREVIEW, uid: \"x\") { preview } }"
        ));
        // Fragments could hide anything, and unparseable input proves nothing.
        assert!(!is_introspection_only(
            "{ ...F } fragment F on Query { __typename }"
        ));
        assert!(!is_introspection_only("{ this is not graphql"));
    }

    #[test]
    fn none_when_neither_headers_nor_default() {
        // hosted mode with no upstream-injected credentials — must refuse.
        assert_eq!(resolved_credentials(Some(&headers_with(&[])), None), None);
        assert_eq!(resolved_credentials(None, None), None);
    }
}
