//! DataLoaders backing the nested GraphQL resolvers.
//!
//! Every read that leaves the process goes through one of these; no resolver
//! calls the CalDAV client directly. async-graphql resolves sibling fields and
//! list elements concurrently, so N resolvers each asking for one key collapse
//! into a single batched `load` per loader.
//!
//! What "batched" can mean here is set by CalDAV, which is stingier than JMAP:
//!
//! | Data | Request | Batching |
//! | --- | --- | --- |
//! | Calendar listing | one `PROPFIND` on the calendar home | whole list, keyed by `()` — one call however many ask |
//! | Events in a window | `calendar-query` REPORT, one per collection | no plural form; identical windows deduplicate, distinct ones are issued concurrently |
//! | Events by resource path | `calendar-multiget` REPORT | **true batch** — every href in one collection, one request |
//! | An event by UID | `calendar-query` REPORT | filters are all-AND (RFC 4791 §9.7), so no OR over UIDs: one request per calendar per UID, issued concurrently |
//!
//! Loaders are built **per GraphQL request** (see [`super::request`]), so their
//! cache is a request-scoped cache: repeating a key inside one query is free,
//! and nothing is held long enough to go stale.

use std::collections::HashMap;
use std::sync::Arc;

use async_graphql::dataloader::{DataLoader, HashMapCache, Loader};
use chrono::{DateTime, Utc};

use super::SharedClient;
use crate::caldav::fan_out;
use crate::error::Error;
use crate::models::{Calendar, Event};

/// Loader errors are cloned out to every waiter in a batch, so they must be
/// cheap to clone — hence the `Arc`.
pub type LoadError = Arc<Error>;

/// Turn a loader error into a GraphQL error. `Arc<Error>` can't implement
/// `Into<async_graphql::Error>` here (both types are foreign), so resolvers call
/// this explicitly.
pub fn to_gql_error(e: LoadError) -> async_graphql::Error {
    async_graphql::Error::new(e.to_string())
}

/// Loads the account's calendar collections.
///
/// Every calendar question — by id, by name, the default, which collections a
/// range query should span — is answered by filtering this one list, so there is
/// one loader keyed by `()` and each question is in-memory work on its result.
///
/// This is the loader that fixes a real bug. [`crate::caldav::CalDavClient`]
/// memoises the listing for its whole life and clients are pooled per
/// credential, so before this a long-running MCP server never saw a calendar
/// created, renamed, or deleted after start-up. Keyed by unit, the
/// request-scoped cache gives the same deduplication with none of the staleness.
pub struct CalendarListLoader(SharedClient);

impl Loader<()> for CalendarListLoader {
    type Value = Arc<Vec<Calendar>>;
    type Error = LoadError;

    async fn load(&self, _keys: &[()]) -> Result<HashMap<(), Arc<Vec<Calendar>>>, Self::Error> {
        let calendars = self.0.list_calendars().await.map_err(Arc::new)?;
        Ok(HashMap::from([((), Arc::new(calendars))]))
    }
}

/// One calendar's events over one time window.
///
/// Both the window and `expand` are part of the key because both change what
/// comes back: two resolvers asking for the same calendar over the same range
/// share a fetch, and asking for occurrences versus master events does not.
#[derive(Clone, Hash, PartialEq, Eq, Debug)]
pub struct Window {
    pub calendar_id: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub expand: bool,
}

/// Runs `calendar-query` REPORTs.
///
/// CalDAV has no cross-collection query, so a batch of N distinct windows is N
/// requests whatever we do. What the loader buys is that they go out
/// concurrently rather than one after another, and that duplicate windows —
/// which is what a graph query produces, every event on a page asking about the
/// same day — collapse to one.
pub struct EventWindowLoader {
    client: SharedClient,
    calendars: Arc<Calendars>,
}

impl Loader<Window> for EventWindowLoader {
    type Value = Arc<Vec<Event>>;
    type Error = LoadError;

    async fn load(&self, keys: &[Window]) -> Result<HashMap<Window, Arc<Vec<Event>>>, Self::Error> {
        let calendars = load_calendars(&self.calendars).await?;

        let calls: Vec<_> = keys
            .iter()
            .filter_map(|key| {
                let calendar = calendars.iter().find(|c| c.id == key.calendar_id)?;
                Some(async move {
                    let events = self
                        .client
                        .list_events(calendar, key.start, key.end, key.expand)
                        .await;
                    (key.clone(), events)
                })
            })
            .collect();

        let mut out = HashMap::with_capacity(calls.len());
        for (key, events) in fan_out(calls).await {
            match events {
                Ok(events) => {
                    out.insert(key, Arc::new(events));
                }
                // One unreadable calendar (shared, or a server hiccup) shouldn't
                // sink the whole query. An absent key resolves to an empty list,
                // matching what the CLI does with the same failure.
                Err(e) => {
                    tracing::warn!(calendar = %key.calendar_id, error = %e, "skipping calendar")
                }
            }
        }
        Ok(out)
    }
}

/// One `.ics` resource, addressed by the collection holding it and its path.
#[derive(Clone, Hash, PartialEq, Eq, Debug)]
pub struct Resource {
    pub calendar_id: String,
    pub href: String,
}

/// Fetches `.ics` resources through `calendar-multiget`.
///
/// This is CalDAV's one genuine batch: every href wanted from a collection goes
/// in a single REPORT. A page of expanded occurrences asking for their master
/// events therefore costs one request per calendar, not one per occurrence.
pub struct EventResourceLoader {
    client: SharedClient,
    calendars: Arc<Calendars>,
}

impl Loader<Resource> for EventResourceLoader {
    type Value = Event;
    type Error = LoadError;

    async fn load(&self, keys: &[Resource]) -> Result<HashMap<Resource, Event>, Self::Error> {
        let calendars = load_calendars(&self.calendars).await?;

        let mut by_calendar: HashMap<&str, Vec<String>> = HashMap::new();
        for key in keys {
            by_calendar
                .entry(key.calendar_id.as_str())
                .or_default()
                .push(key.href.clone());
        }

        let calls: Vec<_> = by_calendar
            .into_iter()
            .filter_map(|(calendar_id, hrefs)| {
                let calendar = calendars.iter().find(|c| c.id == calendar_id)?;
                Some(async move {
                    (
                        calendar_id,
                        self.client.multiget_events(calendar, &hrefs).await,
                    )
                })
            })
            .collect();

        let mut out = HashMap::with_capacity(keys.len());
        for (calendar_id, events) in fan_out(calls).await {
            match events {
                Ok(events) => {
                    for event in events {
                        // Prefer the master event when a series has overrides —
                        // an occurrence and its master share one resource.
                        let key = Resource {
                            calendar_id: calendar_id.to_string(),
                            href: event.href.clone(),
                        };
                        if event.recurrence_id.is_none() || !out.contains_key(&key) {
                            out.insert(key, event);
                        }
                    }
                }
                Err(e) => tracing::warn!(calendar = %calendar_id, error = %e, "multiget failed"),
            }
        }
        Ok(out)
    }
}

/// An event addressed by UID, optionally pinned to one collection.
#[derive(Clone, Hash, PartialEq, Eq, Debug)]
pub struct Uid {
    pub uid: String,
    /// Restrict the search to one calendar. `None` sweeps every collection that
    /// holds events.
    pub calendar_id: Option<String>,
}

/// Finds events by iCalendar UID.
///
/// The unbatchable one. A UID lookup is a `calendar-query` with a `prop-filter`,
/// and CalDAV ANDs every sibling filter (RFC 4791 §9.7) — there is no way to ask
/// for "UID a or UID b". So M UIDs over N calendars is M×N requests; the loader
/// issues them concurrently and deduplicates repeats, which is all that is
/// available. Pinning `calendar_id` brings N to 1.
///
/// Prefer [`EventResourceLoader`] wherever an href is already in hand.
pub struct EventUidLoader {
    client: SharedClient,
    calendars: Arc<Calendars>,
}

impl Loader<Uid> for EventUidLoader {
    type Value = Event;
    type Error = LoadError;

    async fn load(&self, keys: &[Uid]) -> Result<HashMap<Uid, Event>, Self::Error> {
        let calendars = load_calendars(&self.calendars).await?;

        let calls: Vec<_> = keys
            .iter()
            .flat_map(|key| {
                calendars
                    .iter()
                    .filter(move |c| match &key.calendar_id {
                        Some(id) => &c.id == id,
                        None => c.supports_events,
                    })
                    .map(move |calendar| async move {
                        (
                            key.clone(),
                            self.client.event_in_calendar(&key.uid, calendar).await,
                        )
                    })
            })
            .collect();

        let mut out: HashMap<Uid, Event> = HashMap::with_capacity(keys.len());
        for (key, found) in fan_out(calls).await {
            // Calendars are searched in listing order and `fan_out` preserves
            // it, so the first hit wins deterministically — the same calendar
            // the CLI's short-circuiting scan would have stopped at.
            if let Ok(Some(event)) = found {
                out.entry(key).or_insert(event);
            }
        }
        Ok(out)
    }
}

/// The account's calendars, through the loader.
async fn load_calendars(loader: &Calendars) -> Result<Arc<Vec<Calendar>>, LoadError> {
    loader
        .load_one(())
        .await?
        .ok_or_else(|| Arc::new(Error::Discovery("calendar listing")))
}

/// Cap on hrefs per `calendar-multiget`. Servers bound request body size, and a
/// page of results is 100 at most, so this collapses any realistic fan-out into
/// one request while staying well inside what a server will accept.
const MULTIGET_BATCH: usize = 100;

// Loader handles as resolvers see them. `DataLoader::new` only deduplicates
// within a single batch, so these are built `with_cache` to get a genuine
// request-scoped cache: a calendar pulled for one field is free for every later
// field in the same query.
pub type Calendars = DataLoader<CalendarListLoader, HashMapCache>;
pub type EventWindows = DataLoader<EventWindowLoader, HashMapCache>;
pub type EventResources = DataLoader<EventResourceLoader, HashMapCache>;
pub type EventUids = DataLoader<EventUidLoader, HashMapCache>;

/// All loaders for one GraphQL request, ready to be attached as request data.
pub struct Loaders {
    pub calendar: Arc<Calendars>,
    pub window: EventWindows,
    pub resource: EventResources,
    pub uid: EventUids,
}

impl Loaders {
    pub fn new(client: SharedClient) -> Self {
        // The calendar loader is shared rather than duplicated: the other three
        // all need the listing to turn an id into a collection URL, and going
        // through the same handle means they hit the same request-scoped cache
        // instead of each paying for their own PROPFIND.
        let calendar = Arc::new(Calendars::with_cache(
            CalendarListLoader(client.clone()),
            tokio::spawn,
            HashMapCache::default(),
        ));

        Self {
            window: EventWindows::with_cache(
                EventWindowLoader {
                    client: client.clone(),
                    calendars: calendar.clone(),
                },
                tokio::spawn,
                HashMapCache::default(),
            ),
            resource: EventResources::with_cache(
                EventResourceLoader {
                    client: client.clone(),
                    calendars: calendar.clone(),
                },
                tokio::spawn,
                HashMapCache::default(),
            )
            .max_batch_size(MULTIGET_BATCH),
            uid: EventUids::with_cache(
                EventUidLoader {
                    client,
                    calendars: calendar.clone(),
                },
                tokio::spawn,
                HashMapCache::default(),
            ),
            calendar,
        }
    }
}
