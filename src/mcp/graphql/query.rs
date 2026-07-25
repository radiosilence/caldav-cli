//! GraphQL query resolvers

use async_graphql::{Context, Object, Result};

use super::DefaultCalendar;
use super::connection::{
    EventConnection, EventQuery, ListConnection, PageArgs, events_connection, page_complexity,
    paginate,
};
use super::filter::{EventFilter, EventSort};
use super::loaders::{EventUids, Uid, to_gql_error};
use super::types::*;
use crate::caldav::resolve_calendar;
use crate::commands::RangeArgs;

pub struct QueryRoot;

#[Object]
#[allow(clippy::too_many_arguments)]
impl QueryRoot {
    /// Every calendar on the account. Start here to discover calendar ids, then
    /// walk into `events { ... }` without a second round trip for the listing.
    #[graphql(complexity = "page_complexity(first, last, child_complexity)")]
    async fn calendars(
        &self,
        ctx: &Context<'_>,
        after: Option<String>,
        before: Option<String>,
        first: Option<i32>,
        last: Option<i32>,
    ) -> Result<ListConnection<GqlCalendar>> {
        let calendars = all_calendars(ctx).await?;
        paginate(
            calendars.iter().cloned().map(GqlCalendar::from).collect(),
            PageArgs {
                after,
                before,
                first,
                last,
            },
            |c| c.0.id.clone(),
        )
    }

    /// One calendar by id, display name, or path. Omit `id` for where a new
    /// event lands if you don't say otherwise: the calendar chosen for this
    /// connection, else the account's own default.
    async fn calendar(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Calendar id, display name, or href. Omit for the default calendar.")]
        id: Option<String>,
    ) -> Result<GqlCalendar> {
        // The same fallback `createEvent` applies, so asking where an event
        // will go and then creating one can't disagree.
        let id = id.or_else(|| ctx.data_opt::<DefaultCalendar>().and_then(|d| d.0.clone()));
        let calendars = all_calendars(ctx).await?;
        Ok(GqlCalendar::from(resolve_calendar(
            &calendars,
            id.as_deref(),
        )?))
    }

    /// Events in a time window, across every calendar unless one is named.
    ///
    /// Defaults to the next 7 days. The window is what goes on the wire — it is
    /// the one thing CalDAV servers agree how to filter on — and `filter` then
    /// narrows the results here, where `and`/`or`/`not` nest freely.
    ///
    /// Cursors are event ids, so `after: "<last id you saw>"` resumes after that
    /// event. `totalCount` is free and exact.
    #[graphql(complexity = "page_complexity(first, last, child_complexity)")]
    async fn events(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Calendar name or id. Omit to span every calendar.")] calendar: Option<
            String,
        >,
        #[graphql(desc = "Window start: ISO 8601, YYYY-MM-DD, 'today', 'tomorrow', or '+2d'")]
        start: Option<String>,
        #[graphql(desc = "Window end. Takes precedence over `days`.")] end: Option<String>,
        #[graphql(desc = "Days from `start`. Default 7.")] days: Option<i64>,
        #[graphql(desc = "IANA timezone for naive dates, e.g. Europe/London")] tz: Option<String>,
        #[graphql(
            desc = "Expand recurring series into one event per occurrence. Default true. False gives the master event carrying its RRULE, which is what you edit."
        )]
        expand: Option<bool>,
        #[graphql(desc = "Which events to keep. Omit to keep everything in the window.")]
        filter: Option<EventFilter>,
        #[graphql(desc = "Ordering, most significant first. Defaults to earliest start.")]
        sort: Option<Vec<EventSort>>,
        after: Option<String>,
        before: Option<String>,
        first: Option<i32>,
        last: Option<i32>,
    ) -> Result<EventConnection> {
        events_connection(
            ctx,
            PageArgs {
                after,
                before,
                first,
                last,
            },
            EventQuery {
                calendar,
                range: RangeArgs {
                    start,
                    end,
                    days,
                    tz,
                },
                expand: expand.unwrap_or(true),
                filter,
                sort,
            },
        )
        .await
    }

    /// Everything happening today, or over the next `days` days.
    ///
    /// `events` with the window pinned to today — the shape an agenda wants.
    #[graphql(complexity = "page_complexity(first, last, child_complexity)")]
    async fn agenda(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Days to cover, starting today. Default 1.")] days: Option<i64>,
        #[graphql(desc = "Calendar name or id. Omit to span every calendar.")] calendar: Option<
            String,
        >,
        #[graphql(desc = "IANA timezone, e.g. Europe/London. Decides where 'today' starts.")]
        tz: Option<String>,
        filter: Option<EventFilter>,
        sort: Option<Vec<EventSort>>,
        after: Option<String>,
        before: Option<String>,
        first: Option<i32>,
        last: Option<i32>,
    ) -> Result<EventConnection> {
        events_connection(
            ctx,
            PageArgs {
                after,
                before,
                first,
                last,
            },
            EventQuery {
                calendar,
                range: RangeArgs {
                    start: None,
                    end: None,
                    days: Some(days.unwrap_or(1)),
                    tz,
                },
                expand: true,
                filter,
                sort,
            },
        )
        .await
    }

    /// One event by its UID.
    ///
    /// Unlike the range queries this needs no window — it asks the server for
    /// the UID directly. CalDAV filters have no OR, so without `calendar` this
    /// is one request per calendar until it hits; naming the calendar makes it
    /// exactly one.
    async fn event(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The event UID (the `id` from events/searchEvents)")] id: String,
        #[graphql(desc = "Restrict the lookup to one calendar — one request instead of a sweep.")]
        calendar: Option<String>,
    ) -> Result<Option<GqlEvent>> {
        let calendar_id = match calendar {
            Some(name) => Some(resolve_calendar(&all_calendars(ctx).await?, Some(&name))?.id),
            None => None,
        };
        Ok(ctx
            .data::<EventUids>()?
            .load_one(Uid {
                uid: id,
                calendar_id,
            })
            .await
            .map_err(to_gql_error)?
            .map(GqlEvent::from))
    }

    /// Search events by text across title, notes, location, categories, and
    /// attendees, within a time window.
    ///
    /// Superseded by `events(filter: { text: ... })`, which composes with
    /// `and`/`or`/`not` and narrows on any field rather than one blanket sweep.
    /// Kept as a shim: `query` maps onto a single `EventFilter.text`.
    #[graphql(
        deprecation = "Use `events(filter: { text: \"...\" })` — it composes with and/or/not and narrows per field.",
        complexity = "page_complexity(first, last, child_complexity)"
    )]
    async fn search_events(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Text to match (case-insensitive substring)")] query: String,
        #[graphql(desc = "Calendar name or id. Omit to span every calendar.")] calendar: Option<
            String,
        >,
        #[graphql(desc = "Window start. Default: today.")] start: Option<String>,
        #[graphql(desc = "Window end. Takes precedence over `days`.")] end: Option<String>,
        #[graphql(desc = "Days from `start`. Default 7 — widen this to search further ahead.")]
        days: Option<i64>,
        #[graphql(desc = "IANA timezone for naive dates")] tz: Option<String>,
        after: Option<String>,
        before: Option<String>,
        first: Option<i32>,
        last: Option<i32>,
    ) -> Result<EventConnection> {
        events_connection(
            ctx,
            PageArgs {
                after,
                before,
                first,
                last,
            },
            EventQuery {
                calendar,
                range: RangeArgs {
                    start,
                    end,
                    days,
                    tz,
                },
                expand: true,
                filter: Some(EventFilter {
                    text: Some(query),
                    ..Default::default()
                }),
                sort: None,
            },
        )
        .await
    }

    /// Busy windows over a range — consult this before proposing a meeting time.
    ///
    /// Tries the server's free-busy REPORT and falls back to deriving the
    /// windows from the events themselves, which is what iCloud needs.
    #[graphql(complexity = "page_complexity(first, last, child_complexity)")]
    async fn free_busy(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Window start. Default: today.")] start: Option<String>,
        #[graphql(desc = "Window end. Takes precedence over `days`.")] end: Option<String>,
        #[graphql(desc = "Days from `start`. Default 7.")] days: Option<i64>,
        #[graphql(desc = "IANA timezone for naive dates")] tz: Option<String>,
        after: Option<String>,
        before: Option<String>,
        first: Option<i32>,
        last: Option<i32>,
    ) -> Result<ListConnection<GqlBusyPeriod>> {
        let (from, to) = RangeArgs {
            start,
            end,
            days,
            tz,
        }
        .resolve()?;
        let client = ctx.data::<super::SharedClient>()?;
        let periods = client.free_busy(from, to).await?;
        paginate(
            periods.into_iter().map(GqlBusyPeriod::from).collect(),
            PageArgs {
                after,
                before,
                first,
                last,
            },
            |p| format!("{}/{}", p.start, p.end),
        )
    }
}
