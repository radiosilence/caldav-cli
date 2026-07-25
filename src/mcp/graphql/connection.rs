//! Relay-style connections over the CalDAV reads.
//!
//! **Paging is slicing.** CalDAV has no windowed query — a `calendar-query`
//! REPORT hands back everything in the time range, and there is no offset,
//! anchor or continuation token to ask for less. So the time range is the real
//! bound on what gets fetched, and `first`/`after` decide what of it gets
//! serialised. Two honest consequences, both said in the schema rather than
//! implied away:
//!
//! - `totalCount` is **free**. It is a length, not extra work for the server, so
//!   selecting it costs nothing and it is always exact.
//! - A cursor lasts only as long as its event. Cursors are event ids (plus the
//!   `RECURRENCE-ID` for one occurrence of a series, since a series repeats its
//!   UID), which keeps them stable when neighbouring events change and legible
//!   to a model composing the next page — it is an id it has already seen. If
//!   the event is gone next time you get a "restart pagination" error rather
//!   than a quietly different page.

use async_graphql::connection::{Connection, Edge};
use async_graphql::{Context, OutputType, Result, SimpleObject};

use super::filter::{EventFilter, EventSort, sort_events};
use super::loaders::{EventWindows, Window, to_gql_error};
use super::types::{GqlEvent, all_calendars, clamp_page};
use crate::caldav::resolve_calendar;
use crate::commands::RangeArgs;
use crate::models::{Calendar, Event};

/// Arguments every connection accepts.
pub struct PageArgs {
    pub after: Option<String>,
    pub before: Option<String>,
    pub first: Option<i32>,
    pub last: Option<i32>,
}

/// The extra field on a connection built from a list already in hand.
#[derive(SimpleObject)]
pub struct CountFields {
    /// How many items match, before paging. Free to ask for — this list arrived
    /// whole from one request, so the count is a length.
    pub total_count: u64,
}

/// A connection over an in-memory list. The GraphQL type name comes from the
/// node, so `Calendar` gives `CalendarConnection`.
pub type ListConnection<T> = Connection<String, T, CountFields>;

/// Extra fields on an event connection.
#[derive(SimpleObject)]
pub struct EventConnectionFields {
    /// Total events matching the range and filter, before paging. Free and
    /// exact — see the module docs.
    pub total_count: u64,
    /// Zero-based index of the first returned event within the whole result set.
    pub position: u64,
    /// How many calendar collections were queried to build this page.
    ///
    /// CalDAV has no cross-collection query, so an account-wide read costs one
    /// REPORT per calendar. This is that number, reported rather than hidden —
    /// pass `calendar:` to bring it down to one.
    pub calendars_queried: u64,
}

/// Cursors are event ids — see the module docs for why.
pub type EventConnection = Connection<String, GqlEvent, EventConnectionFields>;

/// The cursor for an event. An expanded series repeats its UID across every
/// occurrence, so the `RECURRENCE-ID` is what makes one addressable.
pub fn event_cursor(event: &Event) -> String {
    match &event.recurrence_id {
        Some(rid) => format!("{}@{rid}", event.id),
        None => event.id.clone(),
    }
}

/// Where a page sits in a list, with its items already paired with cursors.
#[derive(Debug)]
pub struct Page<T> {
    pub items: Vec<(String, T)>,
    pub start: usize,
    pub total: usize,
    pub has_previous: bool,
    pub has_next: bool,
}

/// Work out which slice of a list a page refers to.
pub fn page_of<T>(
    items: Vec<T>,
    args: PageArgs,
    cursor_of: impl Fn(&T) -> String,
) -> Result<Page<T>> {
    if args.first.is_some() && args.last.is_some() {
        return Err(async_graphql::Error::new(
            "Pass `first` or `last`, not both.",
        ));
    }
    if args.after.is_some() && args.before.is_some() {
        return Err(async_graphql::Error::new(
            "Pass `after` or `before`, not both.",
        ));
    }

    let cursors: Vec<String> = items.iter().map(&cursor_of).collect();
    let total = items.len();

    let locate = |cursor: &str| {
        cursors.iter().position(|c| c == cursor).ok_or_else(|| {
            async_graphql::Error::new(format!(
                "Cursor {cursor:?} is no longer in this list — the event was removed, moved, \
                 or no longer matches, since the cursor was issued. Restart pagination \
                 without `after`/`before`."
            ))
        })
    };

    let mut start = match &args.after {
        Some(cursor) => locate(cursor)? + 1,
        None => 0,
    };
    let mut end = match &args.before {
        Some(cursor) => locate(cursor)?,
        None => total,
    };
    end = end.max(start);

    match (args.first, args.last) {
        (Some(first), _) => end = end.min(start + clamp_page(Some(first.max(0) as u32)) as usize),
        (_, Some(last)) => {
            start = end.saturating_sub(clamp_page(Some(last.max(0) as u32)) as usize)
        }
        _ => end = end.min(start + clamp_page(None) as usize),
    }

    Ok(Page {
        items: items
            .into_iter()
            .zip(cursors)
            .skip(start)
            .take(end - start)
            .map(|(node, cursor)| (cursor, node))
            .collect(),
        start,
        total,
        has_previous: start > 0,
        has_next: end < total,
    })
}

/// Paginate a list already in hand.
pub fn paginate<T: OutputType>(
    items: Vec<T>,
    args: PageArgs,
    cursor_of: impl Fn(&T) -> String,
) -> Result<ListConnection<T>> {
    let page = page_of(items, args, cursor_of)?;
    let mut connection = Connection::with_additional_fields(
        page.has_previous,
        page.has_next,
        CountFields {
            total_count: page.total as u64,
        },
    );
    connection.edges.extend(
        page.items
            .into_iter()
            .map(|(cursor, node)| Edge::new(cursor, node)),
    );
    Ok(connection)
}

/// Turn a list of events into a connection. Used by every event-shaped field, so
/// `occurrences` and `conflicts` page exactly like the top-level `events` does.
pub fn events_connection_of(
    mut events: Vec<Event>,
    args: PageArgs,
    filter: Option<&EventFilter>,
    sort: Option<&[EventSort]>,
    calendars_queried: u64,
) -> Result<EventConnection> {
    if let Some(filter) = filter {
        events.retain(|e| filter.matches(e));
    }
    sort_events(&mut events, sort);

    let page = page_of(events, args, event_cursor)?;
    let mut connection = Connection::with_additional_fields(
        page.has_previous,
        page.has_next,
        EventConnectionFields {
            total_count: page.total as u64,
            position: page.start as u64,
            calendars_queried,
        },
    );
    connection.edges.extend(
        page.items
            .into_iter()
            .map(|(cursor, event)| Edge::new(cursor, GqlEvent::from(event))),
    );
    Ok(connection)
}

/// Everything a range read needs beyond its paging arguments.
pub struct EventQuery {
    /// Calendar name or id. `None` spans every collection holding events.
    pub calendar: Option<String>,
    pub range: RangeArgs,
    /// Expand recurring series into one event per occurrence.
    pub expand: bool,
    pub filter: Option<EventFilter>,
    pub sort: Option<Vec<EventSort>>,
}

/// Fetch a time range and build the connection.
///
/// One `load_many` puts every calendar's window into a single loader batch, so
/// the REPORTs go out together instead of one after another, and a window
/// another part of the query already asked for costs nothing.
pub async fn events_connection(
    ctx: &Context<'_>,
    args: PageArgs,
    query: EventQuery,
) -> Result<EventConnection> {
    let calendars = all_calendars(ctx).await?;
    let targets: Vec<Calendar> = match &query.calendar {
        Some(name) => vec![resolve_calendar(&calendars, Some(name))?],
        None => calendars
            .iter()
            .filter(|c| c.supports_events)
            .cloned()
            .collect(),
    };

    // Nothing below this connection needs an event, so don't fetch any. The
    // count is free once the events are here, but the events themselves are the
    // expensive part, and `pageInfo` alone is not worth a REPORT per calendar.
    let wants_events = ["nodes", "edges", "totalCount"]
        .iter()
        .any(|field| ctx.look_ahead().field(field).exists());
    if !wants_events {
        return events_connection_of(Vec::new(), args, None, None, targets.len() as u64);
    }

    let (start, end) = query.range.resolve()?;
    let windows: Vec<Window> = targets
        .iter()
        .map(|c| Window {
            calendar_id: c.id.clone(),
            start,
            end,
            expand: query.expand,
        })
        .collect();

    let loaded = ctx
        .data::<EventWindows>()?
        .load_many(windows.clone())
        .await
        .map_err(to_gql_error)?;

    // Flattened in calendar order rather than however the futures settled, so
    // repeating a query gives the same page.
    let events: Vec<Event> = windows
        .iter()
        .filter_map(|w| loaded.get(w))
        .flat_map(|events| events.iter().cloned())
        .collect();

    events_connection_of(
        events,
        args,
        query.filter.as_ref(),
        query.sort.as_deref(),
        targets.len() as u64,
    )
}

/// Cost of a connection field: the page size asked for, times the cost of a node.
pub fn page_complexity(first: Option<i32>, last: Option<i32>, child_complexity: usize) -> usize {
    let requested = first.or(last).map(|n| n.max(0) as u32);
    clamp_page(requested) as usize * child_complexity
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(items: &[&str], args: PageArgs) -> Result<Page<String>> {
        page_of(
            items.iter().map(|s| s.to_string()).collect(),
            args,
            Clone::clone,
        )
    }

    fn args(after: Option<&str>, first: Option<i32>) -> PageArgs {
        PageArgs {
            after: after.map(str::to_string),
            before: None,
            first,
            last: None,
        }
    }

    #[test]
    fn a_cursor_resumes_after_the_item_it_names() {
        let p = page(&["a", "b", "c", "d"], args(Some("b"), Some(2))).unwrap();
        let got: Vec<_> = p.items.iter().map(|(_, v)| v.as_str()).collect();
        assert_eq!(got, ["c", "d"]);
        assert_eq!(p.total, 4);
        assert!(p.has_previous);
        assert!(!p.has_next);
    }

    #[test]
    fn a_stale_cursor_says_how_to_recover() {
        let err = page(&["a", "b"], args(Some("gone"), None)).unwrap_err();
        assert!(err.message.contains("Restart pagination"), "{err:?}");
    }

    #[test]
    fn first_and_last_together_are_rejected() {
        let err = page(
            &["a"],
            PageArgs {
                after: None,
                before: None,
                first: Some(1),
                last: Some(1),
            },
        )
        .unwrap_err();
        assert!(err.message.contains("not both"));
    }

    #[test]
    fn last_takes_from_the_end() {
        let p = page(
            &["a", "b", "c"],
            PageArgs {
                after: None,
                before: None,
                first: None,
                last: Some(2),
            },
        )
        .unwrap();
        let got: Vec<_> = p.items.iter().map(|(_, v)| v.as_str()).collect();
        assert_eq!(got, ["b", "c"]);
        assert!(p.has_previous);
    }

    #[test]
    fn page_size_is_capped() {
        let items: Vec<String> = (0..500).map(|i| i.to_string()).collect();
        let p = page_of(
            items,
            PageArgs {
                after: None,
                before: None,
                first: Some(9999),
                last: None,
            },
            Clone::clone,
        )
        .unwrap();
        assert_eq!(p.items.len(), super::super::types::MAX_PAGE as usize);
        assert!(p.has_next);
    }

    #[test]
    fn occurrences_of_one_series_get_distinct_cursors() {
        let mut master = crate::models::Event {
            id: "series-1".into(),
            calendar: "Home".into(),
            calendar_href: "/c/".into(),
            href: "/c/e.ics".into(),
            resource_url: "https://dav.test/c/e.ics".into(),
            etag: None,
            summary: None,
            description: None,
            location: None,
            url: None,
            status: None,
            start: Default::default(),
            end: Default::default(),
            all_day: false,
            recurrence: None,
            recur_source: None,
            recurrence_id: None,
            organizer: None,
            attendees: vec![],
            categories: vec![],
            created: None,
            last_modified: None,
            sequence: 0,
        };
        assert_eq!(event_cursor(&master), "series-1");
        master.recurrence_id = Some("20260725T090000Z".into());
        assert_eq!(event_cursor(&master), "series-1@20260725T090000Z");
    }
}
