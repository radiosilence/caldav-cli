//! Minimal iCalendar (RFC 5545) reader/writer.
//!
//! Hand-rolled for the same reason the vCard handling in `fastmail-cli` is:
//! we touch a small, well-specified subset (VEVENT and VFREEBUSY) and a full
//! iCalendar library would bring far more surface than the job needs.

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;

use crate::models::{Attendee, BusyPeriod, Event, EventTime};
use crate::util;

/// A parsed content line: `NAME;PARAM=VAL:value`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentLine {
    pub name: String,
    pub params: Vec<(String, String)>,
    pub value: String,
}

impl ContentLine {
    /// First value of a parameter, case-insensitively by name.
    pub fn param(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }
}

/// Unfold per RFC 5545 §3.1: a CRLF (or LF) followed by a space or tab is a
/// fold and both the break and the single leading whitespace are removed.
pub fn unfold(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in raw.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(rest) = line.strip_prefix([' ', '\t']) {
            match out.last_mut() {
                Some(prev) => prev.push_str(rest),
                // A continuation with nothing to continue: keep it rather than
                // dropping content we don't understand.
                None => out.push(rest.to_string()),
            }
        } else {
            out.push(line.to_string());
        }
    }
    out.retain(|l| !l.is_empty());
    out
}

/// Split a content line into name, parameters, and value.
///
/// The delimiters `;` and `:` are only structural outside a quoted string —
/// `ATTENDEE;CN="Doe, Jane":mailto:jane@x.test` has a comma and a colon inside
/// quotes and a second colon inside the value.
pub fn parse_content_line(line: &str) -> Option<ContentLine> {
    let mut in_quotes = false;
    let mut colon = None;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => {
                colon = Some(i);
                break;
            }
            _ => {}
        }
    }
    let colon = colon?;
    let (head, value) = line.split_at(colon);
    let value = &value[1..];

    let mut parts = split_unquoted(head, ';');
    if parts.is_empty() {
        return None;
    }
    let name = parts.remove(0).trim().to_ascii_uppercase();
    if name.is_empty() {
        return None;
    }

    let params = parts
        .into_iter()
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            Some((
                k.trim().to_ascii_uppercase(),
                v.trim().trim_matches('"').to_string(),
            ))
        })
        .collect();

    Some(ContentLine {
        name,
        params,
        value: value.to_string(),
    })
}

/// Split on `sep`, ignoring separators inside double quotes.
fn split_unquoted(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(c);
            }
            c if c == sep && !in_quotes => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// Unescape a TEXT value per RFC 5545 §3.3.11.
pub fn unescape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') | Some('N') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some(';') => out.push(';'),
            Some(',') => out.push(','),
            // Unknown escape: keep both characters rather than eating input.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Escape a TEXT value per RFC 5545 §3.3.11. `:` is deliberately not escaped —
/// it is only structural before the value begins.
pub fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}

/// Fold a content line to the RFC 5545 §3.1 75-octet limit, breaking only on
/// character boundaries so multi-byte text survives the round trip.
pub fn fold_line(line: &str) -> String {
    const LIMIT: usize = 73; // leave room for the CRLF + leading space
    if line.len() <= LIMIT {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len() + line.len() / LIMIT * 3);
    let mut used = 0;
    for c in line.chars() {
        let width = c.len_utf8();
        if used + width > LIMIT {
            out.push_str("\r\n ");
            used = 1; // the leading space counts toward the next line
        }
        out.push(c);
        used += width;
    }
    out
}

/// Parse an ISO 8601 duration as used by iCalendar: `PT1H30M`, `P1D`, `P1DT2H`.
pub fn parse_duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    let (sign, rest) = match value.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, value.strip_prefix('+').unwrap_or(value)),
    };
    let rest = rest.strip_prefix('P')?;
    let mut total = Duration::zero();
    let mut digits = String::new();
    let mut in_time = false;
    let mut saw_unit = false;
    for c in rest.chars() {
        match c {
            'T' => in_time = true,
            '0'..='9' => digits.push(c),
            unit => {
                let n: i64 = digits.parse().ok()?;
                digits.clear();
                total += match (unit, in_time) {
                    ('W', _) => Duration::weeks(n),
                    ('D', _) => Duration::days(n),
                    ('H', true) => Duration::hours(n),
                    ('M', true) => Duration::minutes(n),
                    ('S', true) => Duration::seconds(n),
                    _ => return None,
                };
                saw_unit = true;
            }
        }
    }
    // Trailing digits with no unit, or no units at all, is malformed.
    if !digits.is_empty() || !saw_unit {
        return None;
    }
    Some(total * sign)
}

/// Interpret a DATE / DATE-TIME property value with its parameters.
pub fn parse_time(line: &ContentLine, default_tz: Tz) -> EventTime {
    let raw = line.value.trim().to_string();
    let tzid = line.param("TZID").map(str::to_string);
    let is_date = line
        .param("VALUE")
        .is_some_and(|v| v.eq_ignore_ascii_case("DATE"))
        || (raw.len() == 8 && !raw.contains('T'));

    // An unknown TZID falls back to the calendar default rather than dropping
    // the property — a slightly-off time beats a missing one.
    let tz = tzid
        .as_deref()
        .and_then(|id| id.parse::<Tz>().ok())
        .unwrap_or(default_tz);

    let instant = if is_date {
        // A DATE is not an instant. RFC 5545 gives it no timezone and it means
        // the same day everywhere, so resolving it through one moves the day
        // itself: east of Greenwich, local midnight is the previous day in UTC,
        // and the event renders a day early. Anchor at UTC midnight so the date
        // survives the round trip.
        NaiveDate::parse_from_str(&raw, "%Y%m%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .map(|naive| Utc.from_utc_datetime(&naive))
    } else if let Some(stripped) = raw.strip_suffix('Z') {
        NaiveDateTime::parse_from_str(stripped, "%Y%m%dT%H%M%S")
            .ok()
            .map(|naive| Utc.from_utc_datetime(&naive))
    } else {
        NaiveDateTime::parse_from_str(&raw, "%Y%m%dT%H%M%S")
            .ok()
            .and_then(|naive| tz.from_local_datetime(&naive).earliest())
            .map(|dt| dt.with_timezone(&Utc))
    };

    EventTime {
        date_time: instant.filter(|_| !is_date).map(util::format_rfc3339),
        date: instant.filter(|_| is_date).map(util::format_iso_date),
        tzid,
        all_day: is_date,
        raw,
        instant,
    }
}

/// Parse `mailto:jane@x.test` with `CN`/`ROLE`/`PARTSTAT` parameters.
fn parse_attendee(line: &ContentLine) -> Attendee {
    let value = line.value.trim();
    let email = value
        .strip_prefix("mailto:")
        .or_else(|| value.strip_prefix("MAILTO:"))
        .unwrap_or(value)
        .to_string();
    Attendee {
        email,
        name: line.param("CN").map(unescape_text),
        role: line.param("ROLE").map(str::to_string),
        status: line.param("PARTSTAT").map(str::to_string),
    }
}

/// Re-render a parsed content line. Used to hand the recurrence properties to
/// the `rrule` crate in the form it parses — including the `TZID` parameter,
/// without which a weekly series would drift an hour across a DST boundary.
fn render_line(line: &ContentLine) -> String {
    let mut out = String::from(&line.name);
    for (k, v) in &line.params {
        out.push(';');
        out.push_str(k);
        out.push('=');
        out.push_str(v);
    }
    out.push(':');
    out.push_str(&line.value);
    out
}

/// Split an iCalendar document into the content lines of each `BEGIN:VEVENT`
/// block, skipping nested components such as `VALARM` and `VTIMEZONE`.
fn vevent_blocks(lines: &[String]) -> Vec<Vec<ContentLine>> {
    let mut blocks = Vec::new();
    let mut current: Option<Vec<ContentLine>> = None;
    let mut nested = 0usize;

    for line in lines {
        let Some(parsed) = parse_content_line(line) else {
            continue;
        };
        match (parsed.name.as_str(), parsed.value.trim()) {
            ("BEGIN", "VEVENT") if current.is_none() => current = Some(Vec::new()),
            ("BEGIN", _) if current.is_some() => nested += 1,
            ("END", "VEVENT") if current.is_some() && nested == 0 => {
                if let Some(block) = current.take() {
                    blocks.push(block);
                }
            }
            ("END", _) if current.is_some() && nested > 0 => nested -= 1,
            _ => {
                // Properties of a nested VALARM/VTIMEZONE belong to that
                // component, not the event.
                if nested == 0
                    && let Some(block) = current.as_mut()
                {
                    block.push(parsed);
                }
            }
        }
    }
    blocks
}

/// The `X-WR-TIMEZONE` / enclosing `VTIMEZONE` id, used to anchor floating
/// times. Falls back to UTC.
fn calendar_default_tz(lines: &[String]) -> Tz {
    lines
        .iter()
        .filter_map(|l| parse_content_line(l))
        .find(|l| l.name == "X-WR-TIMEZONE")
        .and_then(|l| l.value.trim().parse::<Tz>().ok())
        .unwrap_or(Tz::UTC)
}

/// Parse every VEVENT in one `.ics` resource.
///
/// `href`/`etag` describe the resource the events came from, so callers can
/// PUT or DELETE them later without re-running discovery.
pub fn parse_events(
    ics: &str,
    calendar: &str,
    calendar_href: &str,
    href: &str,
    etag: Option<&str>,
) -> Vec<Event> {
    let lines = unfold(ics);
    let default_tz = calendar_default_tz(&lines);

    vevent_blocks(&lines)
        .into_iter()
        .filter_map(|block| build_event(block, calendar, calendar_href, href, etag, default_tz))
        .collect()
}

fn build_event(
    block: Vec<ContentLine>,
    calendar: &str,
    calendar_href: &str,
    href: &str,
    etag: Option<&str>,
    default_tz: Tz,
) -> Option<Event> {
    let mut id = String::new();
    let mut summary = None;
    let mut description = None;
    let mut location = None;
    let mut url = None;
    let mut status = None;
    let mut start = None;
    let mut end = None;
    let mut duration = None;
    let mut recurrence = None;
    let mut recurrence_id = None;
    let mut organizer = None;
    let mut attendees = Vec::new();
    let mut categories = Vec::new();
    let mut created = None;
    let mut last_modified = None;
    let mut sequence = 0u32;
    // DTSTART + RRULE/RDATE/EXDATE/EXRULE, verbatim — the input `rrule` wants.
    let mut recur_lines: Vec<String> = Vec::new();
    let mut has_rule = false;

    for line in &block {
        match line.name.as_str() {
            "UID" => id = line.value.trim().to_string(),
            "SUMMARY" => summary = Some(unescape_text(&line.value)),
            "DESCRIPTION" => description = Some(unescape_text(&line.value)),
            "LOCATION" => location = Some(unescape_text(&line.value)),
            "URL" => url = Some(line.value.trim().to_string()),
            "STATUS" => status = Some(line.value.trim().to_ascii_uppercase()),
            "DTSTART" => {
                start = Some(parse_time(line, default_tz));
                recur_lines.insert(0, render_line(line));
            }
            "DTEND" => end = Some(parse_time(line, default_tz)),
            "DURATION" => duration = parse_duration(&line.value),
            "RRULE" => {
                recurrence = Some(line.value.trim().to_string());
                recur_lines.push(render_line(line));
                has_rule = true;
            }
            "RDATE" => {
                recur_lines.push(render_line(line));
                has_rule = true;
            }
            "EXDATE" | "EXRULE" => recur_lines.push(render_line(line)),
            "RECURRENCE-ID" => recurrence_id = Some(parse_time(line, default_tz)),
            "ORGANIZER" => organizer = Some(parse_attendee(line)),
            "ATTENDEE" => attendees.push(parse_attendee(line)),
            "CATEGORIES" => categories.extend(
                split_unquoted(&line.value, ',')
                    .into_iter()
                    .map(|c| unescape_text(c.trim()))
                    .filter(|c| !c.is_empty()),
            ),
            "CREATED" => created = parse_time(line, default_tz).date_time,
            "LAST-MODIFIED" => last_modified = parse_time(line, default_tz).date_time,
            "SEQUENCE" => sequence = line.value.trim().parse().unwrap_or(0),
            _ => {}
        }
    }

    // A VEVENT without a UID can't be addressed for update or delete, so it is
    // not something we can honestly hand back as an event.
    if id.is_empty() {
        return None;
    }

    let start = start?;
    let all_day = start.all_day;

    // RFC 5545 §3.6.1: DTEND and DURATION are mutually exclusive, and an event
    // with neither ends when it starts (all-day events last one day).
    let end = end.unwrap_or_else(|| {
        let span = duration.unwrap_or_else(|| {
            if all_day {
                Duration::days(1)
            } else {
                Duration::zero()
            }
        });
        derive_end(&start, span)
    });

    Some(Event {
        id,
        calendar: calendar.to_string(),
        calendar_href: util::href_path(calendar_href),
        href: util::href_path(href),
        // Callers that addressed a specific host (the client) overwrite this
        // with the absolute URL; parsing alone only knows the path.
        resource_url: String::new(),
        etag: etag.map(str::to_string),
        summary,
        description,
        location,
        url,
        status,
        start,
        end,
        all_day,
        recurrence,
        // Only a series carries an expandable rule; a lone DTSTART is not one.
        recur_source: has_rule.then(|| recur_lines.join("\n")),
        recurrence_id: recurrence_id.and_then(|t| t.date_time.or(t.date)),
        organizer,
        attendees,
        categories,
        created,
        last_modified,
        sequence,
    })
}

/// Build the implied DTEND from DTSTART plus a span.
fn derive_end(start: &EventTime, span: Duration) -> EventTime {
    let Some(instant) = start.instant.map(|i| i + span) else {
        return start.clone();
    };
    EventTime {
        date_time: (!start.all_day).then(|| util::format_rfc3339(instant)),
        date: start.all_day.then(|| util::format_iso_date(instant)),
        tzid: start.tzid.clone(),
        all_day: start.all_day,
        raw: if start.all_day {
            util::format_ical_date(instant)
        } else {
            util::format_ical_utc(instant)
        },
        instant: Some(instant),
    }
}

/// Parse `FREEBUSY` periods out of a VFREEBUSY response.
pub fn parse_freebusy(ics: &str) -> Vec<BusyPeriod> {
    let mut out = Vec::new();
    for line in unfold(ics) {
        let Some(parsed) = parse_content_line(&line) else {
            continue;
        };
        if parsed.name != "FREEBUSY" {
            continue;
        }
        let status = parsed.param("FBTYPE").unwrap_or("BUSY").to_string();
        // The value is a comma-separated list of `start/end` or
        // `start/duration` periods.
        for period in parsed.value.split(',') {
            let Some((start_raw, end_raw)) = period.trim().split_once('/') else {
                continue;
            };
            let Some(start) = parse_utc_stamp(start_raw) else {
                continue;
            };
            let end = parse_utc_stamp(end_raw)
                .or_else(|| parse_duration(end_raw).map(|d| start + d))
                .unwrap_or(start);
            out.push(BusyPeriod {
                start: util::format_rfc3339(start),
                end: util::format_rfc3339(end),
                status: status.clone(),
            });
        }
    }
    out.sort_by(|a, b| a.start.cmp(&b.start));
    out
}

fn parse_utc_stamp(raw: &str) -> Option<DateTime<Utc>> {
    let stripped = raw.trim().strip_suffix('Z')?;
    NaiveDateTime::parse_from_str(stripped, "%Y%m%dT%H%M%S")
        .ok()
        .map(|naive| Utc.from_utc_datetime(&naive))
}

/// The property name at the head of a content line.
fn line_name(line: &str) -> &str {
    line.split([';', ':']).next().unwrap_or_default()
}

/// The recurrence properties other than `DTSTART` and `RRULE` — `EXDATE`,
/// `RDATE`, `EXRULE` — out of an [`crate::models::Event::recur_source`].
///
/// `DTSTART` and `RRULE` are excluded because they are rebuilt from the model;
/// these are not modelled at all and so have to survive as text.
pub fn recur_extra_lines(recur_source: Option<&str>) -> Vec<String> {
    recur_source
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| {
            matches!(
                line_name(line).to_ascii_uppercase().as_str(),
                "EXDATE" | "RDATE" | "EXRULE"
            )
        })
        .map(str::to_string)
        .collect()
}

/// The same recurrence source with its exclusions stripped, so expanding it
/// yields every occurrence the rule generates rather than the ones still live.
///
/// Which occurrence a user *named* and which are still standing are different
/// questions: "that one is already cancelled" is a far better answer than "no
/// such occurrence", and only an unfiltered expansion can tell them apart.
pub fn recur_source_without_exclusions(recur_source: Option<&str>) -> Option<String> {
    recur_source.map(|src| {
        src.lines()
            .filter(|line| {
                !matches!(
                    line_name(line.trim()).to_ascii_uppercase().as_str(),
                    "EXDATE" | "EXRULE"
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    })
}

/// Every instant already excluded from a series by an `EXDATE`.
///
/// One property may carry a comma-separated list, and a series may carry several
/// properties — so this flattens both rather than assuming one value each.
pub fn excluded_instants(recur_source: Option<&str>, default_tz: Tz) -> Vec<DateTime<Utc>> {
    recur_extra_lines(recur_source)
        .iter()
        .filter_map(|line| parse_content_line(line))
        .filter(|line| line.name.eq_ignore_ascii_case("EXDATE"))
        .flat_map(|line| {
            split_unquoted(&line.value, ',')
                .into_iter()
                .filter_map(|value| {
                    parse_time(
                        &ContentLine {
                            value: value.trim().to_string(),
                            ..line.clone()
                        },
                        default_tz,
                    )
                    .instant
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// How a DTSTART/DTEND should be written back out.
pub struct TimeSpec<'a> {
    pub instant: DateTime<Utc>,
    pub all_day: bool,
    /// When set, the value is written as local time with a `TZID` parameter.
    pub tzid: Option<&'a str>,
}

impl TimeSpec<'_> {
    /// Render as a full content line, e.g. `DTSTART;TZID=Europe/London:20260724T090000`.
    ///
    /// Also used for `EXDATE`, which servers only honour when its value type and
    /// `TZID` match the `DTSTART` of the series — writing both through here is
    /// what keeps them in step.
    pub fn render(&self, name: &str) -> String {
        if self.all_day {
            return format!("{name};VALUE=DATE:{}", util::format_ical_date(self.instant));
        }
        match self
            .tzid
            .and_then(|id| id.parse::<Tz>().ok().map(|tz| (id, tz)))
        {
            Some((id, tz)) => {
                let local = self.instant.with_timezone(&tz);
                format!("{name};TZID={id}:{}", local.format("%Y%m%dT%H%M%S"))
            }
            None => format!("{name}:{}", util::format_ical_utc(self.instant)),
        }
    }
}

/// Everything needed to write a VEVENT.
pub struct VEventSpec<'a> {
    pub uid: &'a str,
    pub summary: &'a str,
    pub start: TimeSpec<'a>,
    pub end: TimeSpec<'a>,
    pub description: Option<&'a str>,
    pub location: Option<&'a str>,
    pub url: Option<&'a str>,
    pub status: Option<&'a str>,
    pub recurrence: Option<&'a str>,
    /// The recurrence properties other than `RRULE` — `EXDATE`, `RDATE`,
    /// `EXRULE` — as complete content lines, written back verbatim.
    ///
    /// These carry structured values this tool doesn't model, and rebuilding a
    /// series without them silently resurrects every cancelled occurrence. Get
    /// them from [`recur_extra_lines`].
    pub recur_extra: &'a [String],
    pub attendees: &'a [Attendee],
    pub organizer: Option<&'a Attendee>,
    pub categories: &'a [String],
    pub sequence: u32,
    /// DTSTAMP — passed in rather than read from the clock so builds are
    /// reproducible and testable.
    pub stamp: DateTime<Utc>,
}

/// Build a complete VCALENDAR document wrapping one VEVENT.
pub fn build_vcalendar(spec: &VEventSpec<'_>) -> String {
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".to_string(),
        "PRODID:-//radiosilence//caldav//EN".to_string(),
        "CALSCALE:GREGORIAN".to_string(),
        "BEGIN:VEVENT".to_string(),
        format!("UID:{}", spec.uid),
        format!("DTSTAMP:{}", util::format_ical_utc(spec.stamp)),
        format!("SEQUENCE:{}", spec.sequence),
        spec.start.render("DTSTART"),
        spec.end.render("DTEND"),
        format!("SUMMARY:{}", escape_text(spec.summary)),
    ];

    if let Some(v) = spec.description.filter(|s| !s.is_empty()) {
        lines.push(format!("DESCRIPTION:{}", escape_text(v)));
    }
    if let Some(v) = spec.location.filter(|s| !s.is_empty()) {
        lines.push(format!("LOCATION:{}", escape_text(v)));
    }
    if let Some(v) = spec.url.filter(|s| !s.is_empty()) {
        lines.push(format!("URL:{v}"));
    }
    if let Some(v) = spec.status.filter(|s| !s.is_empty()) {
        lines.push(format!("STATUS:{}", v.to_ascii_uppercase()));
    }
    if let Some(v) = spec.recurrence.filter(|s| !s.is_empty()) {
        // RRULE is structured, not TEXT — it must not be escaped.
        lines.push(format!("RRULE:{}", v.trim_start_matches("RRULE:")));
    }
    // Already complete content lines, straight off the wire — not escaped, and
    // only meaningful next to the RRULE they qualify.
    lines.extend(spec.recur_extra.iter().cloned());
    if !spec.categories.is_empty() {
        let joined: Vec<String> = spec.categories.iter().map(|c| escape_text(c)).collect();
        lines.push(format!("CATEGORIES:{}", joined.join(",")));
    }
    if let Some(org) = spec.organizer {
        lines.push(render_participant("ORGANIZER", org));
    }
    for attendee in spec.attendees {
        lines.push(render_participant("ATTENDEE", attendee));
    }

    lines.push("END:VEVENT".to_string());
    lines.push("END:VCALENDAR".to_string());

    lines
        .iter()
        .map(|l| fold_line(l))
        .collect::<Vec<_>>()
        .join("\r\n")
        + "\r\n"
}

fn render_participant(name: &str, p: &Attendee) -> String {
    let mut line = String::from(name);
    if let Some(cn) = p.name.as_deref().filter(|s| !s.is_empty()) {
        // CN is quoted so commas and colons in a display name stay in-parameter.
        line.push_str(&format!(";CN=\"{}\"", cn.replace('"', "'")));
    }
    if let Some(role) = p.role.as_deref().filter(|s| !s.is_empty()) {
        line.push_str(&format!(";ROLE={role}"));
    }
    if let Some(status) = p.status.as_deref().filter(|s| !s.is_empty()) {
        line.push_str(&format!(";PARTSTAT={status}"));
    }
    line.push_str(&format!(":mailto:{}", p.email));
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:evt-1\r\n\
SUMMARY:Standup\r\n\
DTSTART;TZID=Europe/London:20260724T090000\r\n\
DTEND;TZID=Europe/London:20260724T093000\r\n\
LOCATION:Room 4\r\n\
DESCRIPTION:Daily sync\\, all welcome\r\n\
ATTENDEE;CN=\"Doe, Jane\";ROLE=REQ-PARTICIPANT;PARTSTAT=ACCEPTED:mailto:jane@x.test\r\n\
BEGIN:VALARM\r\n\
ACTION:DISPLAY\r\n\
SUMMARY:Ignore me\r\n\
END:VALARM\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    fn parse_one(ics: &str) -> Event {
        let events = parse_events(
            ics,
            "Home",
            "/cal/home/",
            "/cal/home/evt-1.ics",
            Some("W/1"),
        );
        assert_eq!(events.len(), 1, "expected exactly one event");
        events.into_iter().next().unwrap()
    }

    #[test]
    fn unfolds_continuation_lines() {
        let raw = "SUMMARY:Very long\r\n  title\r\nUID:x";
        assert_eq!(unfold(raw), vec!["SUMMARY:Very long title", "UID:x"]);
    }

    #[test]
    fn unfolds_tab_continuations_and_bare_lf() {
        let raw = "SUMMARY:a\n\tb\nUID:x";
        assert_eq!(unfold(raw), vec!["SUMMARY:ab", "UID:x"]);
    }

    #[test]
    fn parses_params_with_quoted_delimiters() {
        let line = parse_content_line("ATTENDEE;CN=\"Doe, Jane\";ROLE=CHAIR:mailto:j@x.test")
            .expect("parses");
        assert_eq!(line.name, "ATTENDEE");
        assert_eq!(line.param("CN"), Some("Doe, Jane"));
        assert_eq!(line.param("ROLE"), Some("CHAIR"));
        // The colon inside the value must survive.
        assert_eq!(line.value, "mailto:j@x.test");
    }

    #[test]
    fn parses_event_with_tzid() {
        let event = parse_one(SAMPLE);
        assert_eq!(event.id, "evt-1");
        assert_eq!(event.summary.as_deref(), Some("Standup"));
        assert_eq!(event.location.as_deref(), Some("Room 4"));
        // Escaped comma is unescaped.
        assert_eq!(
            event.description.as_deref(),
            Some("Daily sync, all welcome")
        );
        // 09:00 BST is 08:00Z.
        assert_eq!(
            event.start.date_time.as_deref(),
            Some("2026-07-24T08:00:00Z")
        );
        assert_eq!(event.end.date_time.as_deref(), Some("2026-07-24T08:30:00Z"));
        assert_eq!(event.start.tzid.as_deref(), Some("Europe/London"));
        assert!(!event.all_day);
        assert_eq!(event.etag.as_deref(), Some("W/1"));
    }

    #[test]
    fn ignores_nested_valarm_properties() {
        // The VALARM has its own SUMMARY; the event's must win.
        assert_eq!(parse_one(SAMPLE).summary.as_deref(), Some("Standup"));
    }

    #[test]
    fn parses_attendees_with_params() {
        let event = parse_one(SAMPLE);
        assert_eq!(event.attendees.len(), 1);
        let a = &event.attendees[0];
        assert_eq!(a.email, "jane@x.test");
        assert_eq!(a.name.as_deref(), Some("Doe, Jane"));
        assert_eq!(a.status.as_deref(), Some("ACCEPTED"));
    }

    #[test]
    fn parses_all_day_event() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:h\r\nSUMMARY:Holiday\r\n\
DTSTART;VALUE=DATE:20260724\r\nDTEND;VALUE=DATE:20260725\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let event = parse_one(ics);
        assert!(event.all_day);
        assert_eq!(event.start.date.as_deref(), Some("2026-07-24"));
        assert_eq!(event.end.date.as_deref(), Some("2026-07-25"));
        assert!(event.start.date_time.is_none());
    }

    #[test]
    fn derives_end_from_duration() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:d\r\nSUMMARY:Call\r\n\
DTSTART:20260724T090000Z\r\nDURATION:PT1H30M\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let event = parse_one(ics);
        assert_eq!(event.end.date_time.as_deref(), Some("2026-07-24T10:30:00Z"));
    }

    #[test]
    fn all_day_event_without_end_lasts_one_day() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:a\r\nSUMMARY:Day off\r\n\
DTSTART;VALUE=DATE:20260724\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let event = parse_one(ics);
        assert_eq!(event.end.date.as_deref(), Some("2026-07-25"));
    }

    #[test]
    fn skips_events_without_uid() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nSUMMARY:Anonymous\r\n\
DTSTART:20260724T090000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(parse_events(ics, "Home", "/c/", "/c/x.ics", None).is_empty());
    }

    #[test]
    fn parses_multiple_events_in_one_resource() {
        let ics = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\nUID:m1\r\nDTSTART:20260724T090000Z\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:m2\r\nDTSTART:20260725T090000Z\r\nRECURRENCE-ID:20260725T090000Z\r\nEND:VEVENT\r\n\
END:VCALENDAR\r\n";
        let events = parse_events(ics, "Home", "/c/", "/c/m.ics", None);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[1].recurrence_id.as_deref(),
            Some("2026-07-25T09:00:00Z")
        );
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("PT1H30M"), Some(Duration::minutes(90)));
        assert_eq!(parse_duration("P1D"), Some(Duration::days(1)));
        assert_eq!(parse_duration("P1DT2H"), Some(Duration::hours(26)));
        assert_eq!(parse_duration("-PT15M"), Some(Duration::minutes(-15)));
        assert_eq!(parse_duration("P2W"), Some(Duration::weeks(2)));
        assert_eq!(parse_duration("PT"), None);
        assert_eq!(parse_duration("P1"), None);
        assert_eq!(parse_duration("garbage"), None);
    }

    #[test]
    fn escapes_and_unescapes_text_round_trip() {
        let original = "Semi; comma, backslash \\ and\nnewline";
        assert_eq!(unescape_text(&escape_text(original)), original);
    }

    #[test]
    fn escape_leaves_colons_alone() {
        assert_eq!(escape_text("https://x.test"), "https://x.test");
    }

    #[test]
    fn folds_long_lines_on_char_boundaries() {
        let long = format!("SUMMARY:{}", "é".repeat(80));
        let folded = fold_line(&long);
        assert!(folded.contains("\r\n "));
        // Unfolding must reproduce the original exactly.
        assert_eq!(unfold(&folded), vec![long]);
    }

    #[test]
    fn short_lines_are_not_folded() {
        assert_eq!(fold_line("UID:x"), "UID:x");
    }

    #[test]
    fn builds_and_reparses_an_event() {
        let stamp = Utc.with_ymd_and_hms(2026, 7, 20, 12, 0, 0).unwrap();
        let attendees = vec![Attendee {
            email: "jane@x.test".into(),
            name: Some("Doe, Jane".into()),
            role: Some("REQ-PARTICIPANT".into()),
            status: Some("NEEDS-ACTION".into()),
        }];
        let spec = VEventSpec {
            uid: "built-1",
            summary: "Plan; the, thing",
            start: TimeSpec {
                instant: Utc.with_ymd_and_hms(2026, 7, 24, 8, 0, 0).unwrap(),
                all_day: false,
                tzid: Some("Europe/London"),
            },
            end: TimeSpec {
                instant: Utc.with_ymd_and_hms(2026, 7, 24, 9, 0, 0).unwrap(),
                all_day: false,
                tzid: Some("Europe/London"),
            },
            description: Some("Line one\nline two"),
            location: Some("Room 4"),
            url: Some("https://x.test/e"),
            status: Some("confirmed"),
            recurrence: Some("FREQ=WEEKLY;BYDAY=MO"),
            recur_extra: &[],
            attendees: &attendees,
            organizer: None,
            categories: &["work".to_string(), "planning".to_string()],
            sequence: 3,
            stamp,
        };
        let ics = build_vcalendar(&spec);

        // Local time is written with the TZID, not converted to UTC.
        assert!(ics.contains("DTSTART;TZID=Europe/London:20260724T090000"));
        assert!(ics.contains("STATUS:CONFIRMED"));
        assert!(ics.contains("RRULE:FREQ=WEEKLY;BYDAY=MO"));
        assert!(ics.contains("SEQUENCE:3"));

        let event = parse_one(&ics);
        assert_eq!(event.id, "built-1");
        assert_eq!(event.summary.as_deref(), Some("Plan; the, thing"));
        assert_eq!(event.description.as_deref(), Some("Line one\nline two"));
        assert_eq!(
            event.start.date_time.as_deref(),
            Some("2026-07-24T08:00:00Z")
        );
        assert_eq!(event.categories, vec!["work", "planning"]);
        assert_eq!(event.attendees[0].name.as_deref(), Some("Doe, Jane"));
        assert_eq!(event.recurrence.as_deref(), Some("FREQ=WEEKLY;BYDAY=MO"));
        assert_eq!(event.sequence, 3);
    }

    #[test]
    fn builds_all_day_events_as_date_values() {
        let stamp = Utc.with_ymd_and_hms(2026, 7, 20, 12, 0, 0).unwrap();
        let spec = VEventSpec {
            uid: "allday",
            summary: "Holiday",
            start: TimeSpec {
                instant: Utc.with_ymd_and_hms(2026, 7, 24, 0, 0, 0).unwrap(),
                all_day: true,
                tzid: None,
            },
            end: TimeSpec {
                instant: Utc.with_ymd_and_hms(2026, 7, 25, 0, 0, 0).unwrap(),
                all_day: true,
                tzid: None,
            },
            description: None,
            location: None,
            url: None,
            status: None,
            recurrence: None,
            recur_extra: &[],
            attendees: &[],
            organizer: None,
            categories: &[],
            sequence: 0,
            stamp,
        };
        let ics = build_vcalendar(&spec);
        assert!(ics.contains("DTSTART;VALUE=DATE:20260724"));
        assert!(ics.contains("DTEND;VALUE=DATE:20260725"));
        assert!(parse_one(&ics).all_day);
    }

    #[test]
    fn parses_freebusy_periods() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VFREEBUSY\r\n\
FREEBUSY;FBTYPE=BUSY:20260724T090000Z/20260724T100000Z,20260724T140000Z/PT30M\r\n\
END:VFREEBUSY\r\nEND:VCALENDAR\r\n";
        let periods = parse_freebusy(ics);
        assert_eq!(periods.len(), 2);
        assert_eq!(periods[0].start, "2026-07-24T09:00:00Z");
        assert_eq!(periods[0].end, "2026-07-24T10:00:00Z");
        assert_eq!(periods[0].status, "BUSY");
        // Second period uses start/duration form.
        assert_eq!(periods[1].end, "2026-07-24T14:30:00Z");
    }

    /// A series with two kinds of exception on it, plus an unmodelled property.
    const WITH_EXCEPTIONS: &str = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:s\r\n\
SUMMARY:Standup\r\nDTSTART;TZID=Europe/London:20260724T090000\r\n\
DTEND;TZID=Europe/London:20260724T093000\r\nRRULE:FREQ=DAILY\r\n\
EXDATE;TZID=Europe/London:20260727T090000,20260728T090000\r\n\
RDATE;TZID=Europe/London:20260801T140000\r\nEND:VEVENT\r\nEND:VCALENDAR";

    #[test]
    fn recurrence_extras_are_kept_and_dtstart_and_rrule_are_not() {
        let event = parse_one(WITH_EXCEPTIONS);
        let extras = recur_extra_lines(event.recur_source.as_deref());

        // DTSTART and RRULE are rebuilt from the model; re-emitting them here
        // would duplicate them in the output.
        assert_eq!(extras.len(), 2, "{extras:?}");
        assert!(
            extras
                .iter()
                .any(|l| l.starts_with("EXDATE;TZID=Europe/London:"))
        );
        assert!(
            extras
                .iter()
                .any(|l| l.starts_with("RDATE;TZID=Europe/London:"))
        );
        assert!(!extras.iter().any(|l| l.starts_with("DTSTART")));
        assert!(!extras.iter().any(|l| l.starts_with("RRULE")));
    }

    #[test]
    fn a_comma_separated_exdate_yields_every_instant_in_it() {
        let event = parse_one(WITH_EXCEPTIONS);
        let excluded = excluded_instants(event.recur_source.as_deref(), Tz::UTC);

        // One property, two values — reading only the first would let a
        // re-cancel through as if it were new.
        assert_eq!(excluded.len(), 2);
        assert_eq!(util::format_rfc3339(excluded[0]), "2026-07-27T08:00:00Z");
        assert_eq!(util::format_rfc3339(excluded[1]), "2026-07-28T08:00:00Z");
    }

    #[test]
    fn stripping_exclusions_keeps_the_rule_and_the_extra_dates() {
        let event = parse_one(WITH_EXCEPTIONS);
        let stripped = recur_source_without_exclusions(event.recur_source.as_deref()).unwrap();

        assert!(!stripped.contains("EXDATE"));
        // RDATE adds occurrences rather than removing them, so it stays.
        assert!(stripped.contains("RDATE"));
        assert!(stripped.contains("RRULE:FREQ=DAILY"));
        assert!(stripped.contains("DTSTART"));
    }

    #[test]
    fn a_built_event_round_trips_its_exceptions() {
        let event = parse_one(WITH_EXCEPTIONS);
        let extras = recur_extra_lines(event.recur_source.as_deref());
        let ics = build_vcalendar(&VEventSpec {
            uid: "s",
            summary: "Standup",
            start: TimeSpec {
                instant: event.start.instant.unwrap(),
                all_day: false,
                tzid: Some("Europe/London"),
            },
            end: TimeSpec {
                instant: event.end.instant.unwrap(),
                all_day: false,
                tzid: Some("Europe/London"),
            },
            description: None,
            location: None,
            url: None,
            status: None,
            recurrence: Some("FREQ=DAILY"),
            recur_extra: &extras,
            attendees: &[],
            organizer: None,
            categories: &[],
            sequence: 1,
            stamp: Utc.with_ymd_and_hms(2026, 7, 20, 12, 0, 0).unwrap(),
        });

        // Re-reading what we wrote must yield the same exclusions we started
        // with — this is the round trip an update depends on.
        let reparsed = parse_one(&ics);
        assert_eq!(
            excluded_instants(reparsed.recur_source.as_deref(), Tz::UTC),
            excluded_instants(event.recur_source.as_deref(), Tz::UTC)
        );
        assert_eq!(ics.matches("RRULE").count(), 1);
        assert_eq!(ics.matches("DTSTART").count(), 1);
    }
}
