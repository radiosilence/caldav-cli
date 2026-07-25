//! GraphQL schema for the CalDAV MCP.
//!
//! One composable query interface over the CalDAV client, rather than a tool
//! per operation — the same shape `fastmail-cli` exposes for email.

use async_graphql::Schema;

pub mod connection;
pub mod filter;
pub mod loaders;
mod mutation;
mod query;
#[cfg(test)]
mod tests;
pub mod types;

use mutation::MutationRoot;
use query::QueryRoot;

pub type CalDavSchema = Schema<QueryRoot, MutationRoot, async_graphql::EmptySubscription>;

/// The per-request CalDAV client, injected into each GraphQL execution as
/// request data. Shared (`Arc`) so a client that has already paid for discovery
/// is reused across requests for the same account. The client's own methods
/// take `&self`, so no lock is needed.
pub type SharedClient = std::sync::Arc<crate::caldav::CalDavClient>;

/// The calendar the user picked for new events, injected per request alongside
/// the client. `None` falls through to the server's own default calendar.
/// Reads are never scoped by it — "what's on today" must span the account.
pub struct DefaultCalendar(pub Option<String>);

/// Maximum selection-set nesting. The graph has cycles by design — an event
/// belongs to a calendar whose events belong to calendars — so unbounded depth
/// would let one query walk forever. 15 is far past anything useful:
/// `calendars → events → conflicts → calendar → events` is 5.
const MAX_DEPTH: usize = 15;

/// Build the GraphQL schema with only the process-shared preview-nonce store.
///
/// The CalDAV client is **not** baked in — it is supplied per request via
/// [`request`] so a single schema can serve many tenants, each with their own
/// credentials. The nonce store stays schema-level because preview→confirm spans
/// two separate requests.
pub fn build_schema() -> CalDavSchema {
    // Complexity is deliberately **not** capped. Fields still declare costs, and
    // those costs are surfaced in the descriptions so a caller can pick a
    // sensible page size — but they are guidance, not a gate. Refusing an
    // expensive-but-legitimate query leaves the caller guessing at a threshold
    // it cannot see, which is a bad deal for a model composing a query.
    //
    // Depth stays capped: the graph has cycles, and nothing else bounds them.
    Schema::build(QueryRoot, MutationRoot, async_graphql::EmptySubscription)
        .data(types::NonceStore::default())
        .limit_depth(MAX_DEPTH)
        .finish()
}

/// Build a GraphQL request carrying everything a resolver may need: the CalDAV
/// client, the user's chosen default calendar, and a fresh set of DataLoaders.
///
/// Loaders are per request on purpose — their cache is then a request-scoped
/// cache, so repeating a key inside one query is free while nothing is retained
/// long enough to go stale.
pub fn request(
    query: &str,
    client: SharedClient,
    default_calendar: Option<String>,
) -> async_graphql::Request {
    let loaders = loaders::Loaders::new(client.clone());
    async_graphql::Request::new(query)
        .data(client)
        .data(DefaultCalendar(default_calendar))
        .data(loaders.calendar)
        .data(loaders.window)
        .data(loaders.resource)
        .data(loaders.uid)
}
