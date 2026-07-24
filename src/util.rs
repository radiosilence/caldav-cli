//! Date/time parsing and URL helpers shared by the CLI, client, and GraphQL layer.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;

use crate::error::{Error, Result};

/// A user-supplied time resolved to an instant, plus how it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTime {
    pub instant: DateTime<Utc>,
    /// True when the input named a day with no time — an all-day value.
    pub date_only: bool,
    /// The zone a naive input was interpreted in. `None` when the input
    /// carried its own offset (`Z` or `+01:00`) and needed no assumption.
    pub tzid: Option<String>,
}

/// Naive datetime layouts we accept, most specific first.
const NAIVE_DATETIME_FORMATS: &[&str] = &[
    "%Y-%m-%dT%H:%M:%S",
    "%Y-%m-%dT%H:%M",
    "%Y-%m-%d %H:%M:%S",
    "%Y-%m-%d %H:%M",
    "%Y%m%dT%H%M%S",
];

const DATE_FORMATS: &[&str] = &["%Y-%m-%d", "%Y%m%d", "%d/%m/%Y"];

/// Resolve a timezone id, defaulting to UTC. An unknown id is an error rather
/// than a silent fallback — silently shifting an event by hours is worse than
/// refusing it.
pub fn resolve_tz(tzid: Option<&str>) -> Result<Tz> {
    match tzid.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(Tz::UTC),
        Some(id) => id.parse::<Tz>().map_err(|_| Error::InvalidDateTime {
            input: id.to_string(),
            reason: "unknown timezone id (expected an IANA name like Europe/London)".into(),
        }),
    }
}

/// Parse a user-supplied date/time. Accepts, in order:
///
/// - keywords: `now`, `today`, `tomorrow`, `yesterday`
/// - relative offsets: `+90m`, `-2h`, `+3d`, `+1w`
/// - RFC 3339 with an offset: `2026-07-24T09:00:00Z`
/// - naive date-times: `2026-07-24 09:00`, `20260724T090000`
/// - bare dates: `2026-07-24`, `20260724`, `24/07/2026`
///
/// Naive inputs are interpreted in `tzid` (UTC when absent).
pub fn parse_datetime(input: &str, tzid: Option<&str>) -> Result<ParsedTime> {
    parse_datetime_at(input, tzid, Utc::now())
}

/// [`parse_datetime`] with an injectable "now", so relative inputs are testable.
pub fn parse_datetime_at(
    input: &str,
    tzid: Option<&str>,
    now: DateTime<Utc>,
) -> Result<ParsedTime> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(Error::InvalidDateTime {
            input: input.to_string(),
            reason: "empty value".into(),
        });
    }
    let tz = resolve_tz(tzid)?;
    let tz_name = tzid
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    // Keywords and relative offsets are resolved against `now` in `tz`, so
    // "today" means the user's today, not UTC's.
    let lower = raw.to_ascii_lowercase();
    let local_now = now.with_timezone(&tz);
    match lower.as_str() {
        "now" => {
            return Ok(ParsedTime {
                instant: now,
                date_only: false,
                tzid: tz_name,
            });
        }
        "today" | "tomorrow" | "yesterday" => {
            let offset = match lower.as_str() {
                "tomorrow" => 1,
                "yesterday" => -1,
                _ => 0,
            };
            let day = local_now.date_naive() + Duration::days(offset);
            return Ok(ParsedTime {
                instant: start_of_day(day, tz)?,
                date_only: true,
                tzid: tz_name,
            });
        }
        _ => {}
    }

    if let Some(delta) = parse_relative(&lower) {
        return Ok(ParsedTime {
            instant: now + delta,
            date_only: false,
            tzid: tz_name,
        });
    }

    // Inputs that carry their own offset need no zone assumption.
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Ok(ParsedTime {
            instant: dt.with_timezone(&Utc),
            date_only: false,
            tzid: None,
        });
    }
    // iCal UTC form: 20260724T090000Z
    if let Some(stripped) = raw.strip_suffix('Z')
        && let Ok(naive) = NaiveDateTime::parse_from_str(stripped, "%Y%m%dT%H%M%S")
    {
        return Ok(ParsedTime {
            instant: Utc.from_utc_datetime(&naive),
            date_only: false,
            tzid: None,
        });
    }

    for fmt in NAIVE_DATETIME_FORMATS {
        if let Ok(naive) = NaiveDateTime::parse_from_str(raw, fmt) {
            return Ok(ParsedTime {
                instant: from_local(naive, tz, raw)?,
                date_only: false,
                tzid: tz_name,
            });
        }
    }

    for fmt in DATE_FORMATS {
        if let Ok(date) = NaiveDate::parse_from_str(raw, fmt) {
            return Ok(ParsedTime {
                instant: start_of_day(date, tz)?,
                date_only: true,
                tzid: tz_name,
            });
        }
    }

    Err(Error::InvalidDateTime {
        input: input.to_string(),
        reason: "expected ISO 8601 (2026-07-24T09:00:00Z), 'YYYY-MM-DD [HH:MM]', \
                 a keyword (now/today/tomorrow), or a relative offset (+2h, +3d)"
            .into(),
    })
}

/// `+90m`, `-2h`, `+3d`, `+1w`. Returns `None` if the shape doesn't match.
fn parse_relative(input: &str) -> Option<Duration> {
    let (sign, rest) = match input.strip_prefix('+') {
        Some(rest) => (1, rest),
        None => (-1, input.strip_prefix('-')?),
    };
    let (digits, unit) = rest.split_at(rest.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = digits.parse().ok()?;
    let magnitude = match unit {
        "m" | "min" | "mins" => Duration::minutes(n),
        "h" | "hr" | "hrs" => Duration::hours(n),
        "d" | "day" | "days" => Duration::days(n),
        "w" | "wk" | "weeks" => Duration::weeks(n),
        _ => return None,
    };
    Some(magnitude * sign)
}

/// Midnight on `date` in `tz`, as a UTC instant.
fn start_of_day(date: NaiveDate, tz: Tz) -> Result<DateTime<Utc>> {
    let naive = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| Error::InvalidDateTime {
            input: date.to_string(),
            reason: "not a representable date".into(),
        })?;
    from_local(naive, tz, &date.to_string())
}

/// Interpret a naive local time in `tz`. DST gaps have no valid instant; we
/// take the earliest instant after the gap rather than failing the whole call.
fn from_local(naive: NaiveDateTime, tz: Tz, input: &str) -> Result<DateTime<Utc>> {
    match tz.from_local_datetime(&naive).earliest() {
        Some(dt) => Ok(dt.with_timezone(&Utc)),
        // Spring-forward gap: this wall-clock time doesn't exist. Step forward
        // an hour, which lands just past the transition.
        None => tz
            .from_local_datetime(&(naive + Duration::hours(1)))
            .earliest()
            .map(|dt| dt.with_timezone(&Utc))
            .ok_or_else(|| Error::InvalidDateTime {
                input: input.to_string(),
                reason: format!("no valid instant in timezone {tz}"),
            }),
    }
}

/// iCalendar UTC form: `20260724T090000Z`.
pub fn format_ical_utc(dt: DateTime<Utc>) -> String {
    dt.format("%Y%m%dT%H%M%SZ").to_string()
}

/// iCalendar `VALUE=DATE` form: `20260724`.
pub fn format_ical_date(dt: DateTime<Utc>) -> String {
    dt.format("%Y%m%d").to_string()
}

/// `YYYY-MM-DD` for JSON output.
pub fn format_iso_date(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%d").to_string()
}

/// RFC 3339 in UTC, seconds precision — the timestamp form used in JSON output.
pub fn format_rfc3339(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// The scheme+authority of `base`, e.g. `https://caldav.icloud.com`.
pub fn origin(base: &str) -> String {
    let base = base.trim_end_matches('/');
    match base.find("://") {
        Some(i) => {
            let after = &base[i + 3..];
            match after.find('/') {
                Some(j) => base[..i + 3 + j].to_string(),
                None => base.to_string(),
            }
        }
        None => base.to_string(),
    }
}

/// Turn an href from a DAV response into an absolute URL. Servers return
/// either an absolute URL or a rooted path; both are common in the wild.
pub fn resolve_url(base: &str, href: &str) -> String {
    let href = href.trim();
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    let origin = origin(base);
    if href.starts_with('/') {
        format!("{origin}{href}")
    } else {
        format!("{origin}/{href}")
    }
}

/// The path component of an href, so hrefs from different servers compare and
/// store uniformly (we always keep paths, never absolute URLs).
pub fn href_path(href: &str) -> String {
    let href = href.trim();
    match href.find("://") {
        Some(i) => {
            let after = &href[i + 3..];
            match after.find('/') {
                Some(j) => after[j..].to_string(),
                None => "/".to_string(),
            }
        }
        None => href.to_string(),
    }
}

/// Last non-empty path segment — used as a calendar's short id.
pub fn last_segment(href: &str) -> String {
    href_path(href)
        .split('/')
        .rfind(|s| !s.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Split a comma-separated list, dropping empties. Used for attendees,
/// categories, and other repeatable CLI flags.
pub fn split_list(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The UTC day range `[start_of_today, start_of_today + days)` in `tz`.
pub fn day_range(days: i64, tz: Tz, now: DateTime<Utc>) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    let today = now.with_timezone(&tz).date_naive();
    let start = start_of_day(today, tz)?;
    let end = start_of_day(today + Duration::days(days.max(1)), tz)?;
    Ok((start, end))
}

/// The UTC range covering the calendar month containing `now` in `tz`.
pub fn month_range(tz: Tz, now: DateTime<Utc>) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    let today = now.with_timezone(&tz).date_naive();
    let first = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).ok_or_else(|| {
        Error::InvalidDateTime {
            input: today.to_string(),
            reason: "not a representable month".into(),
        }
    })?;
    let next = if today.month() == 12 {
        NaiveDate::from_ymd_opt(today.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(today.year(), today.month() + 1, 1)
    }
    .ok_or_else(|| Error::InvalidDateTime {
        input: today.to_string(),
        reason: "not a representable month".into(),
    })?;
    Ok((start_of_day(first, tz)?, start_of_day(next, tz)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        // Friday 2026-07-24 12:00:00Z
        Utc.with_ymd_and_hms(2026, 7, 24, 12, 0, 0).unwrap()
    }

    #[test]
    fn parses_rfc3339_with_offset() {
        let p = parse_datetime_at("2026-07-24T09:00:00+01:00", None, now()).unwrap();
        assert_eq!(format_rfc3339(p.instant), "2026-07-24T08:00:00Z");
        assert!(!p.date_only);
        // Input carried its own offset — no zone was assumed.
        assert_eq!(p.tzid, None);
    }

    #[test]
    fn parses_ical_utc_form() {
        let p = parse_datetime_at("20260724T090000Z", None, now()).unwrap();
        assert_eq!(format_rfc3339(p.instant), "2026-07-24T09:00:00Z");
    }

    #[test]
    fn parses_naive_datetime_in_given_zone() {
        let p = parse_datetime_at("2026-07-24 09:00", Some("Europe/London"), now()).unwrap();
        // BST in July: 09:00 local is 08:00Z.
        assert_eq!(format_rfc3339(p.instant), "2026-07-24T08:00:00Z");
        assert_eq!(p.tzid.as_deref(), Some("Europe/London"));
    }

    #[test]
    fn parses_bare_date_as_all_day() {
        let p = parse_datetime_at("2026-07-24", None, now()).unwrap();
        assert!(p.date_only);
        assert_eq!(format_ical_date(p.instant), "20260724");
    }

    #[test]
    fn parses_keywords_relative_to_now() {
        let today = parse_datetime_at("today", None, now()).unwrap();
        assert_eq!(format_iso_date(today.instant), "2026-07-24");
        assert!(today.date_only);

        let tomorrow = parse_datetime_at("TOMORROW", None, now()).unwrap();
        assert_eq!(format_iso_date(tomorrow.instant), "2026-07-25");

        let yesterday = parse_datetime_at("yesterday", None, now()).unwrap();
        assert_eq!(format_iso_date(yesterday.instant), "2026-07-23");
    }

    #[test]
    fn today_is_the_users_today_not_utcs() {
        // 23:30Z on the 24th is already the 25th in Tokyo (+09:00).
        let late = Utc.with_ymd_and_hms(2026, 7, 24, 23, 30, 0).unwrap();
        let p = parse_datetime_at("today", Some("Asia/Tokyo"), late).unwrap();
        // Midnight Tokyo on the 25th is 15:00Z on the 24th.
        assert_eq!(format_rfc3339(p.instant), "2026-07-24T15:00:00Z");
    }

    #[test]
    fn parses_relative_offsets() {
        assert_eq!(
            format_rfc3339(parse_datetime_at("+2h", None, now()).unwrap().instant),
            "2026-07-24T14:00:00Z"
        );
        assert_eq!(
            format_rfc3339(parse_datetime_at("-30m", None, now()).unwrap().instant),
            "2026-07-24T11:30:00Z"
        );
        assert_eq!(
            format_iso_date(parse_datetime_at("+1w", None, now()).unwrap().instant),
            "2026-07-31"
        );
    }

    #[test]
    fn rejects_garbage_and_unknown_zones() {
        assert!(parse_datetime_at("next tuesday-ish", None, now()).is_err());
        assert!(parse_datetime_at("", None, now()).is_err());
        assert!(parse_datetime_at("2026-07-24", Some("Mars/Olympus"), now()).is_err());
    }

    #[test]
    fn dst_gap_resolves_forward_instead_of_failing() {
        // 2026-03-29 01:30 doesn't exist in Europe/London (clocks jump 01:00→02:00).
        let p = parse_datetime_at("2026-03-29 01:30", Some("Europe/London"), now()).unwrap();
        assert_eq!(format_rfc3339(p.instant), "2026-03-29T01:30:00Z");
    }

    #[test]
    fn origin_strips_path() {
        assert_eq!(
            origin("https://caldav.icloud.com/1234/calendars/"),
            "https://caldav.icloud.com"
        );
        assert_eq!(
            origin("https://caldav.icloud.com"),
            "https://caldav.icloud.com"
        );
    }

    #[test]
    fn resolve_url_handles_both_href_shapes() {
        assert_eq!(
            resolve_url("https://caldav.icloud.com", "/1234/calendars/home/"),
            "https://caldav.icloud.com/1234/calendars/home/"
        );
        assert_eq!(
            resolve_url(
                "https://caldav.icloud.com",
                "https://p42.icloud.com/1234/x/"
            ),
            "https://p42.icloud.com/1234/x/"
        );
    }

    #[test]
    fn href_path_normalises_absolute_urls() {
        assert_eq!(
            href_path("https://caldav.icloud.com/1234/calendars/home/"),
            "/1234/calendars/home/"
        );
        assert_eq!(href_path("/1234/calendars/home/"), "/1234/calendars/home/");
    }

    #[test]
    fn last_segment_ignores_trailing_slash() {
        assert_eq!(last_segment("/1234/calendars/home/"), "home");
        assert_eq!(last_segment("https://x.test/a/b/c.ics"), "c.ics");
    }

    #[test]
    fn day_range_covers_requested_days() {
        let (start, end) = day_range(3, Tz::UTC, now()).unwrap();
        assert_eq!(format_iso_date(start), "2026-07-24");
        assert_eq!(format_iso_date(end), "2026-07-27");
    }

    #[test]
    fn day_range_treats_zero_as_one_day() {
        let (start, end) = day_range(0, Tz::UTC, now()).unwrap();
        assert_eq!(format_iso_date(start), "2026-07-24");
        assert_eq!(format_iso_date(end), "2026-07-25");
    }

    #[test]
    fn month_range_spans_the_calendar_month() {
        let (start, end) = month_range(Tz::UTC, now()).unwrap();
        assert_eq!(format_iso_date(start), "2026-07-01");
        assert_eq!(format_iso_date(end), "2026-08-01");
    }

    #[test]
    fn month_range_rolls_over_december() {
        let dec = Utc.with_ymd_and_hms(2026, 12, 14, 9, 0, 0).unwrap();
        let (start, end) = month_range(Tz::UTC, dec).unwrap();
        assert_eq!(format_iso_date(start), "2026-12-01");
        assert_eq!(format_iso_date(end), "2027-01-01");
    }

    #[test]
    fn split_list_trims_and_drops_empties() {
        assert_eq!(split_list("a, b ,,c "), vec!["a", "b", "c"]);
        assert!(split_list("  ").is_empty());
    }
}
