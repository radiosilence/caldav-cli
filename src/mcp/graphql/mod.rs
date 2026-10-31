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

/// Ceiling on a query's declared cost, the sum of every field's page size
/// times the cost of what it selects. See [`build_schema`].
const MAX_COMPLEXITY: usize = 250_000;

/// Build the GraphQL schema with only the process-shared preview-nonce store.
///
/// The CalDAV client is **not** baked in — it is supplied per request via
/// [`request`] so a single schema can serve many tenants, each with their own
/// credentials. The nonce store stays schema-level because preview→confirm spans
/// two separate requests.
pub fn build_schema() -> CalDavSchema {
    // Fields declare their cost as page size times what is below, and the
    // descriptions surface it so a caller can pick a sensible page size. The
    // ceiling sits far above any query worth asking, and refuses only the
    // multiplied ones: a page of 100 events each asking for 100 conflicts is
    // about 30,000, and a third level of nesting is a hundred times that.
    // Fields that do network work without paging (a UID lookup, a free-busy
    // read, a probe, every write) carry a fixed cost, so aliases of them are
    // priced too.
    //
    // Depth is capped as well: the graph has cycles, and nothing else bounds
    // them.
    Schema::build(
        QueryRoot,
        MutationRoot::default(),
        async_graphql::EmptySubscription,
    )
    .data(types::NonceStore::default())
    .limit_depth(MAX_DEPTH)
    .limit_complexity(MAX_COMPLEXITY)
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
    // Every request this query makes, by whatever path, shares one limit.
    let client: SharedClient = std::sync::Arc::new(client.limited());
    let loaders = loaders::Loaders::new(client.clone());
    async_graphql::Request::new(query)
        .data(client)
        .data(loaders.budget)
        .data(filter::FilterBudget::default())
        .data(DefaultCalendar(default_calendar))
        .data(loaders.calendar)
        .data(loaders.default)
        .data(loaders.window)
        .data(loaders.resource)
        .data(loaders.uid)
}
