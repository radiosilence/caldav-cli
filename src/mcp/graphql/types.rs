//! GraphQL type wrappers around the domain models, plus the preview/confirm
//! nonce store that guards every write.
//!
//! Calendars and events are `#[Object]`s rather than plain structs because most
//! of what makes them interesting is an edge, and an edge should cost nothing
//! until it is selected. `Event.calendar` resolves through the calendar loader,
//! `Event.series` through a `calendar-multiget`, `Calendar.events` through a
//! deduplicated range query — and a query that asks for none of them issues none
//! of those requests.

use async_graphql::{Context, Enum, Object, Result, SimpleObject};
use chrono::{DateTime, Duration, Utc};

use super::connection::{
    EventConnection, EventQuery, PageArgs, event_cursor, events_connection, events_connection_of,
    page_complexity,
};
use super::filter::{EventFilter, EventSort};
use super::loaders::{Calendars, EventResources, Resource, to_gql_error};
use crate::caldav::recur;
use crate::commands::RangeArgs;
use crate::models::{Attendee, BusyPeriod, Calendar, Event, EventTime};
use crate::util;

/// Page size used when a connection names no `first`/`last`.
pub(crate) const DEFAULT_PAGE: u32 = 25;
/// Hard cap on a single page, and the figure nested lists are costed at.
pub(crate) const MAX_PAGE: u32 = 100;

pub(crate) fn clamp_page(size: Option<u32>) -> u32 {
    size.unwrap_or(DEFAULT_PAGE).min(MAX_PAGE)
}

/// The account's calendars, through the loader. Every calendar question is
/// answered by filtering this one list, so a query touching calendars at ten
/// different points still costs a single `PROPFIND`.
pub(crate) async fn all_calendars(ctx: &Context<'_>) -> Result<std::sync::Arc<Vec<Calendar>>> {
    ctx.data::<std::sync::Arc<Calendars>>()?
        .load_one(())
        .await
        .map_err(to_gql_error)?
        .ok_or_else(|| async_graphql::Error::new("Calendar listing unavailable"))
}

// ============ Output Types ============

/// A calendar collection. Navigate into it with `events { ... }`.
pub struct GqlCalendar(pub Calendar);

impl From<Calendar> for GqlCalendar {
    fn from(c: Calendar) -> Self {
        Self(c)
    }
}

#[Object(name = "Calendar")]
#[allow(clippy::too_many_arguments)]
impl GqlCalendar {
    /// Short id — pass this (or the name) wherever a calendar is accepted.
    async fn id(&self) -> &str {
        &self.0.id
    }
    /// Server path to the collection.
    async fn href(&self) -> &str {
        &self.0.href
    }
    /// Absolute URL of the collection. Not always `server + href`: iCloud shards
    /// accounts onto partition hosts, so a calendar found via `caldav.icloud.com`
    /// may actually live on `pNN-caldav.icloud.com`.
    async fn url(&self) -> &str {
        &self.0.url
    }
    async fn name(&self) -> &str {
        &self.0.name
    }
    async fn description(&self) -> Option<&str> {
        self.0.description.as_deref()
    }
    /// Hex colour as set in the calendar app (`#RRGGBBAA` on Apple servers).
    async fn color(&self) -> Option<&str> {
        self.0.color.as_deref()
    }
    /// True when this account cannot write to the calendar.
    async fn read_only(&self) -> bool {
        self.0.read_only
    }
    /// False for task-only collections, which hold no events.
    async fn supports_events(&self) -> bool {
        self.0.supports_events
    }
    /// The account's own default calendar, per the server. New events land here
    /// unless the user picked another one or `calendar` is given.
    async fn is_default(&self) -> bool {
        self.0.is_default
    }

    /// Events in this calendar over a time window, defaulting to the next 7
    /// days.
    ///
    /// Takes the same `filter` and `sort` as the top-level `events` query, fixed
    /// to this calendar. Asking several calendars for the same window costs one
    /// REPORT each, issued together; asking twice for the same one costs nothing
    /// the second time.
    #[graphql(complexity = "page_complexity(first, last, child_complexity)")]
    async fn events(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Window start: ISO 8601, YYYY-MM-DD, 'today', 'tomorrow', or '+2d'")]
        start: Option<String>,
        #[graphql(desc = "Window end. Takes precedence over `days`.")] end: Option<String>,
        #[graphql(desc = "Days from `start`. Default 7.")] days: Option<i64>,
        #[graphql(desc = "IANA timezone for naive dates, e.g. Europe/London")] tz: Option<String>,
        #[graphql(
            desc = "Expand recurring series into one event per occurrence. Default true. False gives the master event carrying its RRULE, which is what you edit."
        )]
        expand: Option<bool>,
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
                calendar: Some(self.0.id.clone()),
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
}

#[derive(SimpleObject)]
#[graphql(name = "EventTime")]
pub struct GqlEventTime {
    /// RFC 3339 UTC timestamp. Null for all-day times — use `date` instead.
    pub date_time: Option<String>,
    /// `YYYY-MM-DD`, set only for all-day times.
    pub date: Option<String>,
    /// IANA timezone the event was authored in, when the server sent one.
    pub tzid: Option<String>,
    pub all_day: bool,
}

impl From<EventTime> for GqlEventTime {
    fn from(t: EventTime) -> Self {
        Self {
            date_time: t.date_time,
            date: t.date,
            tzid: t.tzid,
            all_day: t.all_day,
        }
    }
}

#[derive(SimpleObject)]
#[graphql(name = "Attendee")]
pub struct GqlAttendee {
    pub email: String,
    pub name: Option<String>,
    /// e.g. `REQ-PARTICIPANT`, `OPT-PARTICIPANT`, `CHAIR`.
    pub role: Option<String>,
    /// e.g. `ACCEPTED`, `DECLINED`, `TENTATIVE`, `NEEDS-ACTION`.
    pub status: Option<String>,
}

impl From<Attendee> for GqlAttendee {
    fn from(a: Attendee) -> Self {
        Self {
            email: a.email,
            name: a.name,
            role: a.role,
            status: a.status,
        }
    }
}

/// An event. Everything below `calendar`, `series`, `occurrences` and
/// `conflicts` is lazy — selecting none of them issues no further requests.
pub struct GqlEvent(pub Event);

impl From<Event> for GqlEvent {
    fn from(e: Event) -> Self {
        Self(e)
    }
}

/// The window `conflicts` searches: whole UTC days covering the event.
///
/// Snapping to day boundaries is what makes the field affordable. Every event on
/// a given day resolves to the same window, so a page of them shares one fetch
/// per calendar through the loader rather than each opening its own.
fn conflict_window(event: &Event) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let start = event.start.instant?;
    let end = event.end.instant.unwrap_or(start).max(start);
    let floor = start.date_naive().and_hms_opt(0, 0, 0)?.and_utc();
    let ceil = end
        .date_naive()
        .succ_opt()?
        .and_hms_opt(0, 0, 0)?
        .and_utc()
        .max(floor + Duration::days(1));
    Some((floor, ceil))
}

/// Do two events share any time? Half-open, so back-to-back meetings don't clash.
fn overlaps(a: &Event, b: &Event) -> bool {
    let (Some(a_start), Some(b_start)) = (a.start.instant, b.start.instant) else {
        return false;
    };
    let a_end = a.end.instant.unwrap_or(a_start).max(a_start);
    let b_end = b.end.instant.unwrap_or(b_start).max(b_start);
    // A zero-length event still collides with anything containing its instant.
    a_start < b_end && b_start < a_end || (a_start == b_start)
}

#[Object(name = "Event")]
#[allow(clippy::too_many_arguments)]
impl GqlEvent {
    /// The iCalendar UID — pass this to `event`, `updateEvent`, `deleteEvent`.
    async fn id(&self) -> &str {
        &self.0.id
    }
    /// Display name of the calendar holding the event. Free — it arrives with
    /// the event. Select `calendar { ... }` for the collection itself.
    async fn calendar_name(&self) -> &str {
        &self.0.calendar
    }
    /// Path to the `.ics` resource. Two occurrences of one series share it.
    async fn href(&self) -> &str {
        &self.0.href
    }
    /// The server's entity tag for the resource, bumped on every change.
    async fn etag(&self) -> Option<&str> {
        self.0.etag.as_deref()
    }
    /// iCalendar `SEQUENCE`, incremented on each published revision.
    async fn sequence(&self) -> u32 {
        self.0.sequence
    }
    async fn summary(&self) -> Option<&str> {
        self.0.summary.as_deref()
    }
    async fn description(&self) -> Option<&str> {
        self.0.description.as_deref()
    }
    async fn location(&self) -> Option<&str> {
        self.0.location.as_deref()
    }
    async fn url(&self) -> Option<&str> {
        self.0.url.as_deref()
    }
    /// `CONFIRMED`, `TENTATIVE`, or `CANCELLED`.
    async fn status(&self) -> Option<&str> {
        self.0.status.as_deref()
    }
    async fn start(&self) -> GqlEventTime {
        self.0.start.clone().into()
    }
    async fn end(&self) -> GqlEventTime {
        self.0.end.clone().into()
    }
    async fn all_day(&self) -> bool {
        self.0.all_day
    }
    /// Length in whole minutes. Null when either end couldn't be anchored to an
    /// instant.
    async fn duration_minutes(&self) -> Option<i64> {
        let start = self.0.start.instant?;
        let end = self.0.end.instant?;
        Some((end - start).num_minutes().max(0))
    }
    /// Recurrence rule of the series, e.g. `FREQ=WEEKLY;BYDAY=MO`.
    async fn recurrence(&self) -> Option<&str> {
        self.0.recurrence.as_deref()
    }
    /// Set on one occurrence of a recurring series. Two results can share an
    /// `id` and differ only here.
    async fn recurrence_id(&self) -> Option<&str> {
        self.0.recurrence_id.as_deref()
    }
    /// True when this event belongs to a repeating series.
    async fn is_recurring(&self) -> bool {
        self.0.recurrence.is_some() || self.0.recurrence_id.is_some()
    }
    async fn organizer(&self) -> Option<GqlAttendee> {
        self.0.organizer.clone().map(Into::into)
    }
    /// Participants. A plain list: they belong to the event, arrive with it, and
    /// wrapping them in a connection would be ceremony at every call site.
    async fn attendees(&self) -> Vec<GqlAttendee> {
        self.0.attendees.iter().cloned().map(Into::into).collect()
    }
    async fn categories(&self) -> &[String] {
        &self.0.categories
    }
    async fn created(&self) -> Option<&str> {
        self.0.created.as_deref()
    }
    async fn last_modified(&self) -> Option<&str> {
        self.0.last_modified.as_deref()
    }

    /// The calendar this event lives in.
    ///
    /// Resolved through the calendar loader, so a whole page of events asking
    /// for it shares the one `PROPFIND` the query already paid for — and can
    /// walk on into `calendar { events { ... } }` from there.
    async fn calendar(&self, ctx: &Context<'_>) -> Result<Option<GqlCalendar>> {
        Ok(self.collection(ctx).await?.map(GqlCalendar::from))
    }

    /// The master event of a recurring series — the one carrying the `RRULE`,
    /// and the one you edit to change every occurrence.
    ///
    /// Null when this event already is the master. Occurrences share a resource
    /// with their master, so a page of them collapses into a single
    /// `calendar-multiget` per calendar.
    #[graphql(complexity = "5 + child_complexity")]
    async fn series(&self, ctx: &Context<'_>) -> Result<Option<GqlEvent>> {
        if self.0.recurrence_id.is_none() {
            return Ok(None);
        }
        let Some(calendar) = self.collection(ctx).await? else {
            return Ok(None);
        };
        Ok(ctx
            .data::<EventResources>()?
            .load_one(Resource {
                calendar_id: calendar.id,
                href: self.0.href.clone(),
            })
            .await
            .map_err(to_gql_error)?
            .map(GqlEvent::from))
    }

    /// Occurrences of this event's recurrence rule within a window.
    ///
    /// Computed from the rule already in hand, so it costs no request at all —
    /// `calendarsQueried` is 0. Empty for a one-off. Use this to see where a
    /// series actually falls without expanding every calendar.
    #[graphql(complexity = "page_complexity(first, last, child_complexity)")]
    async fn occurrences(
        &self,
        #[graphql(desc = "Window start. Default: today.")] start: Option<String>,
        #[graphql(desc = "Window end. Takes precedence over `days`.")] end: Option<String>,
        #[graphql(desc = "Days from `start`. Default 7 — widen to look further ahead.")]
        days: Option<i64>,
        #[graphql(desc = "IANA timezone for naive dates")] tz: Option<String>,
        filter: Option<EventFilter>,
        sort: Option<Vec<EventSort>>,
        after: Option<String>,
        before: Option<String>,
        first: Option<i32>,
        last: Option<i32>,
    ) -> Result<EventConnection> {
        let (from, to) = RangeArgs {
            start,
            end,
            days,
            tz,
        }
        .resolve()?;
        let events = match self.0.recurrence {
            Some(_) => recur::expand_all(vec![self.0.clone()], from, to),
            None => Vec::new(),
        };
        events_connection_of(
            events,
            PageArgs {
                after,
                before,
                first,
                last,
            },
            filter.as_ref(),
            sort.as_deref(),
            0,
        )
    }

    /// Other events overlapping this one — what you'd clash with.
    ///
    /// Searches the whole UTC days this event covers, across every calendar
    /// unless one is named, and excludes the event itself. The window is snapped
    /// to day boundaries deliberately: every event on a given day resolves the
    /// same window, so a page of them shares one fetch per calendar instead of
    /// each issuing its own.
    #[graphql(complexity = "page_complexity(first, last, child_complexity)")]
    async fn conflicts(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Calendar name or id. Omit to check every calendar.")] calendar: Option<
            String,
        >,
        filter: Option<EventFilter>,
        sort: Option<Vec<EventSort>>,
        after: Option<String>,
        before: Option<String>,
        first: Option<i32>,
        last: Option<i32>,
    ) -> Result<EventConnection> {
        let args = PageArgs {
            after,
            before,
            first,
            last,
        };
        // An event we couldn't anchor to an instant has no window to search.
        let Some((from, to)) = conflict_window(&self.0) else {
            return events_connection_of(Vec::new(), args, None, None, 0);
        };

        let day = events_connection(
            ctx,
            PageArgs {
                after: None,
                before: None,
                // The candidates get filtered down to actual overlaps below, so
                // the day has to arrive whole rather than a page at a time.
                first: Some(MAX_PAGE as i32),
                last: None,
            },
            EventQuery {
                calendar,
                range: RangeArgs {
                    start: Some(util::format_rfc3339(from)),
                    end: Some(util::format_rfc3339(to)),
                    days: None,
                    tz: None,
                },
                expand: true,
                filter: None,
                sort: None,
            },
        )
        .await?;

        let calendars_queried = day.additional_fields.calendars_queried;
        let me = event_cursor(&self.0);
        let clashing: Vec<Event> = day
            .edges
            .into_iter()
            .map(|edge| edge.node.0)
            .filter(|other| event_cursor(other) != me && overlaps(&self.0, other))
            .collect();

        events_connection_of(
            clashing,
            args,
            filter.as_ref(),
            sort.as_deref(),
            calendars_queried,
        )
    }
}

impl GqlEvent {
    /// The collection holding this event, matched by path against the loaded
    /// listing. `Event` carries the calendar's href rather than its id, and the
    /// href is what the server guarantees is stable.
    async fn collection(&self, ctx: &Context<'_>) -> Result<Option<Calendar>> {
        Ok(all_calendars(ctx)
            .await?
            .iter()
            .find(|c| c.href == self.0.calendar_href)
            .cloned())
    }
}

#[derive(SimpleObject)]
#[graphql(name = "BusyPeriod")]
pub struct GqlBusyPeriod {
    /// RFC 3339 UTC.
    pub start: String,
    /// RFC 3339 UTC.
    pub end: String,
    /// `BUSY`, `BUSY-TENTATIVE`, or `BUSY-UNAVAILABLE`.
    pub status: String,
}

impl From<BusyPeriod> for GqlBusyPeriod {
    fn from(p: BusyPeriod) -> Self {
        Self {
            start: p.start,
            end: p.end,
            status: p.status,
        }
    }
}

/// Two-step guard on the destructive writes — updates and deletes: PREVIEW
/// returns a human-readable summary and a one-shot token; CONFIRM performs the
/// change. Creates don't take one; they just happen.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum WriteAction {
    /// Describe what would happen and return a `confirmationToken`.
    Preview,
    /// Apply the change. Requires the token from a matching PREVIEW.
    Confirm,
}

#[derive(SimpleObject)]
#[graphql(name = "EventMutationResult")]
pub struct GqlEventResult {
    pub success: bool,
    /// The event as it now stands. Null on PREVIEW and on failure.
    pub event: Option<GqlEvent>,
    /// Human-readable description of the pending change. Set on PREVIEW.
    pub preview: Option<String>,
    /// One-shot token to pass back with CONFIRM. Set on PREVIEW.
    pub confirmation_token: Option<String>,
    pub error: Option<String>,
}

impl GqlEventResult {
    /// Named `pending` rather than `preview` because `SimpleObject`
    /// already generates a `preview` field accessor.
    pub fn pending(text: String, token: String) -> Self {
        Self {
            success: true,
            event: None,
            preview: Some(text),
            confirmation_token: Some(token),
            error: None,
        }
    }

    pub fn done(event: Event) -> Self {
        Self {
            success: true,
            event: Some(event.into()),
            preview: None,
            confirmation_token: None,
            error: None,
        }
    }

    pub fn failed(msg: impl Into<String>) -> Self {
        Self {
            success: false,
            event: None,
            preview: None,
            confirmation_token: None,
            error: Some(msg.into()),
        }
    }
}

// ============ Confirmation nonces ============

pub struct Nonce {
    fingerprint: String,
    issued_at: std::time::Instant,
}

/// Process-shared store of outstanding preview tokens. Schema-level rather
/// than request-level because a preview and its confirm are separate requests.
pub type NonceStore = tokio::sync::Mutex<std::collections::HashMap<String, Nonce>>;

/// Hard cap on outstanding nonces. A preview without a confirm is user intent —
/// capacity for hundreds of pending edits is plenty for a single session.
const NONCE_CAP: usize = 256;

/// How long a PREVIEW'd nonce stays valid. Long enough for a human to read and
/// approve, short enough to expire well before the process restarts.
const NONCE_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Drop entries older than TTL, then if still over cap drop the oldest.
fn evict(map: &mut std::collections::HashMap<String, Nonce>) {
    let now = std::time::Instant::now();
    map.retain(|_, n| now.duration_since(n.issued_at) < NONCE_TTL);
    while map.len() >= NONCE_CAP {
        let oldest = map
            .iter()
            .min_by_key(|(_, n)| n.issued_at)
            .map(|(k, _)| k.clone());
        match oldest {
            Some(k) => {
                map.remove(&k);
            }
            None => break,
        }
    }
}

/// Fingerprint the params so we can detect tampering between PREVIEW and
/// CONFIRM. Non-cryptographic — it only needs to catch accidental drift, not
/// defeat an attacker who already controls the process.
pub fn params_fingerprint(parts: &[&str]) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for part in parts {
        part.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// Issue a new one-shot confirmation nonce for the given params.
pub async fn issue_nonce(store: &NonceStore, parts: &[&str]) -> String {
    let nonce = uuid::Uuid::new_v4().to_string();
    let entry = Nonce {
        fingerprint: params_fingerprint(parts),
        issued_at: std::time::Instant::now(),
    };
    let mut map = store.lock().await;
    evict(&mut map);
    map.insert(nonce.clone(), entry);
    nonce
}

/// Consume a nonce, returning Ok(()) if it was issued for the given params.
/// The nonce is always removed on consumption, even on mismatch or expiry, so
/// a bad CONFIRM forces the caller back to PREVIEW.
pub async fn consume_nonce(
    store: &NonceStore,
    nonce: Option<&str>,
    parts: &[&str],
) -> std::result::Result<(), &'static str> {
    let nonce =
        nonce.ok_or("Missing confirmationToken. Use action=PREVIEW first to obtain one.")?;
    let entry = store
        .lock()
        .await
        .remove(nonce)
        .ok_or("Invalid or already-used confirmationToken. Re-run PREVIEW.")?;
    if std::time::Instant::now().duration_since(entry.issued_at) >= NONCE_TTL {
        return Err("confirmationToken expired. Re-run PREVIEW.");
    }
    if entry.fingerprint != params_fingerprint(parts) {
        return Err("Params changed between PREVIEW and CONFIRM. Re-run PREVIEW.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spanning(start_hour: u32, end_hour: u32) -> Event {
        let at = |h: u32| Utc.with_ymd_and_hms(2026, 7, 25, h, 0, 0).unwrap();
        use chrono::TimeZone;
        Event {
            id: format!("uid-{start_hour}"),
            calendar: "Home".into(),
            calendar_href: "/cal/home/".into(),
            href: "/cal/home/e.ics".into(),
            resource_url: "https://dav.test/cal/home/e.ics".into(),
            etag: None,
            summary: None,
            description: None,
            location: None,
            url: None,
            status: None,
            start: EventTime {
                instant: Some(at(start_hour)),
                ..Default::default()
            },
            end: EventTime {
                instant: Some(at(end_hour)),
                ..Default::default()
            },
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
        }
    }

    use chrono::TimeZone;

    #[test]
    fn back_to_back_events_do_not_clash() {
        assert!(!overlaps(&spanning(9, 10), &spanning(10, 11)));
    }

    #[test]
    fn straddling_events_clash() {
        assert!(overlaps(&spanning(9, 11), &spanning(10, 12)));
    }

    #[test]
    fn a_zero_length_event_clashes_with_what_contains_it() {
        assert!(overlaps(&spanning(10, 10), &spanning(9, 11)));
        assert!(overlaps(&spanning(9, 11), &spanning(10, 10)));
    }

    #[test]
    fn the_conflict_window_covers_whole_days() {
        let (from, to) = conflict_window(&spanning(9, 10)).unwrap();
        assert_eq!(from, Utc.with_ymd_and_hms(2026, 7, 25, 0, 0, 0).unwrap());
        assert_eq!(to, Utc.with_ymd_and_hms(2026, 7, 26, 0, 0, 0).unwrap());
    }

    #[test]
    fn events_on_one_day_share_a_conflict_window() {
        // The whole point: sibling events resolve the same loader key.
        assert_eq!(
            conflict_window(&spanning(9, 10)),
            conflict_window(&spanning(14, 15))
        );
    }

    #[tokio::test]
    async fn nonce_round_trips_for_matching_params() {
        let store = NonceStore::default();
        let params = ["Standup", "2026-07-24T09:00:00Z"];
        let nonce = issue_nonce(&store, &params).await;
        assert!(consume_nonce(&store, Some(&nonce), &params).await.is_ok());
    }

    #[tokio::test]
    async fn nonce_is_single_use() {
        let store = NonceStore::default();
        let params = ["Standup"];
        let nonce = issue_nonce(&store, &params).await;
        assert!(consume_nonce(&store, Some(&nonce), &params).await.is_ok());
        assert!(consume_nonce(&store, Some(&nonce), &params).await.is_err());
    }

    #[tokio::test]
    async fn changed_params_are_rejected() {
        let store = NonceStore::default();
        let nonce = issue_nonce(&store, &["Standup", "09:00"]).await;
        let err = consume_nonce(&store, Some(&nonce), &["Standup", "17:00"])
            .await
            .unwrap_err();
        assert!(err.contains("Params changed"));
    }

    #[tokio::test]
    async fn missing_and_unknown_tokens_are_rejected() {
        let store = NonceStore::default();
        assert!(consume_nonce(&store, None, &["x"]).await.is_err());
        assert!(
            consume_nonce(&store, Some("made-up"), &["x"])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn store_stays_bounded() {
        let store = NonceStore::default();
        for i in 0..(NONCE_CAP + 50) {
            issue_nonce(&store, &[&i.to_string()]).await;
        }
        assert!(store.lock().await.len() <= NONCE_CAP);
    }

    #[test]
    fn fingerprint_is_order_sensitive() {
        assert_ne!(
            params_fingerprint(&["a", "b"]),
            params_fingerprint(&["b", "a"])
        );
    }
}
