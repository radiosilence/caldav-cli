//! GraphQL query resolvers

use async_graphql::{Context, Object, Result};

use crate::caldav::MAX_EVENTS;
use crate::commands::RangeArgs;

use super::SharedClient;
use super::types::*;

pub struct QueryRoot;

/// Collect the window flags every read shares into a [`RangeArgs`].
fn range(
    start: Option<String>,
    end: Option<String>,
    days: Option<i64>,
    tz: Option<String>,
) -> RangeArgs {
    RangeArgs {
        start,
        end,
        days,
        tz,
    }
}

#[Object]
#[allow(clippy::too_many_arguments)]
impl QueryRoot {
    /// List all calendars on the account. Start here to discover calendar ids.
    async fn calendars(&self, ctx: &Context<'_>) -> Result<Vec<GqlCalendar>> {
        let client = ctx.data::<SharedClient>()?;
        let calendars = client.list_calendars().await?;
        Ok(calendars.iter().cloned().map(GqlCalendar::from).collect())
    }

    /// List events in a time window. Defaults to the next 7 days from today.
    async fn events(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Calendar name or id. Omit to search every calendar.")] calendar: Option<
            String,
        >,
        #[graphql(desc = "Window start: ISO 8601, YYYY-MM-DD, 'today', 'tomorrow', or '+2d'")]
        start: Option<String>,
        #[graphql(desc = "Window end. Takes precedence over `days`.")] end: Option<String>,
        #[graphql(desc = "Days from `start`. Default 7.")] days: Option<i64>,
        #[graphql(desc = "IANA timezone for naive dates, e.g. Europe/London")] tz: Option<String>,
        #[graphql(desc = "Maximum events (default 100, max 500)")] limit: Option<u32>,
        #[graphql(
            desc = "Expand recurring series into one result per occurrence. Default true. Set false to get the master event with its RRULE, which is what you edit."
        )]
        expand: Option<bool>,
    ) -> Result<Vec<GqlEvent>> {
        let client = ctx.data::<SharedClient>()?;
        let (from, to) = range(start, end, days, tz).resolve()?;
        let limit = limit.unwrap_or(100).min(MAX_EVENTS as u32) as usize;
        let events = client
            .events_in_range(calendar.as_deref(), from, to, expand.unwrap_or(true), limit)
            .await?;
        Ok(events.into_iter().map(GqlEvent::from).collect())
    }

    /// Everything happening today (or the next `days` days).
    async fn agenda(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Days to cover, starting today. Default 1.")] days: Option<i64>,
        #[graphql(desc = "Calendar name or id. Omit for every calendar.")] calendar: Option<String>,
        #[graphql(desc = "IANA timezone, e.g. Europe/London. Decides where 'today' starts.")]
        tz: Option<String>,
        #[graphql(desc = "Maximum events (default 100, max 500)")] limit: Option<u32>,
    ) -> Result<Vec<GqlEvent>> {
        let client = ctx.data::<SharedClient>()?;
        let (from, to) = range(None, None, Some(days.unwrap_or(1)), tz).resolve()?;
        let limit = limit.unwrap_or(100).min(MAX_EVENTS as u32) as usize;
        let events = client
            .events_in_range(calendar.as_deref(), from, to, true, limit)
            .await?;
        Ok(events.into_iter().map(GqlEvent::from).collect())
    }

    /// Get a single event by its UID.
    async fn event(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The event UID (the `id` from events/searchEvents)")] id: String,
        #[graphql(desc = "Restrict the lookup to one calendar")] calendar: Option<String>,
    ) -> Result<Option<GqlEvent>> {
        let client = ctx.data::<SharedClient>()?;
        let event = client.get_event(&id, calendar.as_deref()).await?;
        Ok(event.map(GqlEvent::from))
    }

    /// Search events by text across title, notes, location, categories, and
    /// attendees. Searches the given window only — widen it to look further out.
    async fn search_events(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Text to match (case-insensitive substring)")] query: String,
        #[graphql(desc = "Calendar name or id. Omit for every calendar.")] calendar: Option<String>,
        #[graphql(desc = "Window start. Default: today.")] start: Option<String>,
        #[graphql(desc = "Window end. Takes precedence over `days`.")] end: Option<String>,
        #[graphql(desc = "Days from `start`. Default 7 — widen this to search further ahead.")]
        days: Option<i64>,
        #[graphql(desc = "IANA timezone for naive dates")] tz: Option<String>,
        #[graphql(desc = "Maximum results (default 50, max 500)")] limit: Option<u32>,
    ) -> Result<Vec<GqlEvent>> {
        let client = ctx.data::<SharedClient>()?;
        let (from, to) = range(start, end, days, tz).resolve()?;
        let limit = limit.unwrap_or(50).min(MAX_EVENTS as u32) as usize;
        let events = client
            .search_events(&query, calendar.as_deref(), from, to, limit)
            .await?;
        Ok(events.into_iter().map(GqlEvent::from).collect())
    }

    /// Busy windows over a range — consult this before proposing a meeting time.
    async fn free_busy(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Window start. Default: today.")] start: Option<String>,
        #[graphql(desc = "Window end. Takes precedence over `days`.")] end: Option<String>,
        #[graphql(desc = "Days from `start`. Default 7.")] days: Option<i64>,
        #[graphql(desc = "IANA timezone for naive dates")] tz: Option<String>,
    ) -> Result<Vec<GqlBusyPeriod>> {
        let client = ctx.data::<SharedClient>()?;
        let (from, to) = range(start, end, days, tz).resolve()?;
        let periods = client.free_busy(from, to).await?;
        Ok(periods.into_iter().map(GqlBusyPeriod::from).collect())
    }
}
