//! CalDAV client (RFC 4791).
//!
//! Uses raw HTTP with reqwest, since CalDAV is WebDAV plus iCalendar — the
//! same shape as the CardDAV client in `fastmail-cli`. Discovery walks
//! `current-user-principal` → `calendar-home-set` → the calendar collections,
//! which is the portable path that works on iCloud, Fastmail, Google, and
//! Nextcloud alike.

pub mod ical;

use chrono::{DateTime, Duration, Utc};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use reqwest::Client;
use tokio::sync::OnceCell;
use tracing::{debug, instrument};

use crate::error::{Error, Result};
use crate::models::{BusyPeriod, Calendar, Event, EventFields};
use crate::util;

use ical::{TimeSpec, VEventSpec};

const DAV_NS: &str = "DAV:";
const CALDAV_NS: &str = "urn:ietf:params:xml:ns:caldav";
const APPLE_NS: &str = "http://apple.com/ns/ical/";

/// Per RFC 3986, these chars need escaping when interpolating into a URL path
/// segment. `/` is the segment delimiter and must be escaped to stay in-segment.
const PATH_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'/')
    .add(b'%');

/// Ceiling on how many events a single query returns, so a decade-wide range
/// on a busy calendar can't blow up a model's context window.
pub const MAX_EVENTS: usize = 500;

pub struct CalDavClient {
    client: Client,
    base: String,
    username: String,
    password: String,
    /// Discovery is three round trips; cache the result for the client's life.
    home: OnceCell<String>,
    calendars: OnceCell<Vec<Calendar>>,
}

impl CalDavClient {
    pub fn new(base: String, username: String, password: String) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self {
            client,
            base: base.trim_end_matches('/').to_string(),
            username,
            password,
            home: OnceCell::new(),
            calendars: OnceCell::new(),
        }
    }

    /// Issue a DAV request and return the body, mapping transport and status
    /// failures onto our error type.
    async fn dav(
        &self,
        method: &str,
        url: &str,
        depth: Option<&str>,
        content_type: &str,
        body: Option<String>,
    ) -> Result<String> {
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| Error::Server(format!("invalid HTTP method {method}")))?;

        let mut req = self
            .client
            .request(method.clone(), url)
            .basic_auth(&self.username, Some(&self.password));
        if let Some(depth) = depth {
            req = req.header("Depth", depth);
        }
        if let Some(body) = body {
            req = req.header("Content-Type", content_type).body(body);
        }

        let response = req.send().await?;
        let status = response.status();
        let text = response.text().await?;
        debug!(%method, %url, %status, "CalDAV response");

        match status.as_u16() {
            401 | 403 => Err(Error::InvalidCredentials(
                "server rejected the username/app password",
            )),
            429 => Err(Error::RateLimited),
            code if status.is_success() || code == 207 => Ok(text),
            code => Err(Error::Dav {
                method: method.to_string(),
                url: url.to_string(),
                status: code,
                // Bodies can be long HTML error pages; a prefix is enough to
                // diagnose without flooding output.
                body: truncate(&text, 400),
            }),
        }
    }

    // ---- Discovery ----

    /// `current-user-principal` for the authenticated user.
    async fn principal(&self) -> Result<String> {
        const BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop><d:current-user-principal/></d:prop>
</d:propfind>"#;

        // Well-known bootstrap first (RFC 6764), then the bare root — some
        // servers 404 the well-known path, others only answer there.
        let mut rejected = None;
        for path in ["/.well-known/caldav", "/"] {
            let url = format!("{}{}", self.base, path);
            let xml = match self
                .dav(
                    "PROPFIND",
                    &url,
                    Some("0"),
                    "application/xml",
                    Some(BODY.into()),
                )
                .await
            {
                Ok(xml) => xml,
                // Hold on to an auth rejection: "wrong password" is a far more
                // actionable message than "discovery failed", and it's the
                // overwhelmingly common cause.
                Err(e @ Error::InvalidCredentials(_)) => {
                    rejected = Some(e);
                    continue;
                }
                Err(_) => continue,
            };
            if let Some(href) = first_href_under(&xml, DAV_NS, "current-user-principal") {
                return Ok(util::href_path(&href));
            }
        }
        Err(rejected.unwrap_or(Error::Discovery("current-user-principal")))
    }

    /// The principal's `calendar-home-set` — the collection holding calendars.
    async fn calendar_home(&self) -> Result<&str> {
        self.home
            .get_or_try_init(|| async {
                const BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><c:calendar-home-set/></d:prop>
</d:propfind>"#;

                let principal = self.principal().await?;
                let url = util::resolve_url(&self.base, &principal);
                let xml = self
                    .dav(
                        "PROPFIND",
                        &url,
                        Some("0"),
                        "application/xml",
                        Some(BODY.into()),
                    )
                    .await?;
                first_href_under(&xml, CALDAV_NS, "calendar-home-set")
                    .map(|href| util::href_path(&href))
                    .ok_or(Error::Discovery("calendar-home-set"))
            })
            .await
            .map(String::as_str)
    }

    /// All calendar collections that can hold events, discovered once per client.
    #[instrument(skip(self))]
    pub async fn list_calendars(&self) -> Result<&[Calendar]> {
        self.calendars
            .get_or_try_init(|| async {
                const BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:ic="http://apple.com/ns/ical/">
  <d:prop>
    <d:displayname/>
    <d:resourcetype/>
    <d:current-user-privilege-set/>
    <c:supported-calendar-component-set/>
    <c:calendar-description/>
    <ic:calendar-color/>
  </d:prop>
</d:propfind>"#;

                let home = self.calendar_home().await?;
                let url = util::resolve_url(&self.base, home);
                let xml = self
                    .dav("PROPFIND", &url, Some("1"), "application/xml", Some(BODY.into()))
                    .await?;
                let mut calendars = parse_calendars(&xml, home);
                calendars.sort_by_key(|c| c.name.to_lowercase());
                Ok(calendars)
            })
            .await
            .map(Vec::as_slice)
    }

    /// Resolve a calendar by id, display name, or href. With no name, the
    /// first writable calendar wins — the common "just put it somewhere
    /// sensible" case.
    pub async fn find_calendar(&self, name: Option<&str>) -> Result<Calendar> {
        let calendars = self.list_calendars().await?;
        match name.map(str::trim).filter(|s| !s.is_empty()) {
            None => calendars
                .iter()
                .find(|c| !c.read_only)
                .or_else(|| calendars.first())
                .cloned()
                .ok_or_else(|| Error::CalendarNotFound("no calendars on this account".into())),
            Some(name) => calendars
                .iter()
                .find(|c| {
                    c.id.eq_ignore_ascii_case(name)
                        || c.name.eq_ignore_ascii_case(name)
                        || c.href == util::href_path(name)
                })
                .cloned()
                .ok_or_else(|| Error::CalendarNotFound(name.to_string())),
        }
    }

    // ---- Reading events ----

    /// Events in one calendar overlapping `[start, end)`.
    ///
    /// With `expand`, the server returns each occurrence of a recurring series
    /// as its own VEVENT (RFC 4791 §9.6.5) — what you want for an agenda. Without
    /// it you get the master event carrying its RRULE, which is what you want
    /// before editing a series.
    #[instrument(skip(self))]
    pub async fn list_events(
        &self,
        calendar: &Calendar,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        expand: bool,
    ) -> Result<Vec<Event>> {
        let range = format!(
            r#"<c:time-range start="{}" end="{}"/>"#,
            util::format_ical_utc(start),
            util::format_ical_utc(end)
        );
        let calendar_data = if expand {
            format!(
                r#"<c:calendar-data><c:expand start="{}" end="{}"/></c:calendar-data>"#,
                util::format_ical_utc(start),
                util::format_ical_utc(end)
            )
        } else {
            "<c:calendar-data/>".to_string()
        };

        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:getetag/>{calendar_data}</d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VEVENT">{range}</c:comp-filter>
    </c:comp-filter>
  </c:filter>
</c:calendar-query>"#
        );

        let url = util::resolve_url(&self.base, &calendar.href);
        let xml = match self
            .dav("REPORT", &url, Some("1"), "application/xml", Some(body))
            .await
        {
            Ok(xml) => xml,
            // Not every server implements <expand>. Retry unexpanded rather
            // than failing the whole query.
            Err(e) if expand => {
                debug!(error = %e, calendar = %calendar.name, "expand unsupported, retrying");
                return Box::pin(self.list_events(calendar, start, end, false)).await;
            }
            Err(e) => return Err(e),
        };

        let mut events = parse_event_responses(&xml, calendar);
        // Expansion can hand back instances just outside the window; the range
        // the caller asked for is the range they get.
        events.retain(|e| overlaps(e, start, end));
        events.sort_by_key(|e| e.start.sort_key());
        Ok(events)
    }

    /// Events across every calendar (or just `calendar`, when named).
    pub async fn events_in_range(
        &self,
        calendar: Option<&str>,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        expand: bool,
        limit: usize,
    ) -> Result<Vec<Event>> {
        let targets: Vec<Calendar> = match calendar {
            Some(name) => vec![self.find_calendar(Some(name)).await?],
            None => self
                .list_calendars()
                .await?
                .iter()
                .filter(|c| c.supports_events)
                .cloned()
                .collect(),
        };

        let mut all = Vec::new();
        for cal in &targets {
            match self.list_events(cal, start, end, expand).await {
                Ok(events) => all.extend(events),
                // One unreadable calendar (shared, or a server hiccup) should
                // not sink the whole agenda.
                Err(e) => tracing::warn!(calendar = %cal.name, error = %e, "skipping calendar"),
            }
        }
        all.sort_by_key(|e| e.start.sort_key());
        all.truncate(limit.min(MAX_EVENTS));
        Ok(all)
    }

    /// Find one event by UID. Searches every calendar unless one is named.
    #[instrument(skip(self))]
    pub async fn get_event(&self, uid: &str, calendar: Option<&str>) -> Result<Option<Event>> {
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:getetag/><c:calendar-data/></d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VEVENT">
        <c:prop-filter name="UID">
          <c:text-match collation="i;octet">{}</c:text-match>
        </c:prop-filter>
      </c:comp-filter>
    </c:comp-filter>
  </c:filter>
</c:calendar-query>"#,
            xml_escape(uid)
        );

        let targets: Vec<Calendar> = match calendar {
            Some(name) => vec![self.find_calendar(Some(name)).await?],
            None => self
                .list_calendars()
                .await?
                .iter()
                .filter(|c| c.supports_events)
                .cloned()
                .collect(),
        };

        for cal in &targets {
            let url = util::resolve_url(&self.base, &cal.href);
            let Ok(xml) = self
                .dav(
                    "REPORT",
                    &url,
                    Some("1"),
                    "application/xml",
                    Some(body.clone()),
                )
                .await
            else {
                continue;
            };
            // A text-match is a substring match on some servers, so confirm the
            // UID actually equals what was asked for. Prefer the master event
            // (no RECURRENCE-ID) when a series has overrides.
            let mut matches: Vec<Event> = parse_event_responses(&xml, cal)
                .into_iter()
                .filter(|e| e.id == uid)
                .collect();
            matches.sort_by_key(|e| e.recurrence_id.is_some());
            if let Some(event) = matches.into_iter().next() {
                return Ok(Some(event));
            }
        }
        Ok(None)
    }

    /// Substring search over summary, description, location, and attendees.
    ///
    /// Done client-side: CalDAV's server-side `text-match` is per-property and
    /// inconsistently implemented, so we fetch the window once and filter.
    pub async fn search_events(
        &self,
        query: &str,
        calendar: Option<&str>,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<Event>> {
        let events = self
            .events_in_range(calendar, start, end, true, MAX_EVENTS)
            .await?;
        let needle = query.trim().to_lowercase();
        let mut hits: Vec<Event> = events
            .into_iter()
            .filter(|e| matches_query(e, &needle))
            .collect();
        hits.truncate(limit.min(MAX_EVENTS));
        Ok(hits)
    }

    /// Busy windows in `[start, end)`.
    ///
    /// Tries the standard free-busy REPORT and falls back to deriving busy
    /// periods from the events themselves — iCloud does not answer free-busy
    /// queries on a personal calendar home.
    #[instrument(skip(self))]
    pub async fn free_busy(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<BusyPeriod>> {
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<c:free-busy-query xmlns:c="urn:ietf:params:xml:ns:caldav">
  <c:time-range start="{}" end="{}"/>
</c:free-busy-query>"#,
            util::format_ical_utc(start),
            util::format_ical_utc(end)
        );

        let home = self.calendar_home().await?;
        let url = util::resolve_url(&self.base, home);
        if let Ok(text) = self
            .dav("REPORT", &url, Some("1"), "application/xml", Some(body))
            .await
        {
            let periods = ical::parse_freebusy(&text);
            if !periods.is_empty() {
                return Ok(periods);
            }
        }

        debug!("free-busy REPORT unavailable or empty; deriving from events");
        let events = self
            .events_in_range(None, start, end, true, MAX_EVENTS)
            .await?;
        let mut periods: Vec<BusyPeriod> = events
            .iter()
            // A declined invitation isn't time you're busy.
            .filter(|e| e.status.as_deref() != Some("CANCELLED"))
            .filter_map(|e| {
                let s = e.start.instant?;
                let t = e.end.instant.unwrap_or(s);
                Some(BusyPeriod {
                    start: util::format_rfc3339(s),
                    end: util::format_rfc3339(t.max(s)),
                    status: if e.status.as_deref() == Some("TENTATIVE") {
                        "BUSY-TENTATIVE".into()
                    } else {
                        "BUSY".into()
                    },
                })
            })
            .collect();
        periods.sort_by(|a, b| a.start.cmp(&b.start));
        Ok(periods)
    }

    // ---- Writing events ----

    /// Create an event. `summary` and `start` are required.
    #[instrument(skip(self, fields))]
    pub async fn create_event(
        &self,
        calendar: Option<&str>,
        fields: &EventFields<'_>,
    ) -> Result<Event> {
        let summary = fields
            .summary
            .ok_or_else(|| Error::Server("summary is required to create an event".into()))?;
        let start_raw = fields
            .start
            .ok_or_else(|| Error::Server("start is required to create an event".into()))?;

        let cal = self.find_calendar(calendar).await?;
        if cal.read_only {
            return Err(Error::Server(format!(
                "calendar '{}' is read-only",
                cal.name
            )));
        }

        let (start, end) = resolve_span(start_raw, fields, None)?;
        let uid = uuid::Uuid::new_v4().to_string();
        let attendees = fields.attendees.unwrap_or(&[]).to_vec();
        let categories = fields.categories.unwrap_or(&[]).to_vec();

        let ics = ical::build_vcalendar(&VEventSpec {
            uid: &uid,
            summary,
            start: start.spec(fields.tzid),
            end: end.spec(fields.tzid),
            description: fields.description,
            location: fields.location,
            url: fields.url,
            status: fields.status,
            recurrence: fields.recurrence,
            attendees: &attendees,
            organizer: None,
            categories: &categories,
            sequence: 0,
            stamp: Utc::now(),
        });

        let href = format!(
            "{}{}.ics",
            ensure_trailing_slash(&cal.href),
            utf8_percent_encode(&uid, PATH_SEGMENT)
        );
        let url = util::resolve_url(&self.base, &href);
        debug!(%url, "creating event");

        let request = self
            .client
            .put(&url)
            .basic_auth(&self.username, Some(&self.password))
            .header("Content-Type", "text/calendar; charset=utf-8")
            // Refuse to clobber an existing resource at this href.
            .header("If-None-Match", "*")
            .body(ics.clone());
        self.send_write(request, "PUT", &url).await?;

        ical::parse_events(&ics, &cal.name, &cal.href, &href, None)
            .into_iter()
            .next()
            .ok_or_else(|| Error::Server("built an event the parser rejected".into()))
    }

    /// Update an event, merging the provided fields over what's stored.
    #[instrument(skip(self, fields))]
    pub async fn update_event(
        &self,
        uid: &str,
        calendar: Option<&str>,
        fields: &EventFields<'_>,
    ) -> Result<Event> {
        let existing = self
            .get_event(uid, calendar)
            .await?
            .ok_or_else(|| Error::EventNotFound(uid.to_string()))?;

        let summary = fields
            .summary
            .or(existing.summary.as_deref())
            .unwrap_or("(no title)");
        let tzid = fields.tzid.or(existing.start.tzid.as_deref());

        // Unspecified start keeps the stored one; unspecified end follows the
        // stored duration so moving an event doesn't silently resize it.
        let (start, end) = match fields.start {
            Some(raw) => resolve_span(raw, fields, Some(&existing))?,
            None => {
                let start = ResolvedTime {
                    instant: existing.start.instant.ok_or_else(|| {
                        Error::Server(
                            "stored event has an unparseable start; set --start to fix it".into(),
                        )
                    })?,
                    all_day: existing.all_day,
                };
                let end = match fields.end {
                    Some(raw) => resolve_end(raw, tzid, start)?,
                    None => match fields.duration_minutes {
                        Some(mins) => ResolvedTime {
                            instant: start.instant + Duration::minutes(mins),
                            all_day: start.all_day,
                        },
                        None => ResolvedTime {
                            instant: existing.end.instant.unwrap_or(start.instant),
                            all_day: existing.all_day,
                        },
                    },
                };
                (start, end)
            }
        };

        let attendees = match fields.attendees {
            Some(a) => a.to_vec(),
            None => existing.attendees.clone(),
        };
        let categories = match fields.categories {
            Some(c) => c.to_vec(),
            None => existing.categories.clone(),
        };

        let ics = ical::build_vcalendar(&VEventSpec {
            uid,
            summary,
            start: start.spec(tzid),
            end: end.spec(tzid),
            description: fields.description.or(existing.description.as_deref()),
            location: fields.location.or(existing.location.as_deref()),
            url: fields.url.or(existing.url.as_deref()),
            status: fields.status.or(existing.status.as_deref()),
            recurrence: fields.recurrence.or(existing.recurrence.as_deref()),
            attendees: &attendees,
            organizer: existing.organizer.as_ref(),
            categories: &categories,
            // Bumping SEQUENCE is how attendees' clients learn the event moved.
            sequence: existing.sequence.saturating_add(1),
            stamp: Utc::now(),
        });

        let url = util::resolve_url(&self.base, &existing.href);
        debug!(%url, "updating event");

        let mut request = self
            .client
            .put(&url)
            .basic_auth(&self.username, Some(&self.password))
            .header("Content-Type", "text/calendar; charset=utf-8")
            .body(ics.clone());
        // Optimistic concurrency: fail rather than overwrite a newer version.
        if let Some(etag) = existing.etag.as_deref() {
            request = request.header("If-Match", etag);
        }
        self.send_write(request, "PUT", &url).await?;

        ical::parse_events(
            &ics,
            &existing.calendar,
            &existing.calendar_href,
            &existing.href,
            None,
        )
        .into_iter()
        .next()
        .ok_or_else(|| Error::Server("built an event the parser rejected".into()))
    }

    /// Delete an event by UID.
    #[instrument(skip(self))]
    pub async fn delete_event(&self, uid: &str, calendar: Option<&str>) -> Result<Event> {
        let existing = self
            .get_event(uid, calendar)
            .await?
            .ok_or_else(|| Error::EventNotFound(uid.to_string()))?;

        let url = util::resolve_url(&self.base, &existing.href);
        debug!(%url, "deleting event");

        let mut request = self
            .client
            .delete(&url)
            .basic_auth(&self.username, Some(&self.password));
        if let Some(etag) = existing.etag.as_deref() {
            request = request.header("If-Match", etag);
        }
        self.send_write(request, "DELETE", &url).await?;
        Ok(existing)
    }

    /// Shared status handling for PUT/DELETE, which return no useful body.
    async fn send_write(
        &self,
        request: reqwest::RequestBuilder,
        method: &str,
        url: &str,
    ) -> Result<()> {
        let response = request.send().await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let text = response.text().await.unwrap_or_default();
        match status.as_u16() {
            401 | 403 => Err(Error::InvalidCredentials(
                "server rejected the username/app password",
            )),
            412 => Err(Error::Server(
                "the event changed on the server since it was read — re-read it and retry".into(),
            )),
            429 => Err(Error::RateLimited),
            code => Err(Error::Dav {
                method: method.to_string(),
                url: url.to_string(),
                status: code,
                body: truncate(&text, 400),
            }),
        }
    }
}

/// A user-supplied time resolved to an instant plus its all-day-ness.
#[derive(Debug, Clone, Copy)]
struct ResolvedTime {
    instant: DateTime<Utc>,
    all_day: bool,
}

impl ResolvedTime {
    fn spec<'a>(&self, tzid: Option<&'a str>) -> TimeSpec<'a> {
        TimeSpec {
            instant: self.instant,
            all_day: self.all_day,
            tzid: tzid.filter(|_| !self.all_day),
        }
    }
}

/// Resolve start and end from the fields, defaulting the end to one hour after
/// the start (a whole day for all-day events).
fn resolve_span(
    start_raw: &str,
    fields: &EventFields<'_>,
    existing: Option<&Event>,
) -> Result<(ResolvedTime, ResolvedTime)> {
    let tzid = fields
        .tzid
        .or_else(|| existing.and_then(|e| e.start.tzid.as_deref()));
    let parsed = util::parse_datetime(start_raw, tzid)?;
    let all_day = fields.all_day.unwrap_or(parsed.date_only);
    let start = ResolvedTime {
        instant: parsed.instant,
        all_day,
    };

    let end = match fields.end {
        Some(raw) => resolve_end(raw, tzid, start)?,
        None => match fields.duration_minutes {
            Some(mins) => ResolvedTime {
                instant: start.instant + Duration::minutes(mins),
                all_day,
            },
            None => ResolvedTime {
                instant: start.instant
                    + if all_day {
                        Duration::days(1)
                    } else {
                        Duration::hours(1)
                    },
                all_day,
            },
        },
    };

    if end.instant < start.instant {
        return Err(Error::InvalidDateTime {
            input: format!("{start_raw} .. {}", fields.end.unwrap_or("")),
            reason: "end is before start".into(),
        });
    }
    Ok((start, end))
}

fn resolve_end(raw: &str, tzid: Option<&str>, start: ResolvedTime) -> Result<ResolvedTime> {
    let parsed = util::parse_datetime(raw, tzid)?;
    Ok(ResolvedTime {
        instant: parsed.instant,
        all_day: start.all_day,
    })
}

/// Does the event overlap `[start, end)`? Zero-length events count as
/// overlapping when they sit exactly on the window's start.
fn overlaps(event: &Event, start: DateTime<Utc>, end: DateTime<Utc>) -> bool {
    let Some(event_start) = event.start.instant else {
        // Keep events we couldn't anchor rather than silently dropping them.
        return true;
    };
    let event_end = event.end.instant.unwrap_or(event_start);
    event_start < end && (event_end > start || event_end == event_start && event_start >= start)
}

fn matches_query(event: &Event, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let contains = |s: &Option<String>| {
        s.as_deref()
            .is_some_and(|v| v.to_lowercase().contains(needle))
    };
    contains(&event.summary)
        || contains(&event.description)
        || contains(&event.location)
        || event
            .categories
            .iter()
            .any(|c| c.to_lowercase().contains(needle))
        || event.attendees.iter().any(|a| {
            a.email.to_lowercase().contains(needle)
                || a.name
                    .as_deref()
                    .is_some_and(|n| n.to_lowercase().contains(needle))
        })
}

fn ensure_trailing_slash(href: &str) -> String {
    if href.ends_with('/') {
        href.to_string()
    } else {
        format!("{href}/")
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Cut on a char boundary so the message stays valid UTF-8.
    let cut = s
        .char_indices()
        .take_while(|(i, _)| *i <= max)
        .last()
        .map(|(i, _)| i)
        .unwrap_or(0);
    format!("{}…", &s[..cut])
}

/// Escape text for interpolation into an XML element body.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ---- Multistatus parsing ----

/// First `<d:href>` nested inside the named property, anywhere in the document.
fn first_href_under(xml: &str, ns: &str, prop: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    doc.descendants()
        .find(|n| n.has_tag_name((ns, prop)))?
        .descendants()
        .find(|n| n.has_tag_name((DAV_NS, "href")))?
        .text()
        .map(str::to_string)
}

fn parse_calendars(xml: &str, home: &str) -> Vec<Calendar> {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return Vec::new();
    };
    let home_path = util::href_path(home);
    let mut out = Vec::new();

    for response in doc
        .descendants()
        .filter(|n| n.has_tag_name((DAV_NS, "response")))
    {
        let Some(href) = response
            .children()
            .find(|n| n.has_tag_name((DAV_NS, "href")))
            .and_then(|n| n.text())
        else {
            continue;
        };
        let href = util::href_path(href);

        // The home collection itself comes back in a Depth:1 listing.
        if href.trim_end_matches('/') == home_path.trim_end_matches('/') {
            continue;
        }
        if !response
            .descendants()
            .any(|n| n.has_tag_name((CALDAV_NS, "calendar")))
        {
            continue;
        }

        let text_of = |ns: &str, name: &str| {
            response
                .descendants()
                .find(|n| n.has_tag_name((ns, name)))
                .and_then(|n| n.text())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };

        let name = text_of(DAV_NS, "displayname").unwrap_or_else(|| util::last_segment(&href));

        // supported-calendar-component-set lists the component types the
        // collection accepts. Absent means "anything", per RFC 4791 §5.2.3.
        let comps: Vec<String> = response
            .descendants()
            .filter(|n| n.has_tag_name((CALDAV_NS, "comp")))
            .filter_map(|n| n.attribute("name"))
            .map(|s| s.to_ascii_uppercase())
            .collect();
        let supports_events = comps.is_empty() || comps.iter().any(|c| c == "VEVENT");

        // If the server told us the privileges, trust them; if it said nothing,
        // assume writable and let a failed PUT be the authority.
        let privileges: Vec<String> = response
            .descendants()
            .filter(|n| n.has_tag_name((DAV_NS, "privilege")))
            .flat_map(|n| n.children().filter(|c| c.is_element()))
            .map(|n| n.tag_name().name().to_ascii_lowercase())
            .collect();
        let read_only = !privileges.is_empty()
            && !privileges
                .iter()
                .any(|p| p == "write" || p == "write-content" || p == "all");

        out.push(Calendar {
            id: util::last_segment(&href),
            href,
            name,
            description: text_of(CALDAV_NS, "calendar-description"),
            color: text_of(APPLE_NS, "calendar-color"),
            read_only,
            supports_events,
        });
    }
    out
}

/// Pull events out of a `calendar-query` multistatus response.
fn parse_event_responses(xml: &str, calendar: &Calendar) -> Vec<Event> {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return Vec::new();
    };
    let mut out = Vec::new();

    for response in doc
        .descendants()
        .filter(|n| n.has_tag_name((DAV_NS, "response")))
    {
        let href = response
            .children()
            .find(|n| n.has_tag_name((DAV_NS, "href")))
            .and_then(|n| n.text())
            .unwrap_or_default();
        let etag = response
            .descendants()
            .find(|n| n.has_tag_name((DAV_NS, "getetag")))
            .and_then(|n| n.text());
        let Some(data) = response
            .descendants()
            .find(|n| n.has_tag_name((CALDAV_NS, "calendar-data")))
            .and_then(|n| n.text())
        else {
            continue;
        };

        out.extend(ical::parse_events(
            data,
            &calendar.name,
            &calendar.href,
            href,
            etag,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Attendee;
    use chrono::TimeZone;

    const CALENDARS_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:ic="http://apple.com/ns/ical/">
  <response>
    <href>/1234/calendars/</href>
    <propstat><prop><resourcetype><collection/></resourcetype></prop></propstat>
  </response>
  <response>
    <href>/1234/calendars/home/</href>
    <propstat><prop>
      <displayname>Home</displayname>
      <resourcetype><collection/><cal:calendar/></resourcetype>
      <ic:calendar-color>#FF2968</ic:calendar-color>
      <cal:supported-calendar-component-set><cal:comp name="VEVENT"/></cal:supported-calendar-component-set>
      <current-user-privilege-set>
        <privilege><read/></privilege><privilege><write/></privilege>
      </current-user-privilege-set>
    </prop></propstat>
  </response>
  <response>
    <href>/1234/calendars/shared/</href>
    <propstat><prop>
      <displayname>Team (read only)</displayname>
      <resourcetype><collection/><cal:calendar/></resourcetype>
      <cal:supported-calendar-component-set><cal:comp name="VEVENT"/></cal:supported-calendar-component-set>
      <current-user-privilege-set><privilege><read/></privilege></current-user-privilege-set>
    </prop></propstat>
  </response>
  <response>
    <href>/1234/calendars/tasks/</href>
    <propstat><prop>
      <displayname>Reminders</displayname>
      <resourcetype><collection/><cal:calendar/></resourcetype>
      <cal:supported-calendar-component-set><cal:comp name="VTODO"/></cal:supported-calendar-component-set>
    </prop></propstat>
  </response>
</multistatus>"#;

    fn calendars() -> Vec<Calendar> {
        let mut c = parse_calendars(CALENDARS_XML, "/1234/calendars/");
        c.sort_by_key(|c| c.name.to_lowercase());
        c
    }

    #[test]
    fn parses_calendars_and_skips_the_home_collection() {
        let cals = calendars();
        assert_eq!(cals.len(), 3);
        assert!(!cals.iter().any(|c| c.href == "/1234/calendars/"));
    }

    #[test]
    fn reads_display_name_color_and_id() {
        let home = calendars().into_iter().find(|c| c.id == "home").unwrap();
        assert_eq!(home.name, "Home");
        assert_eq!(home.color.as_deref(), Some("#FF2968"));
        assert_eq!(home.href, "/1234/calendars/home/");
        assert!(home.supports_events);
        assert!(!home.read_only);
    }

    #[test]
    fn detects_read_only_from_privileges() {
        let shared = calendars().into_iter().find(|c| c.id == "shared").unwrap();
        assert!(shared.read_only);
    }

    #[test]
    fn flags_todo_only_collections_as_not_supporting_events() {
        let tasks = calendars().into_iter().find(|c| c.id == "tasks").unwrap();
        assert!(!tasks.supports_events);
        // No privilege set was sent, so we assume writable.
        assert!(!tasks.read_only);
    }

    #[test]
    fn finds_principal_href_in_propfind_response() {
        let xml = r#"<multistatus xmlns="DAV:"><response><href>/</href><propstat><prop>
          <current-user-principal><href>/1234/principal/</href></current-user-principal>
        </prop></propstat></response></multistatus>"#;
        assert_eq!(
            first_href_under(xml, DAV_NS, "current-user-principal").as_deref(),
            Some("/1234/principal/")
        );
    }

    #[test]
    fn finds_calendar_home_href() {
        let xml = r#"<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
          <response><href>/1234/principal/</href><propstat><prop>
            <c:calendar-home-set><href>https://p42.icloud.com/1234/calendars/</href></c:calendar-home-set>
          </prop></propstat></response></multistatus>"#;
        let href = first_href_under(xml, CALDAV_NS, "calendar-home-set").unwrap();
        assert_eq!(util::href_path(&href), "/1234/calendars/");
    }

    #[test]
    fn parses_events_out_of_a_calendar_query() {
        let xml = r#"<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <response>
    <href>/1234/calendars/home/evt-1.ics</href>
    <propstat><prop>
      <getetag>"abc"</getetag>
      <c:calendar-data>BEGIN:VCALENDAR
BEGIN:VEVENT
UID:evt-1
SUMMARY:Standup
DTSTART:20260724T090000Z
DTEND:20260724T093000Z
END:VEVENT
END:VCALENDAR
</c:calendar-data>
    </prop></propstat>
  </response>
</multistatus>"#;
        let cal = calendars().into_iter().find(|c| c.id == "home").unwrap();
        let events = parse_event_responses(xml, &cal);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, "evt-1");
        assert_eq!(events[0].calendar, "Home");
        assert_eq!(events[0].href, "/1234/calendars/home/evt-1.ics");
        assert_eq!(events[0].etag.as_deref(), Some("\"abc\""));
    }

    fn at(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 24, h, 0, 0).unwrap()
    }

    fn event_spanning(start: DateTime<Utc>, end: DateTime<Utc>) -> Event {
        let mut e = ical::parse_events(
            &format!(
                "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x\r\nSUMMARY:s\r\nDTSTART:{}\r\nDTEND:{}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
                util::format_ical_utc(start),
                util::format_ical_utc(end)
            ),
            "Home",
            "/c/",
            "/c/x.ics",
            None,
        );
        e.remove(0)
    }

    #[test]
    fn overlap_window_is_half_open() {
        let window = (at(10), at(12));
        // Ends exactly at the window start — outside.
        assert!(!overlaps(
            &event_spanning(at(8), at(10)),
            window.0,
            window.1
        ));
        // Starts exactly at the window end — outside.
        assert!(!overlaps(
            &event_spanning(at(12), at(13)),
            window.0,
            window.1
        ));
        // Straddles the window — inside.
        assert!(overlaps(&event_spanning(at(9), at(11)), window.0, window.1));
        // Fully contained — inside.
        assert!(overlaps(
            &event_spanning(at(10), at(11)),
            window.0,
            window.1
        ));
    }

    #[test]
    fn zero_length_event_on_the_boundary_counts_as_inside() {
        assert!(overlaps(&event_spanning(at(10), at(10)), at(10), at(12)));
        assert!(!overlaps(&event_spanning(at(9), at(9)), at(10), at(12)));
    }

    #[test]
    fn search_matches_across_fields() {
        let mut e = event_spanning(at(9), at(10));
        e.summary = Some("Quarterly Review".into());
        e.location = Some("Room 4".into());
        e.attendees = vec![Attendee {
            email: "jane@x.test".into(),
            name: Some("Jane Doe".into()),
            ..Default::default()
        }];
        e.categories = vec!["planning".into()];

        assert!(matches_query(&e, "quarterly"));
        assert!(matches_query(&e, "room 4"));
        assert!(matches_query(&e, "jane@x.test"));
        assert!(matches_query(&e, "jane doe"));
        assert!(matches_query(&e, "planning"));
        assert!(!matches_query(&e, "unrelated"));
        // An empty needle matches everything.
        assert!(matches_query(&e, ""));
    }

    #[test]
    fn resolve_span_defaults_to_an_hour() {
        let fields = EventFields {
            summary: Some("x"),
            start: Some("2026-07-24T09:00:00Z"),
            ..Default::default()
        };
        let (start, end) = resolve_span("2026-07-24T09:00:00Z", &fields, None).unwrap();
        assert_eq!(end.instant - start.instant, Duration::hours(1));
        assert!(!start.all_day);
    }

    #[test]
    fn resolve_span_defaults_all_day_to_one_day() {
        let fields = EventFields {
            summary: Some("x"),
            start: Some("2026-07-24"),
            ..Default::default()
        };
        let (start, end) = resolve_span("2026-07-24", &fields, None).unwrap();
        assert!(start.all_day);
        assert_eq!(end.instant - start.instant, Duration::days(1));
    }

    #[test]
    fn resolve_span_honours_duration_minutes() {
        let fields = EventFields {
            summary: Some("x"),
            start: Some("2026-07-24T09:00:00Z"),
            duration_minutes: Some(45),
            ..Default::default()
        };
        let (start, end) = resolve_span("2026-07-24T09:00:00Z", &fields, None).unwrap();
        assert_eq!(end.instant - start.instant, Duration::minutes(45));
    }

    #[test]
    fn resolve_span_rejects_end_before_start() {
        let fields = EventFields {
            summary: Some("x"),
            start: Some("2026-07-24T09:00:00Z"),
            end: Some("2026-07-24T08:00:00Z"),
            ..Default::default()
        };
        assert!(resolve_span("2026-07-24T09:00:00Z", &fields, None).is_err());
    }

    #[test]
    fn all_day_spec_drops_the_tzid() {
        let resolved = ResolvedTime {
            instant: at(0),
            all_day: true,
        };
        assert!(resolved.spec(Some("Europe/London")).tzid.is_none());
    }

    #[test]
    fn xml_escape_neutralises_markup() {
        assert_eq!(xml_escape("a<b>&c"), "a&lt;b&gt;&amp;c");
    }

    #[test]
    fn truncate_cuts_on_char_boundaries() {
        let s = "é".repeat(300);
        let cut = truncate(&s, 10);
        assert!(cut.ends_with('…'));
        // Would have panicked on a non-boundary slice.
        assert!(cut.len() < s.len());
    }

    #[test]
    fn trailing_slash_is_added_once() {
        assert_eq!(ensure_trailing_slash("/a/b"), "/a/b/");
        assert_eq!(ensure_trailing_slash("/a/b/"), "/a/b/");
    }
}
