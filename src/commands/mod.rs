mod auth;
mod calendars;
mod create;
mod delete;
mod freebusy;
mod get;
mod list;
mod search;
mod update;

pub use auth::*;
pub use calendars::*;
pub use create::*;
pub use delete::*;
pub use freebusy::*;
pub use get::*;
pub use list::*;
pub use search::*;
pub use update::*;

use chrono::{DateTime, Utc};

use crate::caldav::CalDavClient;
use crate::config::Config;
use crate::error::Result;
use crate::util;

/// Build a client from config/env. Shared by every command.
pub(crate) fn make_client() -> Result<CalDavClient> {
    let config = Config::load()?;
    Ok(CalDavClient::new(
        config.get_server_url(),
        config.get_username()?,
        config.get_app_password()?,
    ))
}

/// The time window a read command operates over.
///
/// Explicit `start`/`end` win; otherwise it's `days` (default 7) starting from
/// today in `tz`. Every field accepts the formats [`crate::util::parse_datetime`]
/// understands, so `--start tomorrow --end +2w` works.
#[derive(Debug, Clone, Default)]
pub struct RangeArgs {
    pub start: Option<String>,
    pub end: Option<String>,
    pub days: Option<i64>,
    pub tz: Option<String>,
}

/// Default lookahead when neither an explicit end nor a day count is given.
const DEFAULT_DAYS: i64 = 7;

impl RangeArgs {
    pub fn resolve(&self) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
        self.resolve_at(Utc::now())
    }

    /// [`RangeArgs::resolve`] with an injectable "now", so it can be tested.
    pub fn resolve_at(&self, now: DateTime<Utc>) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
        let tz = util::resolve_tz(self.tz.as_deref())?;
        let tzid = self.tz.as_deref();

        let start = match &self.start {
            Some(raw) => util::parse_datetime_at(raw, tzid, now)?.instant,
            None => util::day_range(1, tz, now)?.0,
        };

        let end = match (&self.end, self.days) {
            (Some(raw), _) => util::parse_datetime_at(raw, tzid, now)?.instant,
            (None, Some(days)) => start + chrono::Duration::days(days.max(1)),
            (None, None) => start + chrono::Duration::days(DEFAULT_DAYS),
        };

        Ok((start, end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 24, 12, 0, 0).unwrap()
    }

    #[test]
    fn defaults_to_a_week_from_today() {
        let (start, end) = RangeArgs::default().resolve_at(now()).unwrap();
        assert_eq!(util::format_rfc3339(start), "2026-07-24T00:00:00Z");
        assert_eq!(util::format_rfc3339(end), "2026-07-31T00:00:00Z");
    }

    #[test]
    fn days_counts_forward_from_the_start() {
        let args = RangeArgs {
            days: Some(2),
            ..Default::default()
        };
        let (start, end) = args.resolve_at(now()).unwrap();
        assert_eq!(util::format_iso_date(start), "2026-07-24");
        assert_eq!(util::format_iso_date(end), "2026-07-26");
    }

    #[test]
    fn explicit_end_beats_days() {
        let args = RangeArgs {
            end: Some("2026-08-01".into()),
            days: Some(2),
            ..Default::default()
        };
        let (_, end) = args.resolve_at(now()).unwrap();
        assert_eq!(util::format_iso_date(end), "2026-08-01");
    }

    #[test]
    fn accepts_relative_bounds() {
        let args = RangeArgs {
            start: Some("tomorrow".into()),
            end: Some("+1w".into()),
            ..Default::default()
        };
        let (start, end) = args.resolve_at(now()).unwrap();
        assert_eq!(util::format_iso_date(start), "2026-07-25");
        assert_eq!(util::format_iso_date(end), "2026-07-31");
    }

    #[test]
    fn start_is_the_users_midnight_not_utcs() {
        let args = RangeArgs {
            tz: Some("Asia/Tokyo".into()),
            days: Some(1),
            ..Default::default()
        };
        let (start, _) = args.resolve_at(now()).unwrap();
        // Midnight on the 24th in Tokyo is 15:00Z on the 23rd.
        assert_eq!(util::format_rfc3339(start), "2026-07-23T15:00:00Z");
    }

    #[test]
    fn zero_days_still_covers_one_day() {
        let args = RangeArgs {
            days: Some(0),
            ..Default::default()
        };
        let (start, end) = args.resolve_at(now()).unwrap();
        assert_eq!(end - start, chrono::Duration::days(1));
    }
}
