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

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{DateTime, Duration, TimeZone, Utc};
use rrule::{Frequency, RRuleSet};
use tracing::debug;

use crate::error::{Error, Result};
use crate::models::{Event, EventTime};
use crate::util;

/// Ceiling on occurrences generated from a single series. Further occurrences
/// in the window are dropped.
const MAX_OCCURRENCES: usize = 750;

/// Ceiling on dates a single series may step through, including those before
/// the window. `rrule` iterates from `DTSTART`, so without this an old
/// `FREQ=SECONDLY` series costs one step per second since it began.
const MAX_STEPS: usize = 50_000;

/// Ceiling on how far past its anchor a sub-daily series may iterate, counted
/// in steps of its interval. rrule advances one interval at a time while
/// BYHOUR, BYMINUTE or BYSECOND rejects the value, so a sparse filter costs
/// this many steps between occurrences, all of them inside a single yield that
/// [`MAX_STEPS`] never sees.
const MAX_WORK: i64 = 5_000_000;

/// Ceiling on occurrences one caller's expansions may produce in all. Each
/// occurrence is a full copy of its master event, so this bounds the memory a
/// wide window over many series and calendars can take.
pub const MAX_EXPANDED: usize = 25_000;

/// What is left of [`MAX_EXPANDED`] for one scope: a GraphQL request, or one
/// CLI read. Shared by every expansion in that scope, so the cap holds across
/// calendars and fields rather than per window.
#[derive(Debug, Default)]
pub struct ExpansionBudget(AtomicUsize);

impl ExpansionBudget {
    /// Reserve `n` occurrences, or refuse without reserving any.
    fn charge(&self, n: usize) -> Result<()> {
        self.0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(n).filter(|total| *total <= MAX_EXPANDED)
            })
            .map(drop)
            .map_err(|_| Error::TooManyOccurrences(MAX_EXPANDED))
    }
}

/// Expand every unexpanded series in `events` across `[start, end)`.
///
/// Events that are not series, and occurrences the server already expanded,
/// pass through untouched. Server-side overrides (a VEVENT with a
/// `RECURRENCE-ID`) win over the generated instance for their slot, and an
/// override marked `CANCELLED` removes that occurrence entirely.
///
/// Fails with [`Error::TooManyOccurrences`] rather than truncating when the
/// occurrences would take `budget` past [`MAX_EXPANDED`].
pub fn expand_all(
    events: Vec<Event>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    budget: &ExpansionBudget,
) -> Result<Vec<Event>> {
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
    // Instants are counted before any master is cloned, so a window over the
    // budget is refused without paying for it.
    let mut series: Vec<(Event, Vec<DateTime<Utc>>)> = Vec::new();
    // A series can be split across resources; only expand each uid once.
    let mut expanded: HashSet<String> = HashSet::new();

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
        if !expanded.insert(event.id.clone()) {
            out.push(event);
            continue;
        }

        match occurrences(&source, start, end) {
            Ok(instants) if !instants.is_empty() => series.push((event, instants)),
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

    budget.charge(series.iter().map(|(_, instants)| instants.len()).sum())?;
    for (master, instants) in &series {
        out.extend(instantiate(master, instants, &overridden, &cancelled));
    }

    out.sort_by_key(|e| e.start.sort_key());
    Ok(out)
}

/// Pin date-valued recurrence anchors to UTC midnight.
///
/// An all-day series' `DTSTART` is a *date* — no timezone, the same day
/// everywhere — but `RRuleSet` has to choose an instant and chooses midnight in
/// the **machine's local zone**. Run east of Greenwich and every occurrence
/// lands at 23:00 the day before, so a Monday bin collection is reported on
/// Sunday, and an occurrence falling on the first day of the window is dropped
/// for sorting before it.
///
/// Rewriting the anchor as an explicit UTC instant leaves rrule nothing to
/// interpret, and matches how [`crate::caldav::ical::parse_time`] resolves the
/// same date. A no-op for timed series, whose values carry a time and are
/// therefore left alone.
///
/// A date `UNTIL` on an all-day series is pinned the same way. It is inclusive
/// of its day (RFC 5545 §3.3.10), and UTC midnight on that day is the instant
/// the last occurrence lands on. On a timed series a date `UNTIL` is invalid,
/// and no single instant says "through the end of that day" in every zone, so it
/// is left for rrule to refuse.
fn pin_dates_to_utc(source: &str) -> String {
    let is_bare_date = |v: &str| v.len() == 8 && v.bytes().all(|b| b.is_ascii_digit());
    let all_day = source.lines().any(|line| {
        line.split_once(':').is_some_and(|(head, value)| {
            head.split(';').next() == Some("DTSTART") && is_bare_date(value.trim())
        })
    });
    source
        .lines()
        .map(|line| {
            let Some((head, values)) = line.split_once(':') else {
                return line.to_string();
            };
            if all_day && matches!(head, "RRULE" | "EXRULE") {
                let parts: Vec<String> = values
                    .split(';')
                    .map(|part| match part.split_once('=') {
                        Some((key, v)) if key.eq_ignore_ascii_case("UNTIL") && is_bare_date(v) => {
                            format!("{key}={v}T000000Z")
                        }
                        _ => part.to_string(),
                    })
                    .collect();
                return format!("{head}:{}", parts.join(";"));
            }
            if !values.split(',').any(|v| is_bare_date(v.trim())) {
                return line.to_string();
            }
            let pinned: Vec<String> = values
                .split(',')
                .map(|v| {
                    let v = v.trim();
                    if is_bare_date(v) {
                        format!("{v}T000000Z")
                    } else {
                        v.to_string()
                    }
                })
                .collect();
            // `VALUE=DATE` and any `TZID` no longer describe the value.
            let property = head.split(';').next().unwrap_or(head);
            format!("{property}:{}", pinned.join(","))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Evaluate a recurrence set over `[start, end)`.
fn occurrences(
    source: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> std::result::Result<Vec<DateTime<Utc>>, String> {
    let set: RRuleSet = pin_dates_to_utc(source)
        .parse()
        .map_err(|e| format!("{e}"))?;
    let set = fast_forward(set, start);
    let anchor = set.get_dt_start().with_timezone(&Utc);
    // The finest interval any rule steps by; `None` for daily and slower.
    let step_secs = set
        .get_rrule()
        .iter()
        .filter_map(|rule| {
            Some(sub_daily_unit(rule.get_freq())? * i64::from(rule.get_interval().max(1)))
        })
        .min();
    let mut dates = Vec::new();
    // `limit` arms rrule's own guard against a rule that never matches, which
    // would otherwise spin inside a single `next`.
    for (step, date) in set.limit().into_iter().enumerate() {
        if step == MAX_STEPS {
            debug!(limit = MAX_STEPS, "series hit the step cap");
            break;
        }
        let date = date.with_timezone(&Utc);
        if step_secs.is_some_and(|secs| (date - anchor).num_seconds() / secs > MAX_WORK) {
            debug!(limit = MAX_WORK, "series hit the work cap");
            break;
        }
        if date >= end {
            break;
        }
        if date >= start {
            dates.push(date);
            if dates.len() == MAX_OCCURRENCES {
                debug!(limit = MAX_OCCURRENCES, "series hit the occurrence cap");
                break;
            }
        }
    }
    Ok(dates)
}

/// Move a sub-daily series' `DTSTART` forward to shortly before `start`, so
/// iteration begins near the window rather than wherever the series began.
///
/// The anchor moves by whole multiples of the rule's interval in local wall
/// time, which is how rrule steps, so every later occurrence is unchanged. It
/// stays put whenever that would not hold:
///
/// - COUNT is counted from `DTSTART`, so a counted rule cannot move.
/// - With more than one rule, or an EXRULE, the rules share the anchor.
/// - rrule carries a sub-hour step into the next minute or hour one unit at a
///   time while BYMINUTE or BYHOUR rejects it, which drifts the grid unless the
///   interval divides a minute. Moving the anchor would then change the drift.
///
/// The anchor never passes UNTIL, which rrule requires to be no earlier. Daily
/// and slower rules are left alone, since [`MAX_STEPS`] of them spans over a
/// century.
fn fast_forward(set: RRuleSet, start: DateTime<Utc>) -> RRuleSet {
    let [rule] = set.get_rrule().as_slice() else {
        return set;
    };
    let interval = i64::from(rule.get_interval().max(1));
    let carries_through_a_filter = 60 % interval != 0
        && match rule.get_freq() {
            Frequency::Minutely => !rule.get_by_hour().is_empty(),
            Frequency::Secondly => {
                !rule.get_by_hour().is_empty() || !rule.get_by_minute().is_empty()
            }
            _ => false,
        };
    let Some(unit_secs) = sub_daily_unit(rule.get_freq()) else {
        return set;
    };
    if rule.get_count().is_some() || !set.get_exrule().is_empty() || carries_through_a_filter {
        return set;
    }

    let dt_start = *set.get_dt_start();
    let tz = dt_start.timezone();
    let step = unit_secs * interval;
    let from = dt_start.naive_local();
    // One step and three hours of margin: wall time can run up to two hours
    // behind the instant across a DST change, so no occurrence at or after
    // `start` can fall before the new anchor.
    let mut target = start.with_timezone(&tz).naive_local() - Duration::seconds(step + 3 * 3600);
    if let Some(until) = rule.get_until() {
        target = target.min(until.with_timezone(&tz).naive_local());
    }
    let steps = (target - from).num_seconds() / step;

    // rrule refuses a DTSTART in a DST gap or overlap, so step back to one
    // that names a single instant.
    let anchor = (steps.saturating_sub(4)..=steps)
        .rev()
        .take_while(|k| *k > 0)
        .find_map(|k| {
            let naive = from.checked_add_signed(Duration::try_seconds(k.checked_mul(step)?)?)?;
            tz.from_local_datetime(&naive).single()
        });
    let Some(anchor) = anchor else {
        return set;
    };
    RRuleSet::new(anchor)
        .set_rrules(set.get_rrule().clone())
        .set_rdates(set.get_rdate().clone())
        .set_exdates(set.get_exdate().clone())
}

/// Seconds in one step of a sub-daily frequency; `None` for daily and slower.
fn sub_daily_unit(freq: Frequency) -> Option<i64> {
    match freq {
        Frequency::Hourly => Some(3600),
        Frequency::Minutely => Some(60),
        Frequency::Secondly => Some(1),
        _ => None,
    }
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
        let out = expand_all(
            parse(WEEKLY),
            at(2026, 7, 6, 0),
            at(2026, 7, 21, 0),
            &ExpansionBudget::default(),
        )
        .unwrap();
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
        let out = expand_all(
            parse(WEEKLY),
            at(2026, 7, 6, 0),
            at(2026, 7, 8, 0),
            &ExpansionBudget::default(),
        )
        .unwrap();
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
        let out = expand_all(
            parse(WEEKLY),
            at(2026, 10, 20, 0),
            at(2026, 11, 3, 0),
            &ExpansionBudget::default(),
        )
        .unwrap();
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
        let out = expand_all(
            parse(WEEKLY),
            at(2026, 7, 13, 0),
            at(2026, 7, 20, 8),
            &ExpansionBudget::default(),
        )
        .unwrap();
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
        let out = expand_all(
            parse(&ics),
            at(2026, 7, 6, 0),
            at(2026, 7, 21, 0),
            &ExpansionBudget::default(),
        )
        .unwrap();
        let starts: Vec<&str> = out
            .iter()
            .map(|e| e.start.date_time.as_deref().unwrap())
            .collect();
        assert_eq!(starts, ["2026-07-06T08:00:00Z", "2026-07-20T08:00:00Z"]);
    }

    #[test]
    fn honours_count_and_until() {
        let counted = WEEKLY.replace("FREQ=WEEKLY;BYDAY=MO", "FREQ=WEEKLY;BYDAY=MO;COUNT=2");
        let out = expand_all(
            parse(&counted),
            at(2026, 7, 6, 0),
            at(2026, 8, 30, 0),
            &ExpansionBudget::default(),
        )
        .unwrap();
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

        let out = expand_all(
            events,
            at(2026, 7, 6, 0),
            at(2026, 7, 21, 0),
            &ExpansionBudget::default(),
        )
        .unwrap();
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

        let out = expand_all(
            events,
            at(2026, 7, 6, 0),
            at(2026, 7, 21, 0),
            &ExpansionBudget::default(),
        )
        .unwrap();
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
        let out = expand_all(
            parse(instance),
            at(2026, 7, 6, 0),
            at(2026, 7, 21, 0),
            &ExpansionBudget::default(),
        )
        .unwrap();
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
        let out = expand_all(
            parse(ics),
            at(2026, 7, 1, 0),
            at(2026, 8, 1, 0),
            &ExpansionBudget::default(),
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].recurrence_id.is_none());
    }

    #[test]
    fn an_unparseable_rule_still_shows_the_event() {
        let ics = WEEKLY.replace("FREQ=WEEKLY;BYDAY=MO", "FREQ=NONSENSE");
        let out = expand_all(
            parse(&ics),
            at(2026, 7, 6, 0),
            at(2026, 7, 21, 0),
            &ExpansionBudget::default(),
        )
        .unwrap();
        assert_eq!(out.len(), 1, "a bad rule must not hide the event");
        assert_eq!(out[0].summary.as_deref(), Some("Standup"));
    }

    #[test]
    fn expands_all_day_series_as_dates() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:bins\r\nSUMMARY:Bin day\r\n\
DTSTART;VALUE=DATE:20260706\r\nDTEND;VALUE=DATE:20260707\r\n\
RRULE:FREQ=WEEKLY;BYDAY=MO\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let out = expand_all(
            parse(ics),
            at(2026, 7, 6, 0),
            at(2026, 7, 21, 0),
            &ExpansionBudget::default(),
        )
        .unwrap();
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|e| e.all_day));
        assert_eq!(out[1].start.date.as_deref(), Some("2026-07-13"));
        assert!(out[1].start.date_time.is_none());
    }

    #[test]
    fn output_is_sorted_by_start() {
        let out = expand_all(
            parse(WEEKLY),
            at(2026, 7, 6, 0),
            at(2026, 7, 28, 0),
            &ExpansionBudget::default(),
        )
        .unwrap();
        let mut sorted = out.clone();
        sorted.sort_by_key(|e| e.start.sort_key());
        let a: Vec<_> = out.iter().map(|e| e.start.sort_key()).collect();
        let b: Vec<_> = sorted.iter().map(|e| e.start.sort_key()).collect();
        assert_eq!(a, b);
    }

    fn series(dtstart: &str, rule: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:s\r\nSUMMARY:S\r\n{dtstart}\r\n{rule}\r\n\
END:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    }

    fn starts(events: &[Event]) -> Vec<DateTime<Utc>> {
        events.iter().filter_map(|e| e.start.instant).collect()
    }

    /// Expansion without moving the anchor, as rrule would do it unaided.
    fn unshifted(ics: &str, start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<DateTime<Utc>> {
        let source = parse(ics).remove(0).recur_source.unwrap();
        let set: RRuleSet = pin_dates_to_utc(&source).parse().unwrap();
        set.limit()
            .into_iter()
            .map(|d| d.with_timezone(&Utc))
            .skip_while(|d| *d < start)
            .take_while(|d| *d < end)
            .collect()
    }

    /// Stepping from a 2020 DTSTART one second at a time took minutes; the
    /// window's occurrences must come back without that walk.
    #[test]
    fn an_old_secondly_series_expands_near_the_window() {
        let ics = series("DTSTART:20200101T000000Z", "RRULE:FREQ=SECONDLY");
        let from = at(2026, 7, 6, 9);
        let out = expand_all(
            parse(&ics),
            from,
            from + Duration::seconds(10),
            &ExpansionBudget::default(),
        )
        .unwrap();
        assert_eq!(
            starts(&out),
            (0..10)
                .map(|s| from + Duration::seconds(s))
                .collect::<Vec<_>>()
        );
    }

    /// Every sub-daily rule shape the anchor may move for must expand exactly
    /// as it would unmoved, and the shapes it must not move for must stay put.
    #[test]
    fn moving_the_anchor_leaves_every_occurrence_where_it_was() {
        let filters = [
            "",
            ";BYHOUR=9",
            ";BYMINUTE=0",
            ";BYSECOND=5",
            ";BYHOUR=1;BYMINUTE=0",
            ";BYDAY=SU",
        ];
        // The window straddles the end of BST at 01:00Z on 25 October 2026. Each
        // series starts far enough before it for the anchor to move past its
        // margin, and near enough that the unmoved reference stays cheap.
        let (from, to) = (at(2026, 10, 25, 0), at(2026, 10, 25, 3));
        let starts_at = [
            ("HOURLY", "20261015T000700"),
            ("MINUTELY", "20261023T230700"),
            ("SECONDLY", "20261024T190700"),
        ];
        let (mut moved, mut expected_moved) = (0, 0);
        for (freq, local) in starts_at {
            let dtstart = format!("DTSTART;TZID=Europe/London:{local}");
            for interval in 1..=61 {
                for filter in filters {
                    let drifts = 60 % interval != 0
                        && match freq {
                            "MINUTELY" => filter.contains("BYHOUR"),
                            "SECONDLY" => filter.contains("BYHOUR") || filter.contains("BYMINUTE"),
                            _ => false,
                        };
                    if !drifts {
                        expected_moved += 1;
                    }
                    let rule = format!("RRULE:FREQ={freq};INTERVAL={interval}{filter}");
                    let ics = series(&dtstart, &rule);
                    let source = parse(&ics).remove(0).recur_source.unwrap();
                    let set: RRuleSet = pin_dates_to_utc(&source).parse().unwrap();
                    let before = *set.get_dt_start();
                    if *fast_forward(set, from).get_dt_start() != before {
                        moved += 1;
                    }

                    let out = expand_all(parse(&ics), from, to, &ExpansionBudget::default());
                    let mut expected = unshifted(&ics, from, to);
                    expected.truncate(MAX_OCCURRENCES);
                    assert_eq!(starts(&out.unwrap()), expected, "{rule}");
                }
            }
        }
        // Without this the comparison could pass with nothing ever moved.
        assert_eq!(moved, expected_moved);
    }

    #[test]
    fn a_finished_sub_daily_series_has_no_later_occurrences() {
        let ics = series(
            "DTSTART;TZID=Europe/London:20200101T090000",
            "RRULE:FREQ=HOURLY;UNTIL=20200301T000000Z",
        );
        let out = expand_all(
            parse(&ics),
            at(2026, 7, 6, 0),
            at(2026, 7, 7, 0),
            &ExpansionBudget::default(),
        )
        .unwrap();
        assert!(
            out.is_empty(),
            "a finished series must not fall back to its master"
        );
    }

    #[test]
    fn a_counted_sub_daily_series_is_bounded_by_the_step_cap() {
        let ics = series(
            "DTSTART:20200101T000000Z",
            "RRULE:FREQ=SECONDLY;COUNT=4000000000",
        );
        let out = expand_all(
            parse(&ics),
            at(2026, 7, 6, 0),
            at(2026, 7, 7, 0),
            &ExpansionBudget::default(),
        )
        .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn an_excluding_rule_does_not_spin() {
        let ics = series(
            "DTSTART:20200101T000000Z",
            "RRULE:FREQ=SECONDLY\r\nEXRULE:FREQ=SECONDLY",
        );
        expand_all(
            parse(&ics),
            at(2026, 7, 6, 0),
            at(2026, 7, 7, 0),
            &ExpansionBudget::default(),
        )
        .unwrap();
    }

    #[test]
    fn a_rule_that_never_matches_returns() {
        let ics = series(
            "DTSTART:20260101T000000Z",
            "RRULE:FREQ=SECONDLY;BYMONTH=2;BYMONTHDAY=30",
        );
        let out = expand_all(
            parse(&ics),
            at(2026, 1, 1, 0),
            at(2027, 1, 1, 0),
            &ExpansionBudget::default(),
        )
        .unwrap();
        assert!(out.is_empty());
    }

    /// The budget spans every expansion sharing it, so many calendars cannot
    /// each stay just under the cap.
    #[test]
    fn a_budget_over_its_cap_is_refused_rather_than_cut() {
        let hours = 500;
        let to = at(2026, 1, 1, 0) + Duration::hours(hours);
        let calendar = |i: usize| {
            let ics = series("DTSTART:20260101T000000Z", "RRULE:FREQ=HOURLY")
                .replace("UID:s", &format!("UID:s{i}"));
            parse(&ics)
        };
        let budget = ExpansionBudget::default();
        for i in 0..MAX_EXPANDED / hours as usize {
            expand_all(calendar(i), at(2026, 1, 1, 0), to, &budget).unwrap();
        }
        assert!(matches!(
            expand_all(calendar(0), at(2026, 1, 1, 0), to, &budget),
            Err(Error::TooManyOccurrences(_))
        ));
    }

    /// A sparse filter makes rrule step through every rejected second inside
    /// one yield; COUNT keeps the anchor where it is. Unbounded, this took tens
    /// of seconds.
    #[test]
    fn a_sparse_sub_daily_filter_is_bounded_by_the_work_cap() {
        let ics = series(
            "DTSTART:20000101T000000Z",
            "RRULE:FREQ=SECONDLY;BYHOUR=0;BYMINUTE=0;BYSECOND=0;COUNT=1000000",
        );
        let started = std::time::Instant::now();
        let out = expand_all(
            parse(&ics),
            at(2026, 7, 6, 0),
            at(2026, 7, 7, 0),
            &ExpansionBudget::default(),
        );
        assert!(out.unwrap().is_empty());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// A date UNTIL is invalid on a timed series. Pinning it to UTC midnight
    /// would silently drop the last day, so it is left for rrule to refuse and
    /// the master is shown as before.
    #[test]
    fn a_date_until_on_a_timed_series_is_not_pinned() {
        let source = "DTSTART;TZID=Europe/London:20260706T090000\nRRULE:FREQ=WEEKLY;UNTIL=20261231";
        assert_eq!(pin_dates_to_utc(source), source);
    }
}

#[cfg(test)]
mod all_day_tests {
    use super::*;
    use crate::caldav::ical;
    use chrono::TimeZone;

    fn at(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
    }

    const BINS: &str = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:bins\r\nSUMMARY:Bin day\r\n\
DTSTART;VALUE=DATE:20260706\r\nDTEND;VALUE=DATE:20260707\r\n\
RRULE:FREQ=WEEKLY;BYDAY=MO\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    fn parse(ics: &str) -> Vec<Event> {
        ical::parse_events(ics, "Home", "/c/", "/c/e.ics", None)
    }

    /// The bug this guards: `RRuleSet` resolves a date-only `DTSTART` in the
    /// machine's local zone, so anywhere east of Greenwich every occurrence
    /// landed at 23:00 the day before — a Monday bin collection reported on
    /// Sunday, and the first Monday dropped for sorting before the window.
    #[test]
    fn all_day_occurrences_keep_their_date_regardless_of_local_zone() {
        let out = expand_all(
            parse(BINS),
            at(2026, 7, 6),
            at(2026, 7, 21),
            &ExpansionBudget::default(),
        )
        .unwrap();

        let dates: Vec<&str> = out
            .iter()
            .map(|e| e.start.date.as_deref().unwrap_or("<none>"))
            .collect();
        assert_eq!(dates, ["2026-07-06", "2026-07-13", "2026-07-20"]);
        assert!(out.iter().all(|e| e.all_day));
        // Every occurrence anchors at UTC midnight, not local midnight.
        assert!(
            out.iter()
                .all(|e| e.start.instant.unwrap().time() == chrono::NaiveTime::MIN)
        );
    }

    /// A series starting exactly on the window's first day must not be filtered
    /// out — the original symptom was three Mondays coming back as two.
    #[test]
    fn an_occurrence_on_the_first_day_of_the_window_is_kept() {
        let out = expand_all(
            parse(BINS),
            at(2026, 7, 6),
            at(2026, 7, 13),
            &ExpansionBudget::default(),
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start.date.as_deref(), Some("2026-07-06"));
    }

    /// Date-valued EXDATEs suffer the same reinterpretation, so they are pinned
    /// too — otherwise the exclusion misses the occurrence it names.
    #[test]
    fn a_date_valued_exdate_excludes_the_right_day() {
        let ics = BINS.replace(
            "RRULE:FREQ=WEEKLY;BYDAY=MO\r\n",
            "RRULE:FREQ=WEEKLY;BYDAY=MO\r\nEXDATE;VALUE=DATE:20260713\r\n",
        );
        let out = expand_all(
            parse(&ics),
            at(2026, 7, 6),
            at(2026, 7, 21),
            &ExpansionBudget::default(),
        )
        .unwrap();
        let dates: Vec<&str> = out
            .iter()
            .map(|e| e.start.date.as_deref().unwrap_or("<none>"))
            .collect();
        assert_eq!(dates, ["2026-07-06", "2026-07-20"]);
    }

    /// A timed series carries a zone on purpose, and must keep it: 09:00 stays
    /// 09:00 across the DST boundary rather than being pinned to UTC.
    #[test]
    fn a_timed_series_still_expands_in_its_own_zone() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:standup\r\nSUMMARY:Standup\r\n\
DTSTART;TZID=Europe/London:20261019T090000\r\nDTEND;TZID=Europe/London:20261019T093000\r\n\
RRULE:FREQ=WEEKLY;BYDAY=MO\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        // The window straddles the end of BST (25 October 2026).
        let out = expand_all(
            parse(ics),
            at(2026, 10, 19),
            at(2026, 11, 3),
            &ExpansionBudget::default(),
        )
        .unwrap();
        let london: Vec<String> = out
            .iter()
            .map(|e| {
                e.start
                    .instant
                    .unwrap()
                    .with_timezone(&chrono_tz::Europe::London)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .collect();
        assert_eq!(
            london,
            ["2026-10-19 09:00", "2026-10-26 09:00", "2026-11-02 09:00"],
            "local wall-clock time must survive the DST change"
        );
    }

    /// A date UNTIL on an all-day series is the RFC-correct form, and its day
    /// is the series' last.
    #[test]
    fn an_all_day_series_ends_on_its_until_date() {
        let ics = BINS.replace("BYDAY=MO", "BYDAY=MO;UNTIL=20260720");
        let out = expand_all(
            parse(&ics),
            at(2026, 7, 1),
            at(2026, 8, 1),
            &ExpansionBudget::default(),
        )
        .unwrap();
        let dates: Vec<&str> = out.iter().filter_map(|e| e.start.date.as_deref()).collect();
        assert_eq!(dates, ["2026-07-06", "2026-07-13", "2026-07-20"]);
    }

    #[test]
    fn pinning_leaves_timed_and_rule_lines_untouched() {
        let source = "DTSTART;TZID=Europe/London:20260706T090000\nRRULE:FREQ=WEEKLY;BYDAY=MO,TU;UNTIL=20260721T000000Z";
        assert_eq!(pin_dates_to_utc(source), source);
    }

    #[test]
    fn pinning_rewrites_a_date_until() {
        assert_eq!(
            pin_dates_to_utc(
                "DTSTART;VALUE=DATE:20260706\nRRULE:FREQ=WEEKLY;UNTIL=20260720;BYDAY=MO"
            ),
            "DTSTART:20260706T000000Z\nRRULE:FREQ=WEEKLY;UNTIL=20260720T000000Z;BYDAY=MO"
        );
    }

    #[test]
    fn pinning_rewrites_every_date_in_a_multi_value_line() {
        let out = pin_dates_to_utc("EXDATE;VALUE=DATE:20260713,20260720");
        assert_eq!(out, "EXDATE:20260713T000000Z,20260720T000000Z");
    }
}
