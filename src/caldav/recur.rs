//! Client-side expansion of recurring series.
//!
//! CalDAV servers can expand a series themselves (RFC 4791 §9.6.5
//! `<C:expand>`), but the support is uneven — iCloud in particular is
//! unreliable here, and the reference Python client moved to expanding
//! client-side by default for exactly that reason. So we ask the server to
//! expand, and expand anything it hands back unexpanded ourselves. The result
//! is the same shape either way.
//!
//! The recurrence rules themselves are evaluated by the `rrule` crate rather
//! than hand-rolled: RFC 5545 recurrence is a genuinely large spec (BYSETPOS,
//! BYDAY ordinals, leap-year BYMONTHDAY, DST-crossing intervals) and getting
//! it subtly wrong means silently showing someone the wrong day.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use rrule::RRuleSet;
use tracing::debug;

use crate::models::{Event, EventTime};
use crate::util;

/// Ceiling on occurrences generated from a single series, so an unbounded
/// `FREQ=SECONDLY` rule can't hang the process.
const MAX_OCCURRENCES: u16 = 750;

/// Expand every unexpanded series in `events` across `[start, end)`.
///
/// Events that are not series, and occurrences the server already expanded,
/// pass through untouched. Server-side overrides (a VEVENT with a
/// `RECURRENCE-ID`) win over the generated instance for their slot, and an
/// override marked `CANCELLED` removes that occurrence entirely.
pub fn expand_all(events: Vec<Event>, start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<Event> {
    // Overrides are keyed by (series uid, occurrence instant) so a generated
    // instance can be replaced by the edited one.
    let mut overridden: HashSet<(String, String)> = HashSet::new();
    let mut cancelled: HashSet<(String, String)> = HashSet::new();
    for e in &events {
        if let Some(rid) = &e.recurrence_id {
            overridden.insert((e.id.clone(), rid.clone()));
            if e.status.as_deref() == Some("CANCELLED") {
                cancelled.insert((e.id.clone(), rid.clone()));
            }
        }
    }

    let mut out: Vec<Event> = Vec::new();
    // A series can be split across resources; only expand each uid once.
    let mut expanded: HashMap<String, ()> = HashMap::new();

    for event in events {
        // A cancelled override is a deletion marker, not something to show.
        if let Some(rid) = &event.recurrence_id
            && cancelled.contains(&(event.id.clone(), rid.clone()))
        {
            continue;
        }

        let Some(source) = event.recur_source.clone() else {
            out.push(event);
            continue;
        };
        // Already an occurrence (server-expanded or an override) — not a
        // master to expand.
        if event.recurrence_id.is_some() {
            out.push(event);
            continue;
        }
        if expanded.insert(event.id.clone(), ()).is_some() {
            out.push(event);
            continue;
        }

        match occurrences(&source, start, end) {
            Ok(instants) if !instants.is_empty() => {
                out.extend(instantiate(&event, &instants, &overridden, &cancelled));
            }
            Ok(_) => {
                // The rule is valid but yields nothing in this window. The
                // master itself is not an occurrence, so it is dropped.
                debug!(uid = %event.id, "series has no occurrences in range");
            }
            Err(e) => {
                // An unparseable rule shouldn't hide the event entirely —
                // fall back to showing the master as-is.
                debug!(uid = %event.id, error = %e, "could not expand series");
                out.push(event);
            }
        }
    }

    out.sort_by_key(|e| e.start.sort_key());
    out
}

/// Evaluate a recurrence set over `[start, end)`.
fn occurrences(
    source: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<DateTime<Utc>>, String> {
    let set: RRuleSet = source.parse().map_err(|e| format!("{e}"))?;
    // `after`/`before` are inclusive at both ends; the window is half-open, so
    // the upper bound is filtered below.
    let result = set
        .after(start.with_timezone(&rrule::Tz::UTC))
        .before(end.with_timezone(&rrule::Tz::UTC))
        .all(MAX_OCCURRENCES);
    if result.limited {
        debug!(limit = MAX_OCCURRENCES, "series hit the occurrence cap");
    }
    Ok(result
        .dates
        .into_iter()
        .map(|d| d.with_timezone(&Utc))
        .filter(|d| *d >= start && *d < end)
        .collect())
}

/// Clone the master into one event per occurrence, preserving its duration.
fn instantiate(
    master: &Event,
    instants: &[DateTime<Utc>],
    overridden: &HashSet<(String, String)>,
    cancelled: &HashSet<(String, String)>,
) -> Vec<Event> {
    // The series duration, applied to every occurrence. A master whose end we
    // couldn't resolve is treated as zero-length rather than dropped.
    let duration = match (master.start.instant, master.end.instant) {
        (Some(s), Some(e)) if e >= s => e - s,
        _ => Duration::zero(),
    };

    instants
        .iter()
        .filter_map(|instant| {
            let rid = util::format_rfc3339(*instant);
            let key = (master.id.clone(), rid.clone());
            // The server already sent a bespoke version of this occurrence, or
            // it was cancelled — either way, don't generate one.
            if overridden.contains(&key) || cancelled.contains(&key) {
                return None;
            }

            let mut occurrence = master.clone();
            occurrence.start = shift(&master.start, *instant);
            occurrence.end = shift(&master.end, *instant + duration);
            occurrence.recurrence_id = Some(rid);
            Some(occurrence)
        })
        .collect()
}

/// Rebuild an [`EventTime`] at a new instant, keeping its all-day-ness and zone.
fn shift(template: &EventTime, instant: DateTime<Utc>) -> EventTime {
    EventTime {
        date_time: (!template.all_day).then(|| util::format_rfc3339(instant)),
        date: template.all_day.then(|| util::format_iso_date(instant)),
        tzid: template.tzid.clone(),
        all_day: template.all_day,
        raw: if template.all_day {
            util::format_ical_date(instant)
        } else {
            util::format_ical_utc(instant)
        },
        instant: Some(instant),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caldav::ical;
    use chrono::TimeZone;

    fn parse(ics: &str) -> Vec<Event> {
        ical::parse_events(ics, "Home", "/c/", "/c/x.ics", None)
    }

    fn at(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    /// A weekly Monday standup, defined in London local time.
    const WEEKLY: &str = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:standup\r\nSUMMARY:Standup\r\n\
DTSTART;TZID=Europe/London:20260706T090000\r\nDTEND;TZID=Europe/London:20260706T093000\r\n\
RRULE:FREQ=WEEKLY;BYDAY=MO\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn captures_the_recurrence_source_with_its_tzid() {
        let event = parse(WEEKLY).remove(0);
        let source = event.recur_source.expect("series must carry its source");
        assert!(source.contains("DTSTART;TZID=Europe/London:20260706T090000"));
        assert!(source.contains("RRULE:FREQ=WEEKLY;BYDAY=MO"));
    }

    #[test]
    fn non_recurring_events_have_no_source() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:one\r\nSUMMARY:Once\r\n\
DTSTART:20260724T090000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(parse(ics)[0].recur_source.is_none());
    }

    #[test]
    fn expands_a_weekly_series_across_the_window() {
        // Three Mondays: 6, 13, 20 July 2026.
        let out = expand_all(parse(WEEKLY), at(2026, 7, 6, 0), at(2026, 7, 21, 0));
        assert_eq!(out.len(), 3);
        let starts: Vec<&str> = out
            .iter()
            .map(|e| e.start.date_time.as_deref().unwrap())
            .collect();
        assert_eq!(
            starts,
            [
                "2026-07-06T08:00:00Z",
                "2026-07-13T08:00:00Z",
                "2026-07-20T08:00:00Z"
            ]
        );
        // Every occurrence keeps the series id and its own recurrence id.
        assert!(out.iter().all(|e| e.id == "standup"));
        assert_eq!(
            out[1].recurrence_id.as_deref(),
            Some("2026-07-13T08:00:00Z")
        );
    }

    #[test]
    fn occurrences_keep_the_series_duration() {
        let out = expand_all(parse(WEEKLY), at(2026, 7, 6, 0), at(2026, 7, 8, 0));
        assert_eq!(
            out[0].start.date_time.as_deref(),
            Some("2026-07-06T08:00:00Z")
        );
        assert_eq!(
            out[0].end.date_time.as_deref(),
            Some("2026-07-06T08:30:00Z")
        );
    }

    #[test]
    fn expansion_holds_local_time_across_a_dst_boundary() {
        // A weekly 09:00 London meeting is 08:00Z in BST and 09:00Z in GMT.
        // Expanding in UTC without the TZID would drift it by an hour.
        let out = expand_all(parse(WEEKLY), at(2026, 10, 20, 0), at(2026, 11, 3, 0));
        let starts: Vec<&str> = out
            .iter()
            .map(|e| e.start.date_time.as_deref().unwrap())
            .collect();
        // Clocks go back on 25 October 2026.
        assert_eq!(
            starts,
            ["2026-10-26T09:00:00Z", "2026-11-02T09:00:00Z"],
            "wall-clock time should stay 09:00 local"
        );
    }

    #[test]
    fn window_is_half_open() {
        // The 20 July occurrence sits exactly on the upper bound — excluded.
        let out = expand_all(parse(WEEKLY), at(2026, 7, 13, 0), at(2026, 7, 20, 8));
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].start.date_time.as_deref(),
            Some("2026-07-13T08:00:00Z")
        );
    }

    #[test]
    fn honours_exdate() {
        let ics = WEEKLY.replace(
            "RRULE:FREQ=WEEKLY;BYDAY=MO\r\n",
            "RRULE:FREQ=WEEKLY;BYDAY=MO\r\nEXDATE;TZID=Europe/London:20260713T090000\r\n",
        );
        let out = expand_all(parse(&ics), at(2026, 7, 6, 0), at(2026, 7, 21, 0));
        let starts: Vec<&str> = out
            .iter()
            .map(|e| e.start.date_time.as_deref().unwrap())
            .collect();
        assert_eq!(starts, ["2026-07-06T08:00:00Z", "2026-07-20T08:00:00Z"]);
    }

    #[test]
    fn honours_count_and_until() {
        let counted = WEEKLY.replace("FREQ=WEEKLY;BYDAY=MO", "FREQ=WEEKLY;BYDAY=MO;COUNT=2");
        let out = expand_all(parse(&counted), at(2026, 7, 6, 0), at(2026, 8, 30, 0));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn a_server_side_override_replaces_its_occurrence() {
        // The 13 July standup was moved to 11:00 and renamed.
        let override_ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:standup\r\n\
SUMMARY:Standup (moved)\r\nDTSTART:20260713T100000Z\r\nDTEND:20260713T103000Z\r\n\
RECURRENCE-ID:20260713T080000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let mut events = parse(WEEKLY);
        events.extend(parse(override_ics));

        let out = expand_all(events, at(2026, 7, 6, 0), at(2026, 7, 21, 0));
        assert_eq!(out.len(), 3, "override must replace, not duplicate");
        let moved = out
            .iter()
            .find(|e| e.summary.as_deref() == Some("Standup (moved)"))
            .expect("override survives");
        assert_eq!(
            moved.start.date_time.as_deref(),
            Some("2026-07-13T10:00:00Z")
        );
        // And no generated instance remains at the original slot.
        assert!(
            !out.iter()
                .any(|e| { e.start.date_time.as_deref() == Some("2026-07-13T08:00:00Z") })
        );
    }

    #[test]
    fn a_cancelled_override_removes_its_occurrence() {
        let cancelled = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:standup\r\nSUMMARY:Standup\r\n\
DTSTART:20260713T080000Z\r\nSTATUS:CANCELLED\r\nRECURRENCE-ID:20260713T080000Z\r\n\
END:VEVENT\r\nEND:VCALENDAR\r\n";
        let mut events = parse(WEEKLY);
        events.extend(parse(cancelled));

        let out = expand_all(events, at(2026, 7, 6, 0), at(2026, 7, 21, 0));
        assert_eq!(out.len(), 2);
        assert!(
            !out.iter()
                .any(|e| { e.start.date_time.as_deref() == Some("2026-07-13T08:00:00Z") })
        );
    }

    #[test]
    fn already_expanded_occurrences_pass_through_untouched() {
        // What a server that honoured <expand> hands back: no RRULE, but a
        // RECURRENCE-ID. Expanding again must not multiply it.
        let instance = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:standup\r\nSUMMARY:Standup\r\n\
DTSTART:20260713T080000Z\r\nDTEND:20260713T083000Z\r\nRECURRENCE-ID:20260713T080000Z\r\n\
END:VEVENT\r\nEND:VCALENDAR\r\n";
        let out = expand_all(parse(instance), at(2026, 7, 6, 0), at(2026, 7, 21, 0));
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].start.date_time.as_deref(),
            Some("2026-07-13T08:00:00Z")
        );
    }

    #[test]
    fn plain_events_are_left_alone() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:one\r\nSUMMARY:Once\r\n\
DTSTART:20260724T090000Z\r\nDTEND:20260724T100000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let out = expand_all(parse(ics), at(2026, 7, 1, 0), at(2026, 8, 1, 0));
        assert_eq!(out.len(), 1);
        assert!(out[0].recurrence_id.is_none());
    }

    #[test]
    fn an_unparseable_rule_still_shows_the_event() {
        let ics = WEEKLY.replace("FREQ=WEEKLY;BYDAY=MO", "FREQ=NONSENSE");
        let out = expand_all(parse(&ics), at(2026, 7, 6, 0), at(2026, 7, 21, 0));
        assert_eq!(out.len(), 1, "a bad rule must not hide the event");
        assert_eq!(out[0].summary.as_deref(), Some("Standup"));
    }

    #[test]
    fn expands_all_day_series_as_dates() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:bins\r\nSUMMARY:Bin day\r\n\
DTSTART;VALUE=DATE:20260706\r\nDTEND;VALUE=DATE:20260707\r\n\
RRULE:FREQ=WEEKLY;BYDAY=MO\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let out = expand_all(parse(ics), at(2026, 7, 6, 0), at(2026, 7, 21, 0));
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|e| e.all_day));
        assert_eq!(out[1].start.date.as_deref(), Some("2026-07-13"));
        assert!(out[1].start.date_time.is_none());
    }

    #[test]
    fn output_is_sorted_by_start() {
        let out = expand_all(parse(WEEKLY), at(2026, 7, 6, 0), at(2026, 7, 28, 0));
        let mut sorted = out.clone();
        sorted.sort_by_key(|e| e.start.sort_key());
        let a: Vec<_> = out.iter().map(|e| e.start.sort_key()).collect();
        let b: Vec<_> = sorted.iter().map(|e| e.start.sort_key()).collect();
        assert_eq!(a, b);
    }
}
