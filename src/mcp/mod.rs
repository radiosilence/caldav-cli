//! MCP (Model Context Protocol) server for CalDAV.
//!
//! Exposes calendar functionality via two GraphQL tools:
//! - `schema_sdl` — returns the full GraphQL SDL for introspection
//! - `graphql` — executes a GraphQL query/mutation

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
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
fn header<'a>(parts: Option<&'a http::request::Parts>, name: &str) -> Option<&'a str> {
    parts
        .and_then(|p| p.headers.get(name))
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// The user's chosen calendar for new events: the per-request header (HTTP),
/// else the configured value (stdio). `None` leaves the choice to the server's
/// own default calendar.
fn resolve_default_calendar(
    parts: Option<&http::request::Parts>,
    default: Option<&str>,
) -> Option<String> {
    header(parts, CALENDAR_HEADER)
        .or(default)
        .map(str::to_string)
}

/// Prefer the per-request headers (HTTP), else fall back to the configured
/// default (stdio). Pure so it can be unit-tested without a live
/// [`RequestContext`].
///
/// Username and password must *both* arrive as headers to be used — a partial
/// header set never mixes with the configured credentials, which would
/// otherwise silently authenticate as the wrong account.
fn resolve_credentials(
    parts: Option<&http::request::Parts>,
    default: Option<&Credentials>,
) -> Option<Credentials> {
    let header = |name: &str| header(parts, name);

    match (header(USERNAME_HEADER), header(PASSWORD_HEADER)) {
        (Some(username), Some(password)) => Some(Credentials {
            server_url: header(SERVER_URL_HEADER)
                .map(str::to_string)
                .or_else(|| default.map(|d| d.server_url.clone()))
                .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string()),
            username: username.to_string(),
            password: password.to_string(),
        }),
        _ => default.cloned(),
    }
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
    /// Fallback credentials for stdio mode (loaded from config). `None` in
    /// hosted HTTP mode, where they must arrive per request via the headers.
    default_credentials: Option<Credentials>,
    /// Configured calendar for new events (stdio). Hosted requests carry their
    /// own in [`CALENDAR_HEADER`].
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

    /// Construct for hosted HTTP use: no default credentials. Each request must
    /// carry its own via the headers, injected by the trusted upstream service.
    pub fn hosted() -> Self {
        Self::build(None, None)
    }

    fn parts<'a>(&self, ctx: &'a RequestContext<RoleServer>) -> Option<&'a http::request::Parts> {
        ctx.extensions.get::<http::request::Parts>()
    }

    fn resolve_credentials(&self, ctx: &RequestContext<RoleServer>) -> Option<Credentials> {
        resolve_credentials(self.parts(ctx), self.default_credentials.as_ref())
    }

    fn resolve_default_calendar(&self, ctx: &RequestContext<RoleServer>) -> Option<String> {
        resolve_default_calendar(self.parts(ctx), self.default_calendar.as_deref())
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
        description = "Returns the full GraphQL SDL (Schema Definition Language) for the CalDAV API. Call this first to discover available queries, mutations, types, and their arguments. The schema covers calendars, events, agenda, search, free/busy, and event creation, updates, and deletion."
    )]
    async fn schema_sdl(&self, ctx: RequestContext<RoleServer>) -> ToolResult {
        // The chosen calendar is per-request, so it can't live in the static
        // server instructions. Prepending it here is the earliest the model
        // sees it — it is told to call this tool first.
        match self.resolve_default_calendar(&ctx) {
            Some(calendar) => Self::text_result(format!(
                "# New events go to the user's chosen calendar, {calendar:?}, unless \
                 createEvent is given a `calendar` argument.\n\n{}",
                self.schema.sdl()
            )),
            None => Self::text_result(self.schema.sdl()),
        }
    }

    #[tool(
        description = "Execute a GraphQL query or mutation against the CalDAV API. Use `schema_sdl` first to discover the schema. Supports listing calendars, reading the agenda, searching events, checking free/busy, and creating, updating, or deleting events (with the preview/confirm pattern). Pass variables as a JSON string."
    )]
    async fn graphql(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(req): Parameters<GraphqlRequest>,
    ) -> ToolResult {
        let Some(creds) = self.resolve_credentials(&ctx) else {
            return Self::error_result(
                "No CalDAV credentials available. Configure them via `caldav-cli auth` \
                 (stdio) or send the X-CalDAV-Username and X-CalDAV-Password headers (HTTP).",
            );
        };
        let client = self.client_for(&creds).await;

        let mut request =
            async_graphql::Request::new(&req.query)
                .data(client)
                .data(graphql::DefaultCalendar(
                    self.resolve_default_calendar(&ctx),
                ));

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
            .with_protocol_version(rmcp::model::ProtocolVersion::V_2024_11_05)
            .with_server_info(server_info)
            .with_instructions(
                "CalDAV MCP Server — GraphQL interface for calendar operations.\n\n\
                ## Getting Started\n\
                1. Call `schema_sdl` to get the full GraphQL schema\n\
                2. Use `graphql` to execute queries and mutations\n\n\
                ## Common Queries\n\
                ```graphql\n\
                # List calendars\n\
                { calendars { id name color readOnly } }\n\n\
                # What's on today\n\
                { agenda(days: 1, tz: \"Europe/London\") { id summary location start { dateTime date allDay } end { dateTime } } }\n\n\
                # A window of events\n\
                { events(start: \"2026-07-24\", days: 7) { id summary start { dateTime date } calendar } }\n\n\
                # Find something\n\
                { searchEvents(query: \"dentist\", days: 90) { id summary start { dateTime date } } }\n\n\
                # When am I busy?\n\
                { freeBusy(start: \"today\", days: 3) { start end status } }\n\
                ```\n\n\
                ## Writing (ALWAYS preview first!)\n\
                ```graphql\n\
                # Step 1: Preview\n\
                mutation { createEvent(action: PREVIEW, summary: \"Coffee\", start: \"tomorrow 15:00\", durationMinutes: 30, tz: \"Europe/London\") { preview confirmationToken } }\n\n\
                # Step 2: After user approval, confirm with the token\n\
                mutation { createEvent(action: CONFIRM, summary: \"Coffee\", start: \"tomorrow 15:00\", durationMinutes: 30, tz: \"Europe/London\", confirmationToken: \"...\") { event { id summary } } }\n\
                ```\n\
                `updateEvent` and `deleteEvent` follow the same two-step pattern.\n\n\
                ## Notes\n\
                - Times accept ISO 8601, 'YYYY-MM-DD HH:MM', 'today'/'tomorrow', and offsets like '+2h'.\n\
                - Pass `tz` (e.g. Europe/London) whenever the user means a local wall-clock time.\n\
                - `events`/`agenda` expand recurring series by default. To edit a series, fetch it with `expand: false` and update the master event.\n\n\
                ## Safety Rules\n\
                - NEVER create, update, or delete without showing the PREVIEW output first\n\
                - NEVER confirm a write without explicit user approval\n\
                - Deletions cannot be undone — read the preview back to the user verbatim",
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

/// Run the MCP server over streamable HTTP on `addr`, mounted at `/mcp`.
///
/// No credentials are baked in: each request must carry them in the
/// `X-CalDAV-*` headers, set by a trusted upstream after authenticating the
/// user. This is the transport the hosted gateway puts behind OAuth.
pub async fn run_http_server(addr: &str) -> anyhow::Result<()> {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    };

    // One shared instance (shared schema + client cache) cloned into each session.
    let template = CalDavMcp::hosted();
    // Disable rmcp's DNS-rebinding Host allowlist: this transport is designed to
    // run behind a trusted reverse proxy, which forwards an internal Host (e.g.
    // the service name) that the default allowlist (localhost/127.0.0.1/::1)
    // would reject with 403. Rebinding protection guards browsers hitting a
    // localhost MCP directly — irrelevant for a proxied, non-browser-facing
    // backend; the proxy is the security boundary.
    let config = StreamableHttpServerConfig::default().disable_allowed_hosts();
    let service = StreamableHttpService::new(
        move || Ok(template.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );

    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("MCP streamable-HTTP server listening on http://{addr}/mcp");
    axum::serve(listener, router)
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

    fn parts_with(headers: &[(&str, &str)]) -> http::request::Parts {
        let mut builder = http::Request::builder();
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        builder.body(()).unwrap().into_parts().0
    }

    #[test]
    fn headers_win_over_default() {
        let parts = parts_with(&[
            (USERNAME_HEADER, "header@x.test"),
            (PASSWORD_HEADER, "header-pw"),
            (SERVER_URL_HEADER, "https://caldav.fastmail.com"),
        ]);
        let got = resolve_credentials(Some(&parts), Some(&creds("default@x.test"))).unwrap();
        assert_eq!(got.username, "header@x.test");
        assert_eq!(got.password, "header-pw");
        assert_eq!(got.server_url, "https://caldav.fastmail.com");
    }

    #[test]
    fn url_header_is_optional_and_falls_back_to_icloud() {
        let parts = parts_with(&[(USERNAME_HEADER, "me@icloud.com"), (PASSWORD_HEADER, "pw")]);
        let got = resolve_credentials(Some(&parts), None).unwrap();
        assert_eq!(got.server_url, DEFAULT_SERVER_URL);
    }

    #[test]
    fn url_header_absent_inherits_the_default_server() {
        let parts = parts_with(&[(USERNAME_HEADER, "me@x.test"), (PASSWORD_HEADER, "pw")]);
        let got = resolve_credentials(Some(&parts), Some(&creds("other@x.test"))).unwrap();
        assert_eq!(got.server_url, "https://caldav.example.com");
        assert_eq!(got.username, "me@x.test");
    }

    #[test]
    fn falls_back_to_default_when_no_headers() {
        let got = resolve_credentials(Some(&parts_with(&[])), Some(&creds("default@x.test")));
        assert_eq!(got.unwrap().username, "default@x.test");
    }

    #[test]
    fn falls_back_to_default_when_no_parts() {
        // stdio: no HTTP parts in the request context at all.
        let got = resolve_credentials(None, Some(&creds("default@x.test")));
        assert_eq!(got.unwrap().username, "default@x.test");
    }

    #[test]
    fn partial_headers_never_mix_with_the_default() {
        // A username header with no password must not authenticate as the
        // configured account under someone else's name.
        let parts = parts_with(&[(USERNAME_HEADER, "attacker@x.test")]);
        let got = resolve_credentials(Some(&parts), Some(&creds("owner@x.test"))).unwrap();
        assert_eq!(got.username, "owner@x.test");
    }

    #[test]
    fn blank_headers_are_treated_as_absent() {
        let parts = parts_with(&[(USERNAME_HEADER, "  "), (PASSWORD_HEADER, "pw")]);
        let got = resolve_credentials(Some(&parts), Some(&creds("owner@x.test"))).unwrap();
        assert_eq!(got.username, "owner@x.test");
    }

    #[test]
    fn calendar_header_overrides_the_configured_one() {
        let parts = parts_with(&[(CALENDAR_HEADER, "Work")]);
        assert_eq!(
            resolve_default_calendar(Some(&parts), Some("Personal")).as_deref(),
            Some("Work")
        );
    }

    #[test]
    fn no_calendar_anywhere_defers_to_the_server() {
        assert_eq!(resolve_default_calendar(Some(&parts_with(&[])), None), None);
        assert_eq!(resolve_default_calendar(None, None), None);
        // A blank header is not a choice — it must not shadow the config.
        let parts = parts_with(&[(CALENDAR_HEADER, "   ")]);
        assert_eq!(
            resolve_default_calendar(Some(&parts), Some("Personal")).as_deref(),
            Some("Personal")
        );
    }

    #[test]
    fn none_when_neither_headers_nor_default() {
        // hosted mode with no upstream-injected credentials — must refuse.
        assert_eq!(resolve_credentials(Some(&parts_with(&[])), None), None);
        assert_eq!(resolve_credentials(None, None), None);
    }
}
