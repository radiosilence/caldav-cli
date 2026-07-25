//! GraphQL schema for the CalDAV MCP.
//!
//! One composable query interface over the CalDAV client, rather than a tool
//! per operation — the same shape `fastmail-cli` exposes for email.

use async_graphql::Schema;

mod mutation;
mod query;
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

/// Build the GraphQL schema with only the process-shared preview-nonce store.
///
/// The CalDAV client is **not** baked in — it is supplied per request via
/// [`async_graphql::Request::data`] so a single schema can serve many tenants,
/// each with their own credentials. The nonce store stays schema-level because
/// preview→confirm spans two separate requests.
pub fn build_schema() -> CalDavSchema {
    Schema::build(QueryRoot, MutationRoot, async_graphql::EmptySubscription)
        .data(types::NonceStore::default())
        .finish()
}
