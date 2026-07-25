//! GraphQL mutation resolvers.
//!
//! Every write is two-phase: PREVIEW renders what would change and hands back a
//! one-shot token, CONFIRM applies it. A calendar is shared, visible state —
//! the model should never move or delete something without the user seeing the
//! change described first.

use async_graphql::{Context, Object, Result};

use crate::commands::parse_attendee_spec;
use crate::models::{Attendee, Event, EventFields};
use crate::util;

use super::types::*;
use super::{DefaultCalendar, SharedClient};

pub struct MutationRoot;

/// The write-shaped arguments, collected so preview, fingerprint, and apply all
/// read from the same place.
#[derive(Default, Clone)]
struct EventInput {
    calendar: Option<String>,
    summary: Option<String>,
    start: Option<String>,
    end: Option<String>,
    duration_minutes: Option<i64>,
    all_day: Option<bool>,
    tz: Option<String>,
    description: Option<String>,
    location: Option<String>,
    url: Option<String>,
    status: Option<String>,
    recurrence: Option<String>,
    /// `None` = leave alone. `Some([])` = clear.
    attendees: Option<Vec<String>>,
    /// `None` = leave alone. `Some([])` = clear.
    categories: Option<Vec<String>>,
}

impl EventInput {
    fn parsed_attendees(&self) -> Option<Vec<Attendee>> {
        self.attendees
            .as_ref()
            .map(|list| list.iter().map(|s| parse_attendee_spec(s)).collect())
    }

    fn fields<'a>(
        &'a self,
        attendees: Option<&'a [Attendee]>,
        categories: Option<&'a [String]>,
    ) -> EventFields<'a> {
        EventFields {
            summary: self.summary.as_deref(),
            description: self.description.as_deref(),
            location: self.location.as_deref(),
            url: self.url.as_deref(),
            status: self.status.as_deref(),
            start: self.start.as_deref(),
            end: self.end.as_deref(),
            duration_minutes: self.duration_minutes,
            all_day: self.all_day,
            tzid: self.tz.as_deref(),
            recurrence: self.recurrence.as_deref(),
            attendees,
            categories,
        }
    }

    /// Everything that affects the outcome, so a CONFIRM whose arguments drifted
    /// from its PREVIEW is rejected.
    fn fingerprint_parts(&self) -> Vec<String> {
        let list = |v: &Option<Vec<String>>| match v {
            None => "-".to_string(),
            Some(items) => items.join("\u{1f}"),
        };
        vec![
            self.calendar.clone().unwrap_or_default(),
            self.summary.clone().unwrap_or_default(),
            self.start.clone().unwrap_or_default(),
            self.end.clone().unwrap_or_default(),
            self.duration_minutes
                .map(|d| d.to_string())
                .unwrap_or_default(),
            self.all_day.map(|b| b.to_string()).unwrap_or_default(),
            self.tz.clone().unwrap_or_default(),
            self.description.clone().unwrap_or_default(),
            self.location.clone().unwrap_or_default(),
            self.url.clone().unwrap_or_default(),
            self.status.clone().unwrap_or_default(),
            self.recurrence.clone().unwrap_or_default(),
            list(&self.attendees),
            list(&self.categories),
        ]
    }
}

/// Render a time the user typed as the instant it resolves to, so the preview
/// shows what will actually be written rather than echoing the input back.
fn describe_time(raw: &str, tz: Option<&str>) -> String {
    match util::parse_datetime(raw, tz) {
        Ok(p) if p.date_only => format!("{} (all day)", util::format_iso_date(p.instant)),
        Ok(p) => match tz {
            Some(zone) => format!("{} [{zone}]", util::format_rfc3339(p.instant)),
            None => util::format_rfc3339(p.instant),
        },
        // Don't fail the preview on an unparseable value — the apply step will
        // report it properly. Showing the raw input is the honest thing here.
        Err(_) => format!("{raw} (unrecognised — will fail on CONFIRM)"),
    }
}

/// Human-readable before → after for an update, listing only what changes.
fn preview_update(existing: &Event, input: &EventInput) -> String {
    let mut lines = vec![format!(
        "Update event: {} ({})",
        existing.summary.as_deref().unwrap_or("(no title)"),
        existing.calendar
    )];
    lines.push(format!("Currently starts: {}", show_time(&existing.start)));

    let mut changes = Vec::new();
    let mut diff = |label: &str, from: Option<&str>, to: Option<&str>| {
        if let Some(to) = to.map(str::trim)
            && Some(to) != from.map(str::trim)
        {
            changes.push(format!(
                "  {label}: {} → {}",
                from.unwrap_or("(unset)"),
                if to.is_empty() { "(cleared)" } else { to }
            ));
        }
    };

    diff(
        "Title",
        existing.summary.as_deref(),
        input.summary.as_deref(),
    );
    diff(
        "Location",
        existing.location.as_deref(),
        input.location.as_deref(),
    );
    diff(
        "Notes",
        existing.description.as_deref(),
        input.description.as_deref(),
    );
    diff(
        "Status",
        existing.status.as_deref(),
        input.status.as_deref(),
    );
    diff(
        "Repeats",
        existing.recurrence.as_deref(),
        input.recurrence.as_deref(),
    );
    diff("Link", existing.url.as_deref(), input.url.as_deref());

    if let Some(start) = input.start.as_deref() {
        changes.push(format!(
            "  Start: {} → {}",
            show_time(&existing.start),
            describe_time(start, input.tz.as_deref())
        ));
    }
    if let Some(end) = input.end.as_deref() {
        changes.push(format!(
            "  End: {} → {}",
            show_time(&existing.end),
            describe_time(end, input.tz.as_deref())
        ));
    } else if let Some(mins) = input.duration_minutes {
        changes.push(format!("  Duration: {mins} minutes"));
    }
    if let Some(attendees) = &input.attendees {
        changes.push(format!(
            "  Invites: {} → {}",
            join_or_none(
                &existing
                    .attendees
                    .iter()
                    .map(|a| a.email.clone())
                    .collect::<Vec<_>>()
            ),
            join_or_none(attendees)
        ));
    }
    if let Some(categories) = &input.categories {
        changes.push(format!(
            "  Categories: {} → {}",
            join_or_none(&existing.categories),
            join_or_none(categories)
        ));
    }

    if changes.is_empty() {
        lines.push("No changes requested.".to_string());
    } else {
        lines.push("Changes:".to_string());
        lines.extend(changes);
    }
    lines.join("\n")
}

fn show_time(t: &crate::models::EventTime) -> String {
    t.date
        .clone()
        .or_else(|| t.date_time.clone())
        .unwrap_or_else(|| t.raw.clone())
}

fn join_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".to_string()
    } else {
        items.join(", ")
    }
}

#[Object]
#[allow(clippy::too_many_arguments)]
impl MutationRoot {
    /// Create an event. Writes immediately — no preview step. Tell the user
    /// what was created afterwards, including the calendar it landed in.
    async fn create_event(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Event title")] summary: String,
        #[graphql(desc = "Start: ISO 8601, 'YYYY-MM-DD [HH:MM]', 'tomorrow', or '+2h'")]
        start: String,
        #[graphql(desc = "Calendar name or id. Omit for the user's default calendar.")]
        calendar: Option<String>,
        #[graphql(desc = "End time. Takes precedence over durationMinutes.")] end: Option<String>,
        #[graphql(desc = "Length in minutes. Default 1 hour (1 day if all-day).")]
        duration_minutes: Option<i64>,
        #[graphql(desc = "Force an all-day event. A bare date start implies this.")]
        all_day: Option<bool>,
        #[graphql(desc = "IANA timezone for naive start/end, e.g. Europe/London")] tz: Option<
            String,
        >,
        #[graphql(desc = "Long-form notes")] description: Option<String>,
        #[graphql(desc = "Where it happens")] location: Option<String>,
        #[graphql(desc = "A link to attach")] url: Option<String>,
        #[graphql(desc = "CONFIRMED, TENTATIVE, or CANCELLED")] status: Option<String>,
        #[graphql(desc = "Recurrence rule, e.g. FREQ=WEEKLY;BYDAY=MO")] recurrence: Option<String>,
        #[graphql(desc = "Attendees as 'email' or 'Name <email>'")] attendees: Option<Vec<String>>,
        #[graphql(desc = "Categories/tags")] categories: Option<Vec<String>>,
    ) -> Result<GqlEventResult> {
        let calendar =
            calendar.or_else(|| ctx.data_opt::<DefaultCalendar>().and_then(|d| d.0.clone()));
        let input = EventInput {
            calendar,
            summary: Some(summary),
            start: Some(start),
            end,
            duration_minutes,
            all_day,
            tz,
            description,
            location,
            url,
            status,
            recurrence,
            attendees,
            categories,
        };

        let client = ctx.data::<SharedClient>()?;
        let attendees = input.parsed_attendees();
        let fields = input.fields(attendees.as_deref(), input.categories.as_deref());
        match client
            .create_event(input.calendar.as_deref(), &fields)
            .await
        {
            Ok(event) => Ok(GqlEventResult::done(event)),
            Err(e) => Ok(GqlEventResult::failed(e.to_string())),
        }
    }

    /// Update an event. Only the arguments you pass are changed. ALWAYS call
    /// with action=PREVIEW first — the preview shows a before → after diff.
    async fn update_event(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "PREVIEW first, then CONFIRM to write")] action: WriteAction,
        #[graphql(desc = "The event UID")] id: String,
        #[graphql(desc = "Restrict the lookup to one calendar")] calendar: Option<String>,
        #[graphql(desc = "New title")] summary: Option<String>,
        #[graphql(desc = "New start time")] start: Option<String>,
        #[graphql(desc = "New end time")] end: Option<String>,
        #[graphql(desc = "New length in minutes")] duration_minutes: Option<i64>,
        #[graphql(desc = "Switch to/from all-day")] all_day: Option<bool>,
        #[graphql(desc = "IANA timezone for naive start/end")] tz: Option<String>,
        #[graphql(desc = "New notes")] description: Option<String>,
        #[graphql(desc = "New location")] location: Option<String>,
        #[graphql(desc = "New link")] url: Option<String>,
        #[graphql(desc = "CONFIRMED, TENTATIVE, or CANCELLED")] status: Option<String>,
        #[graphql(desc = "New recurrence rule")] recurrence: Option<String>,
        #[graphql(desc = "Replaces the attendee list. Pass [] to clear it.")] attendees: Option<
            Vec<String>,
        >,
        #[graphql(desc = "Replaces the categories. Pass [] to clear them.")] categories: Option<
            Vec<String>,
        >,
        #[graphql(desc = "Token from the PREVIEW response — required for CONFIRM")]
        confirmation_token: Option<String>,
    ) -> Result<GqlEventResult> {
        let input = EventInput {
            calendar,
            summary,
            start,
            end,
            duration_minutes,
            all_day,
            tz,
            description,
            location,
            url,
            status,
            recurrence,
            attendees,
            categories,
        };

        let client = ctx.data::<SharedClient>()?;
        let nonce_store = ctx.data::<NonceStore>()?;
        // The id is part of the fingerprint so a CONFIRM can't be redirected at
        // a different event than the one previewed.
        let mut parts = vec![id.clone()];
        parts.extend(input.fingerprint_parts());
        let refs: Vec<&str> = parts.iter().map(String::as_str).collect();

        if action == WriteAction::Preview {
            let Some(existing) = client.get_event(&id, input.calendar.as_deref()).await? else {
                return Ok(GqlEventResult::failed(format!("Event not found: {id}")));
            };
            let token = issue_nonce(nonce_store, &refs).await;
            return Ok(GqlEventResult::pending(
                preview_update(&existing, &input),
                token,
            ));
        }
        if let Err(msg) = consume_nonce(nonce_store, confirmation_token.as_deref(), &refs).await {
            return Ok(GqlEventResult::failed(msg));
        }

        let attendees = input.parsed_attendees();
        let fields = input.fields(attendees.as_deref(), input.categories.as_deref());
        match client
            .update_event(&id, input.calendar.as_deref(), &fields)
            .await
        {
            Ok(event) => Ok(GqlEventResult::done(event)),
            Err(e) => Ok(GqlEventResult::failed(e.to_string())),
        }
    }

    /// Delete an event. ALWAYS call with action=PREVIEW first — the preview
    /// names the event and its time so the user can confirm it's the right one.
    async fn delete_event(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "PREVIEW first, then CONFIRM to delete")] action: WriteAction,
        #[graphql(desc = "The event UID")] id: String,
        #[graphql(desc = "Restrict the lookup to one calendar")] calendar: Option<String>,
        #[graphql(desc = "Token from the PREVIEW response — required for CONFIRM")]
        confirmation_token: Option<String>,
    ) -> Result<GqlEventResult> {
        let client = ctx.data::<SharedClient>()?;
        let nonce_store = ctx.data::<NonceStore>()?;
        let parts = [id.as_str(), calendar.as_deref().unwrap_or("")];

        if action == WriteAction::Preview {
            let Some(existing) = client.get_event(&id, calendar.as_deref()).await? else {
                return Ok(GqlEventResult::failed(format!("Event not found: {id}")));
            };
            let token = issue_nonce(nonce_store, &parts).await;
            let text = format!(
                "Delete event: {}\nCalendar: {}\nStarts: {}\nThis cannot be undone.",
                existing.summary.as_deref().unwrap_or("(no title)"),
                existing.calendar,
                show_time(&existing.start)
            );
            return Ok(GqlEventResult::pending(text, token));
        }
        if let Err(msg) = consume_nonce(nonce_store, confirmation_token.as_deref(), &parts).await {
            return Ok(GqlEventResult::failed(msg));
        }

        match client.delete_event(&id, calendar.as_deref()).await {
            Ok(event) => Ok(GqlEventResult::done(event)),
            Err(e) => Ok(GqlEventResult::failed(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::EventTime;

    fn existing_event() -> Event {
        Event {
            id: "evt-1".into(),
            calendar: "Home".into(),
            calendar_href: "/c/".into(),
            href: "/c/evt-1.ics".into(),
            resource_url: "https://dav.test/c/evt-1.ics".into(),
            etag: None,
            summary: Some("Standup".into()),
            description: None,
            location: Some("Room 4".into()),
            url: None,
            status: None,
            start: EventTime {
                date_time: Some("2026-07-24T09:00:00Z".into()),
                raw: "20260724T090000Z".into(),
                ..Default::default()
            },
            end: EventTime {
                date_time: Some("2026-07-24T09:30:00Z".into()),
                raw: "20260724T093000Z".into(),
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

    #[test]
    fn preview_resolves_times_into_the_instant_they_mean() {
        let input = EventInput {
            start: Some("2026-07-24 09:00".into()),
            tz: Some("Europe/London".into()),
            ..Default::default()
        };
        // 09:00 BST resolves to 08:00Z, and the zone is shown.
        let preview = preview_update(&existing_event(), &input);
        assert!(preview.contains("2026-07-24T08:00:00Z [Europe/London]"));
    }

    #[test]
    fn preview_flags_unparseable_times_instead_of_hiding_them() {
        let input = EventInput {
            start: Some("whenever".into()),
            ..Default::default()
        };
        assert!(preview_update(&existing_event(), &input).contains("unrecognised"));
    }

    #[test]
    fn update_preview_lists_only_changed_fields() {
        let input = EventInput {
            summary: Some("Standup (moved)".into()),
            start: Some("2026-07-24T10:00:00Z".into()),
            // Unchanged — must not appear as a change.
            location: Some("Room 4".into()),
            ..Default::default()
        };
        let preview = preview_update(&existing_event(), &input);
        assert!(preview.contains("Title: Standup → Standup (moved)"));
        assert!(preview.contains("Start: 2026-07-24T09:00:00Z → 2026-07-24T10:00:00Z"));
        assert!(!preview.contains("Location:"));
    }

    #[test]
    fn update_preview_says_so_when_nothing_changes() {
        let preview = preview_update(&existing_event(), &EventInput::default());
        assert!(preview.contains("No changes requested"));
    }

    #[test]
    fn update_preview_shows_clearing_a_field() {
        let input = EventInput {
            location: Some("".into()),
            ..Default::default()
        };
        assert!(preview_update(&existing_event(), &input).contains("Room 4 → (cleared)"));
    }

    #[test]
    fn fingerprint_distinguishes_absent_from_empty_lists() {
        let absent = EventInput::default();
        let cleared = EventInput {
            attendees: Some(vec![]),
            ..Default::default()
        };
        assert_ne!(absent.fingerprint_parts(), cleared.fingerprint_parts());
    }

    #[test]
    fn fingerprint_changes_when_any_field_changes() {
        let base = EventInput {
            summary: Some("a".into()),
            ..Default::default()
        };
        let changed = EventInput {
            summary: Some("b".into()),
            ..Default::default()
        };
        assert_ne!(base.fingerprint_parts(), changed.fingerprint_parts());
    }

    #[test]
    fn attendees_none_and_empty_parse_differently() {
        assert!(EventInput::default().parsed_attendees().is_none());
        let cleared = EventInput {
            attendees: Some(vec![]),
            ..Default::default()
        };
        assert_eq!(cleared.parsed_attendees().unwrap().len(), 0);
    }
}
