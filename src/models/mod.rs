//! Domain types shared by the CalDAV client, the CLI, and the GraphQL layer.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A calendar collection discovered under the principal's calendar-home-set.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Calendar {
    /// Stable short id — the last non-empty path segment of `href`. This is
    /// what CLI flags and GraphQL args accept, alongside the display name.
    pub id: String,
    /// Server path to the collection, e.g. `/1234/calendars/home/`.
    pub href: String,
    /// Absolute URL to address the collection.
    ///
    /// Kept separately from `href` because it is not always
    /// `server_url + href`: iCloud shards accounts onto partition hosts, so a
    /// calendar discovered via `caldav.icloud.com` actually lives on
    /// `pNN-caldav.icloud.com`. Requests must use this.
    #[serde(default)]
    pub url: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// `calendar-color` as sent by the server (`#RRGGBBAA` on Apple servers).
    #[serde(default)]
    pub color: Option<String>,
    /// True when the current principal may not write to this collection.
    pub read_only: bool,
    /// The account's own default calendar, as advertised by the server's
    /// `schedule-default-calendar-URL` (RFC 6638) — what the user's calendar
    /// app drops a new event into. Servers that don't publish it leave every
    /// calendar `false`.
    #[serde(default)]
    pub is_default: bool,
    /// False for collections that hold only VTODO/VJOURNAL — we skip those.
    pub supports_events: bool,
}

/// One end of an event's time range.
///
/// CalDAV hands back three flavours of time: a UTC instant (`...Z`), a local
/// time qualified by `TZID`, and a bare date for all-day events. We normalise
/// to a UTC instant where that's meaningful and always keep the original
/// value, so a round-trip PUT can reproduce exactly what the server sent.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EventTime {
    /// RFC 3339 timestamp in UTC. `None` for all-day times.
    #[serde(default)]
    pub date_time: Option<String>,
    /// `YYYY-MM-DD`, set only for all-day (`VALUE=DATE`) times.
    #[serde(default)]
    pub date: Option<String>,
    /// Olson timezone id from the `TZID` parameter, when the server sent one.
    #[serde(default)]
    pub tzid: Option<String>,
    pub all_day: bool,
    /// The iCalendar value exactly as it appeared on the wire.
    pub raw: String,
    /// Resolved instant, used for sorting and range filters. Not serialised —
    /// `date_time` is the wire form.
    #[serde(skip)]
    pub instant: Option<DateTime<Utc>>,
}

impl EventTime {
    /// Sort key: events we couldn't anchor sort last rather than crashing.
    pub fn sort_key(&self) -> DateTime<Utc> {
        self.instant.unwrap_or(DateTime::<Utc>::MAX_UTC)
    }
}

/// A participant on an event — `ORGANIZER` or one of the `ATTENDEE`s.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Attendee {
    /// Address with the `mailto:` scheme stripped.
    pub email: String,
    #[serde(default)]
    pub name: Option<String>,
    /// `ROLE` parameter, e.g. `REQ-PARTICIPANT`, `CHAIR`.
    #[serde(default)]
    pub role: Option<String>,
    /// `PARTSTAT` parameter, e.g. `ACCEPTED`, `NEEDS-ACTION`, `DECLINED`.
    #[serde(default)]
    pub status: Option<String>,
}

/// A VEVENT, flattened into something a CLI or model can act on.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    /// The iCalendar `UID`. Stable across updates — this is the event id.
    pub id: String,
    /// Display name of the calendar this event lives in.
    pub calendar: String,
    /// Path to the calendar collection.
    pub calendar_href: String,
    /// Path to the `.ics` resource itself.
    pub href: String,
    /// Absolute URL of the `.ics` resource, used for PUT/DELETE. Distinct from
    /// `url`, which is the event's own `URL` property. See [`Calendar::url`]
    /// for why this isn't derived from `href`.
    #[serde(default)]
    pub resource_url: String,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// `STATUS`: `CONFIRMED`, `TENTATIVE`, or `CANCELLED`.
    #[serde(default)]
    pub status: Option<String>,
    pub start: EventTime,
    pub end: EventTime,
    pub all_day: bool,
    /// Raw `RRULE` value, e.g. `FREQ=WEEKLY;BYDAY=MO`.
    #[serde(default)]
    pub recurrence: Option<String>,
    /// The recurrence properties (`DTSTART`, `RRULE`, `RDATE`, `EXDATE`) as
    /// they appeared on the wire, ready to hand to the expander. Not
    /// serialised — `recurrence` is the human-facing rule.
    #[serde(skip)]
    pub recur_source: Option<String>,
    /// Set on a single occurrence of a recurring series — either an expanded
    /// instance or a server-side override. The `id` stays the series `UID`, so
    /// `recurrenceId` is what distinguishes one occurrence from another.
    #[serde(default)]
    pub recurrence_id: Option<String>,
    #[serde(default)]
    pub organizer: Option<Attendee>,
    #[serde(default)]
    pub attendees: Vec<Attendee>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub created: Option<String>,
    #[serde(default)]
    pub last_modified: Option<String>,
    /// Bumped on every write so servers can detect out-of-order updates.
    #[serde(default)]
    pub sequence: u32,
}

impl Event {
    /// One-line rendering used in previews and agenda output.
    pub fn summary_line(&self) -> String {
        let when = match (&self.start.date, &self.start.date_time) {
            (Some(d), _) => d.clone(),
            (None, Some(dt)) => dt.clone(),
            (None, None) => self.start.raw.clone(),
        };
        let title = self.summary.as_deref().unwrap_or("(no title)");
        match &self.location {
            Some(loc) if !loc.is_empty() => format!("{when}  {title}  @ {loc}"),
            _ => format!("{when}  {title}"),
        }
    }
}

/// A busy window from a free/busy report.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BusyPeriod {
    /// RFC 3339, UTC.
    pub start: String,
    /// RFC 3339, UTC.
    pub end: String,
    /// `FBTYPE` parameter — `BUSY`, `BUSY-TENTATIVE`, `BUSY-UNAVAILABLE`.
    pub status: String,
}

/// Fields for creating or updating an event. All optional for updates, where
/// only the provided fields change.
#[derive(Debug, Clone, Default)]
pub struct EventFields<'a> {
    pub summary: Option<&'a str>,
    pub description: Option<&'a str>,
    pub location: Option<&'a str>,
    pub url: Option<&'a str>,
    pub status: Option<&'a str>,
    /// Start, as accepted by [`crate::util::parse_datetime`].
    pub start: Option<&'a str>,
    /// End. Mutually exclusive with `duration_minutes`.
    pub end: Option<&'a str>,
    /// Length in minutes, used when `end` is absent.
    pub duration_minutes: Option<i64>,
    pub all_day: Option<bool>,
    /// Timezone id for naive start/end values. Defaults to UTC.
    pub tzid: Option<&'a str>,
    pub recurrence: Option<&'a str>,
    pub attendees: Option<&'a [Attendee]>,
    pub categories: Option<&'a [String]>,
}

/// Uniform JSON envelope for CLI output: `{"success": true, "data": {...}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Output<T: Serialize> {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl<T: Serialize> Output<T> {
    pub fn success(data: T) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
            message: None,
        }
    }

    pub fn success_msg(message: impl Into<String>) -> Self {
        Self {
            success: true,
            data: None,
            error: None,
            message: Some(message.into()),
        }
    }

    pub fn error(err: impl Into<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(err.into()),
            message: None,
        }
    }

    pub fn print(&self) {
        match serde_json::to_string_pretty(self) {
            Ok(json) => println!("{json}"),
            Err(e) => eprintln!("{{\"success\":false,\"error\":\"Serialization failed: {e}\"}}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event_at(date: Option<&str>, date_time: Option<&str>) -> Event {
        Event {
            id: "uid-1".into(),
            calendar: "Home".into(),
            calendar_href: "/cal/home/".into(),
            href: "/cal/home/uid-1.ics".into(),
            resource_url: "https://dav.test/cal/home/uid-1.ics".into(),
            etag: None,
            summary: Some("Standup".into()),
            description: None,
            location: None,
            url: None,
            status: None,
            start: EventTime {
                date: date.map(str::to_string),
                date_time: date_time.map(str::to_string),
                all_day: date.is_some(),
                raw: date.or(date_time).unwrap_or_default().to_string(),
                ..Default::default()
            },
            end: EventTime::default(),
            all_day: date.is_some(),
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
    fn test_output_success() {
        let output: Output<&str> = Output::success("test data");
        assert!(output.success);
        assert_eq!(output.data, Some("test data"));
        assert!(output.error.is_none());
    }

    #[test]
    fn test_output_error() {
        let output: Output<()> = Output::error("something broke");
        assert!(!output.success);
        assert!(output.data.is_none());
        assert_eq!(output.error, Some("something broke".to_string()));
    }

    #[test]
    fn test_summary_line_prefers_date_for_all_day() {
        let e = event_at(Some("2026-07-24"), None);
        assert_eq!(e.summary_line(), "2026-07-24  Standup");
    }

    #[test]
    fn test_summary_line_includes_location() {
        let mut e = event_at(None, Some("2026-07-24T09:00:00Z"));
        e.location = Some("Room 4".into());
        assert_eq!(e.summary_line(), "2026-07-24T09:00:00Z  Standup  @ Room 4");
    }

    #[test]
    fn test_unanchored_times_sort_last() {
        let anchored = EventTime {
            instant: Some(DateTime::<Utc>::UNIX_EPOCH),
            ..Default::default()
        };
        let floating = EventTime::default();
        assert!(anchored.sort_key() < floating.sort_key());
    }
}
