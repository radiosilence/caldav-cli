//! CalDAV client (RFC 4791).
//!
//! Uses raw HTTP with reqwest, since CalDAV is WebDAV plus iCalendar — the
//! same shape as the CardDAV client in `fastmail-cli`. Discovery walks
//! `current-user-principal` → `calendar-home-set` → the calendar collections,
//! which is the portable path that works on iCloud, Fastmail, Google, and
//! Nextcloud alike.

pub mod ical;
pub mod recur;

use chrono::{DateTime, Duration, Utc};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use reqwest::Client;
use tokio::sync::OnceCell;
use tracing::{debug, instrument};

use crate::error::{Error, Result};
use crate::models::{BusyPeriod, Calendar, CalendarFields, Event, EventFields};
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

/// How many CalDAV requests may be in flight at once.
///
/// Time-range queries are per-collection — CalDAV offers no plural form — so
/// spanning an account costs one REPORT per calendar however you slice it.
/// Issuing them concurrently turns N round trips into roughly one; the cap stops
/// a 30-calendar account from opening 30 connections at a server that would
/// rather it didn't.
pub const MAX_CONCURRENT_REQUESTS: usize = 6;

/// One WebDAV property in a PROPPATCH.
///
/// Constructed rather than written out because the two halves are escaped
/// differently — a display name is text, a default-calendar URL is a `DAV:href`
/// — and getting that wrong is a silent per-property rejection.
pub struct DavProp<'a> {
    ns: &'a str,
    name: &'a str,
    /// The element's body, already XML. `None` removes the property.
    body: Option<String>,
}

impl<'a> DavProp<'a> {
    /// A text-valued property, e.g. `DAV:displayname`.
    pub fn text(ns: &'a str, name: &'a str, value: &str) -> Self {
        Self {
            ns,
            name,
            body: Some(xml_escape(value)),
        }
    }

    /// A property whose value is a `DAV:href`, as RFC 6638 defines
    /// `schedule-default-calendar-URL`.
    pub fn href(ns: &'a str, name: &'a str, href: &str) -> Self {
        Self {
            ns,
            name,
            body: Some(format!("<d:href>{}</d:href>", xml_escape(href))),
        }
    }

    /// Unset the property. Distinct from setting it empty: a server may accept
    /// one and refuse the other.
    pub fn remove(ns: &'a str, name: &'a str) -> Self {
        Self {
            ns,
            name,
            body: None,
        }
    }

    /// The property as an XML element, using the prefix its namespace is bound
    /// to by [`PROP_NAMESPACES`]. `None` for a namespace the document doesn't
    /// declare, which can't be addressed and so must not be silently emitted
    /// unqualified.
    fn element(&self) -> Option<String> {
        let prefix = PROP_NAMESPACES
            .iter()
            .find(|(_, ns)| *ns == self.ns)
            .map(|(prefix, _)| prefix)?;
        Some(match &self.body {
            Some(body) => format!("<{prefix}:{0}>{body}</{prefix}:{0}>", self.name),
            None => format!("<{prefix}:{}/>", self.name),
        })
    }
}

/// The namespaces a property-writing document declares, as `(prefix, uri)`.
/// One list so the declarations and [`DavProp::element`]'s prefixes cannot drift.
const PROP_NAMESPACES: [(&str, &str); 3] = [("d", DAV_NS), ("c", CALDAV_NS), ("ic", APPLE_NS)];

/// `xmlns:` declarations for [`PROP_NAMESPACES`].
fn xmlns_decls() -> String {
    PROP_NAMESPACES
        .iter()
        .map(|(prefix, ns)| format!(r#" xmlns:{prefix}="{ns}""#))
        .collect()
}

/// Run `tasks` concurrently, at most [`MAX_CONCURRENT_REQUESTS`] at a time,
/// returning their outputs in the order the tasks were given.
pub(crate) async fn fan_out<F: std::future::Future>(tasks: Vec<F>) -> Vec<F::Output> {
    use futures_util::stream::{self, StreamExt};
    stream::iter(tasks)
        .buffered(MAX_CONCURRENT_REQUESTS)
        .collect()
        .await
}

pub struct CalDavClient {
    client: Client,
    base: String,
    username: String,
    password: String,
    /// Discovery is three round trips; the principal and home don't change, so
    /// cache them for the client's life.
    home: OnceCell<String>,
}

impl CalDavClient {
    pub fn new(base: String, username: String, password: String) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            // iCloud rejects requests with no User-Agent outright (403), and
            // reqwest sends none by default. Identifying ourselves is also
            // just good manners toward the servers we talk to.
            .user_agent(concat!("caldav-cli/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        Self {
            client,
            base: base.trim_end_matches('/').to_string(),
            username,
            password,
            home: OnceCell::new(),
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
                // Resolve against the URL we just queried, not `self.base` —
                // discovery can hand us a different host (see `calendar_home`).
                return Ok(util::resolve_url(&url, &href));
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
                let xml = self
                    .dav(
                        "PROPFIND",
                        &principal,
                        Some("0"),
                        "application/xml",
                        Some(BODY.into()),
                    )
                    .await?;
                // iCloud shards accounts across partition hosts: the home set
                // for a given user comes back on `pNN-caldav.icloud.com`, not
                // the `caldav.icloud.com` we asked. Everything from here on
                // must address that host, so keep the absolute URL rather than
                // reducing it to a path and re-resolving against `self.base`.
                first_href_under(&xml, CALDAV_NS, "calendar-home-set")
                    .map(|href| util::resolve_url(&principal, &href))
                    .ok_or(Error::Discovery("calendar-home-set"))
            })
            .await
            .map(String::as_str)
    }

    /// All calendar collections that can hold events.
    ///
    /// Deliberately **not** memoised on the client. It used to be, and that was
    /// a bug: clients are pooled per credential for the life of the process, so
    /// a long-running MCP server never saw a calendar created, renamed, or
    /// deleted after start-up — for writes as well as reads. Deduplication
    /// belongs to the caller's scope, not the connection's: the GraphQL layer
    /// gets it from a per-request loader, and a CLI process is one command long.
    ///
    /// Discovery of the principal and calendar home *is* still cached — those
    /// don't change.
    #[instrument(skip(self))]
    pub async fn list_calendars(&self) -> Result<Vec<Calendar>> {
        const BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:ic="http://apple.com/ns/ical/">
  <d:prop>
    <d:displayname/>
    <d:resourcetype/>
    <d:current-user-privilege-set/>
    <c:supported-calendar-component-set/>
    <c:calendar-description/>
    <c:schedule-default-calendar-URL/>
    <ic:calendar-color/>
  </d:prop>
</d:propfind>"#;

        let home = self.calendar_home().await?;
        let xml = self
            .dav(
                "PROPFIND",
                home,
                Some("1"),
                "application/xml",
                Some(BODY.into()),
            )
            .await?;
        let mut calendars = parse_calendars(&xml, home);
        calendars.sort_by_key(|c| c.name.to_lowercase());

        // Servers disagree on where they'll answer this. Most volunteer it in
        // the listing above; RFC 6638 only requires it on the scheduling inbox.
        // Ask there when the free answer is missing — one extra round trip, and
        // only when needed.
        let default = match parse_default_href(&xml) {
            Some(href) => Some(href),
            None => self.default_calendar_from_inbox(&xml, home).await,
        };
        if let Some(href) = default {
            mark_default(&mut calendars, &href);
        }
        Ok(calendars)
    }

    /// Ask the scheduling inbox for the default calendar, the one place RFC
    /// 6638 requires it to live. Best-effort: an account with no scheduling
    /// support has no inbox, and a server may refuse the request outright.
    async fn default_calendar_from_inbox(&self, listing: &str, home: &str) -> Option<String> {
        const BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><c:schedule-default-calendar-URL/></d:prop>
</d:propfind>"#;

        let inbox = parse_inbox_href(listing, home)?;
        let xml = self
            .dav(
                "PROPFIND",
                &inbox,
                Some("0"),
                "application/xml",
                Some(BODY.into()),
            )
            .await
            .inspect_err(|e| debug!(error = %e, "no default calendar from the scheduling inbox"))
            .ok()?;
        parse_default_href(&xml)
    }

    /// Resolve a calendar by id, display name, or href. With no name, the
    /// account's own default wins, then the first writable one — "just put it
    /// where my calendar app would".
    pub async fn find_calendar(&self, name: Option<&str>) -> Result<Calendar> {
        resolve_calendar(&self.list_calendars().await?, name)
    }

    /// The address this client authenticates as, used to pick your own row out
    /// of an event's attendee list.
    pub fn username(&self) -> &str {
        &self.username
    }

    // ---- Writing properties ----

    /// Set or remove properties on a collection (RFC 4918 §9.2).
    ///
    /// A PROPPATCH answers `207` whichever properties it accepted: the real
    /// outcome is the status inside each `propstat`, so a server that refused
    /// every property still looks successful at the HTTP layer. This reads the
    /// propstats and fails on any that isn't 2xx, naming the properties — a
    /// silent no-op is the worst possible outcome for a write.
    #[instrument(skip(self, props))]
    pub async fn proppatch(&self, url: &str, props: &[DavProp<'_>]) -> Result<()> {
        let render = |want_set: bool| -> String {
            props
                .iter()
                .filter(|p| p.body.is_some() == want_set)
                .filter_map(DavProp::element)
                .collect()
        };
        let (set, remove) = (render(true), render(false));
        if set.is_empty() && remove.is_empty() {
            return Ok(());
        }

        let wrap = |tag: &str, inner: String| match inner.is_empty() {
            true => String::new(),
            false => format!("<d:{tag}><d:prop>{inner}</d:prop></d:{tag}>"),
        };
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<d:propertyupdate{}>
{}{}</d:propertyupdate>"#,
            xmlns_decls(),
            wrap("set", set),
            wrap("remove", remove)
        );

        let xml = self
            .dav("PROPPATCH", url, Some("0"), "application/xml", Some(body))
            .await?;
        match proppatch_failures(&xml) {
            failures if failures.is_empty() => Ok(()),
            failures => Err(Error::Server(format!(
                "the server refused {}: {}",
                if failures.len() == 1 {
                    "a property"
                } else {
                    "some properties"
                },
                failures.join("; ")
            ))),
        }
    }

    // ---- Calendar collections ----

    /// Create a calendar collection with `MKCALENDAR` (RFC 4791 §5.3.1).
    ///
    /// The collection's path segment is derived from the name, so a calendar
    /// called "Work Trips" lives at `.../work-trips/` — readable, and the id the
    /// rest of this tool addresses it by. A name that collides with an existing
    /// collection is refused by the server rather than worked around: two
    /// calendars with one name is a worse outcome than an error.
    #[instrument(skip(self, fields))]
    pub async fn create_calendar(
        &self,
        name: &str,
        fields: &CalendarFields<'_>,
    ) -> Result<Calendar> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Server("a calendar needs a name".into()));
        }
        let slug = slugify(name);
        let home = self.calendar_home().await?;
        let url = format!(
            "{}{}/",
            ensure_trailing_slash(home),
            utf8_percent_encode(&slug, PATH_SEGMENT)
        );

        // Properties go in the MKCALENDAR body rather than a follow-up
        // PROPPATCH so the calendar is never briefly visible unnamed.
        let props = [
            Some(DavProp::text(DAV_NS, "displayname", name)),
            fields
                .description
                .map(|v| DavProp::text(CALDAV_NS, "calendar-description", v)),
            fields
                .color
                .map(|v| DavProp::text(APPLE_NS, "calendar-color", v)),
            fields
                .order
                .map(|v| DavProp::text(APPLE_NS, "calendar-order", &v.to_string())),
        ];
        let inner: String = props
            .iter()
            .flatten()
            .filter_map(DavProp::element)
            .collect();
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<c:mkcalendar{}>
  <d:set><d:prop>{inner}</d:prop></d:set>
</c:mkcalendar>"#,
            xmlns_decls()
        );
        debug!(%url, "creating calendar");
        self.dav("MKCALENDAR", &url, None, "application/xml", Some(body))
            .await
            .map_err(|e| match e {
                // 405 is what RFC 4791 §5.3.1.2 mandates for an occupied path.
                Error::Dav { status: 405, .. } => Error::Server(format!(
                    "a collection already exists at {slug:?} — pick a different name"
                )),
                other => other,
            })?;

        // Re-read rather than assume: the server decides the final display name,
        // whether it took the colour, and what privileges it granted.
        self.find_calendar(Some(&slug)).await
    }

    /// Rename, recolour, or re-describe a calendar via PROPPATCH.
    ///
    /// A `Some("")` field removes the property; `None` leaves it alone.
    #[instrument(skip(self, fields))]
    pub async fn update_calendar(
        &self,
        calendar: &str,
        fields: &CalendarFields<'_>,
    ) -> Result<Calendar> {
        let existing = self.find_calendar(Some(calendar)).await?;
        if existing.read_only {
            return Err(Error::Server(format!(
                "calendar '{}' is read-only",
                existing.name
            )));
        }

        // Clearing a display name would leave the calendar addressable only by
        // id, in every client the user owns.
        let name = fields.name.map(str::trim).filter(|s| !s.is_empty());
        let order = fields.order.map(|v| v.to_string());
        let prop = |ns, name, value: &str| match value.is_empty() {
            true => DavProp::remove(ns, name),
            false => DavProp::text(ns, name, value),
        };
        let props: Vec<DavProp<'_>> = [
            name.map(|v| DavProp::text(DAV_NS, "displayname", v)),
            fields
                .description
                .map(|v| prop(CALDAV_NS, "calendar-description", v)),
            fields.color.map(|v| prop(APPLE_NS, "calendar-color", v)),
            order
                .as_deref()
                .map(|v| prop(APPLE_NS, "calendar-order", v)),
        ]
        .into_iter()
        .flatten()
        .collect();

        if props.is_empty() {
            return Ok(existing);
        }
        self.proppatch(&existing.url, &props).await?;
        self.find_calendar(Some(&existing.id)).await
    }

    /// Delete a calendar collection and everything in it.
    ///
    /// Returns the calendar as it was, since after this there is nothing left to
    /// read it back from.
    #[instrument(skip(self))]
    pub async fn delete_calendar(&self, calendar: &str) -> Result<Calendar> {
        let existing = self.find_calendar(Some(calendar)).await?;
        if existing.read_only {
            return Err(Error::Server(format!(
                "calendar '{}' is read-only",
                existing.name
            )));
        }
        if existing.is_default {
            return Err(Error::Server(format!(
                "calendar '{}' is the account's default calendar — point the default \
                 at another calendar first",
                existing.name
            )));
        }
        debug!(url = %existing.url, "deleting calendar");
        let request = self
            .client
            .delete(&existing.url)
            .basic_auth(&self.username, Some(&self.password));
        self.send_write(request, "DELETE", &existing.url).await?;
        Ok(existing)
    }

    /// Point the account's default calendar at `calendar` (RFC 6638 §9.2).
    ///
    /// This is the property the user's own calendar apps read to decide where a
    /// new event goes, so it changes behaviour well beyond this tool. It lives on
    /// the scheduling inbox, not on the calendar — an account without scheduling
    /// support has nowhere to store it and gets a plain error.
    #[instrument(skip(self))]
    pub async fn set_default_calendar(&self, calendar: &str) -> Result<Calendar> {
        let target = self.find_calendar(Some(calendar)).await?;
        if target.read_only {
            return Err(Error::Server(format!(
                "calendar '{}' is read-only, so new events could not be written to it",
                target.name
            )));
        }
        if !target.supports_events {
            return Err(Error::Server(format!(
                "calendar '{}' holds no events, so nothing would ever land in it",
                target.name
            )));
        }

        let inbox = self.scheduling_inbox().await?;
        self.proppatch(
            &inbox,
            &[DavProp::href(
                CALDAV_NS,
                "schedule-default-calendar-URL",
                &target.href,
            )],
        )
        .await?;
        self.find_calendar(Some(&target.id)).await
    }

    /// The scheduling inbox collection (RFC 6638 §2.2.1), the one place the spec
    /// requires the default-calendar property to live.
    async fn scheduling_inbox(&self) -> Result<String> {
        const BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop><d:resourcetype/></d:prop>
</d:propfind>"#;

        let home = self.calendar_home().await?;
        let xml = self
            .dav(
                "PROPFIND",
                home,
                Some("1"),
                "application/xml",
                Some(BODY.into()),
            )
            .await?;
        parse_inbox_href(&xml, home).ok_or_else(|| {
            Error::Server(
                "this account has no scheduling inbox, so it has no default-calendar \
                 property to set"
                    .into(),
            )
        })
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
        let body = self.query_body(start, end, expand);

        let xml = match self
            .dav(
                "REPORT",
                &calendar.url,
                Some("1"),
                "application/xml",
                Some(body),
            )
            .await
        {
            Ok(xml) => xml,
            // Not every server implements <expand> — iCloud is unreliable
            // here. Retry without it and expand client-side below, so the
            // caller gets occurrences either way.
            Err(e) if expand => {
                debug!(error = %e, calendar = %calendar.name, "expand unsupported, retrying");
                let retry = self.query_body(start, end, false);
                self.dav(
                    "REPORT",
                    &calendar.url,
                    Some("1"),
                    "application/xml",
                    Some(retry),
                )
                .await?
            }
            Err(e) => return Err(e),
        };

        let mut events = parse_event_responses(&xml, calendar);
        if expand {
            // Idempotent: anything the server already expanded arrives without
            // an RRULE and passes straight through.
            events = recur::expand_all(events, start, end);
        }
        // Expansion can hand back instances just outside the window; the range
        // the caller asked for is the range they get.
        events.retain(|e| overlaps(e, start, end));
        events.sort_by_key(|e| e.start.sort_key());
        Ok(events)
    }

    /// The `calendar-query` REPORT body for a time range.
    fn query_body(&self, start: DateTime<Utc>, end: DateTime<Utc>, expand: bool) -> String {
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
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:getetag/>{calendar_data}</d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VEVENT">{range}</c:comp-filter>
    </c:comp-filter>
  </c:filter>
</c:calendar-query>"#
        )
    }

    /// Fetch specific `.ics` resources from one calendar in a single request.
    ///
    /// `calendar-multiget` (RFC 4791 §7.9) is the one place CalDAV lets us batch:
    /// any number of hrefs, one REPORT. Everything else — the calendar listing, a
    /// time-range query — is per-collection with no plural form.
    #[instrument(skip(self, hrefs))]
    pub async fn multiget_events(
        &self,
        calendar: &Calendar,
        hrefs: &[String],
    ) -> Result<Vec<Event>> {
        if hrefs.is_empty() {
            return Ok(Vec::new());
        }
        let refs: String = hrefs
            .iter()
            .map(|h| format!("  <d:href>{}</d:href>\n", xml_escape(h)))
            .collect();
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:getetag/><c:calendar-data/></d:prop>
{refs}</c:calendar-multiget>"#
        );
        let xml = self
            .dav(
                "REPORT",
                &calendar.url,
                Some("1"),
                "application/xml",
                Some(body),
            )
            .await?;
        Ok(parse_event_responses(&xml, calendar))
    }

    /// The calendars a read should span: the one named, or every collection that
    /// holds events.
    pub async fn read_targets(&self, calendar: Option<&str>) -> Result<Vec<Calendar>> {
        Ok(match calendar {
            Some(name) => vec![self.find_calendar(Some(name)).await?],
            None => self
                .list_calendars()
                .await?
                .iter()
                .filter(|c| c.supports_events)
                .cloned()
                .collect(),
        })
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
        let targets = self.read_targets(calendar).await?;
        let calls: Vec<_> = targets
            .iter()
            .map(|cal| self.list_events(cal, start, end, expand))
            .collect();
        let results = fan_out(calls).await;

        let mut all: Vec<Event> = results
            .into_iter()
            .zip(&targets)
            // One unreadable calendar (shared, or a server hiccup) should not
            // sink the whole agenda.
            .filter_map(|(events, cal)| {
                events
                    .inspect_err(
                        |e| tracing::warn!(calendar = %cal.name, error = %e, "skipping calendar"),
                    )
                    .ok()
            })
            .flatten()
            .collect();

        all.sort_by_key(|e| e.start.sort_key());
        all.truncate(limit.min(MAX_EVENTS));
        Ok(all)
    }

    /// Look one UID up in one calendar.
    ///
    /// CalDAV filters have no OR (RFC 4791 §9.7 ANDs every sibling), so a UID
    /// lookup is one REPORT per calendar per UID — there is no plural form to
    /// batch into. Callers spanning an account fan these out instead.
    #[instrument(skip(self))]
    pub async fn event_in_calendar(&self, uid: &str, calendar: &Calendar) -> Result<Option<Event>> {
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

        let xml = self
            .dav(
                "REPORT",
                &calendar.url,
                Some("1"),
                "application/xml",
                Some(body),
            )
            .await?;

        // A text-match is a substring match on some servers, so confirm the UID
        // actually equals what was asked for. Prefer the master event (no
        // RECURRENCE-ID) when a series has overrides.
        let mut matches: Vec<Event> = parse_event_responses(&xml, calendar)
            .into_iter()
            .filter(|e| e.id == uid)
            .collect();
        matches.sort_by_key(|e| e.recurrence_id.is_some());
        Ok(matches.into_iter().next())
    }

    /// Find one event by UID. Searches every calendar unless one is named.
    ///
    /// Stops at the first calendar holding the UID, so the usual case costs one
    /// REPORT. Only a miss pays for the whole sweep.
    #[instrument(skip(self))]
    pub async fn get_event(&self, uid: &str, calendar: Option<&str>) -> Result<Option<Event>> {
        for cal in self.read_targets(calendar).await? {
            // An unreadable calendar shouldn't mask a hit in the next one.
            if let Ok(Some(event)) = self.event_in_calendar(uid, &cal).await {
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
        if let Ok(text) = self
            .dav("REPORT", home, Some("1"), "application/xml", Some(body))
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
            // A new event has no history to preserve.
            recur_extra: &[],
            attendees: &attendees,
            organizer: None,
            categories: &categories,
            sequence: 0,
            stamp: Utc::now(),
        });

        let file = format!("{}.ics", utf8_percent_encode(&uid, PATH_SEGMENT));
        let url = format!("{}{}", ensure_trailing_slash(&cal.url), file);
        let href = format!("{}{}", ensure_trailing_slash(&cal.href), file);
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
            .map(|mut e| {
                e.resource_url = url.clone();
                e
            })
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
        // A caller replacing the RRULE is redefining the series, so the
        // exceptions to the old rule no longer describe anything.
        let recur_extra = match fields.recurrence {
            Some(_) => Vec::new(),
            None => ical::recur_extra_lines(existing.recur_source.as_deref()),
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
            // Carried across untouched. Dropping them used to resurrect every
            // occurrence the user had already cancelled.
            recur_extra: &recur_extra,
            attendees: &attendees,
            organizer: existing.organizer.as_ref(),
            categories: &categories,
            // Bumping SEQUENCE is how attendees' clients learn the event moved.
            sequence: existing.sequence.saturating_add(1),
            stamp: Utc::now(),
        });

        let url = existing.resource_url.clone();
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

    /// Move an event into another calendar, keeping its UID.
    ///
    /// Tries WebDAV `MOVE` first because it is lossless — the stored iCalendar
    /// object is relocated byte for byte, alarms, attachments and override
    /// components included. Servers that refuse it fall back to copying the raw
    /// resource across and deleting the original, which is lossless for the same
    /// reason: the bytes are never round-tripped through our model.
    #[instrument(skip(self))]
    pub async fn move_event(&self, uid: &str, from: Option<&str>, to: &str) -> Result<Event> {
        let existing = self
            .get_event(uid, from)
            .await?
            .ok_or_else(|| Error::EventNotFound(uid.to_string()))?;
        let target = self.find_calendar(Some(to)).await?;

        if target.href == existing.calendar_href {
            return Err(Error::Server(format!(
                "the event is already in '{}'",
                target.name
            )));
        }
        if target.read_only {
            return Err(Error::Server(format!(
                "calendar '{}' is read-only",
                target.name
            )));
        }
        if !target.supports_events {
            return Err(Error::Server(format!(
                "calendar '{}' does not hold events",
                target.name
            )));
        }

        let file = util::last_segment(&existing.href);
        let dest_url = format!("{}{}", ensure_trailing_slash(&target.url), file);
        let dest_href = format!("{}{}", ensure_trailing_slash(&target.href), file);
        let source_url = existing.resource_url.clone();
        debug!(%source_url, %dest_url, "moving event");

        let moved = self
            .client
            .request(reqwest::Method::from_bytes(b"MOVE").unwrap(), &source_url)
            .basic_auth(&self.username, Some(&self.password))
            .header("Destination", &dest_url)
            // Never silently replace an unrelated event already at that path.
            .header("Overwrite", "F")
            .send()
            .await;

        match moved {
            Ok(response) if response.status().is_success() => {}
            // A refusal here is the server saying it doesn't implement MOVE
            // across collections; a 412 is it saying something is already there,
            // which copying would hit too.
            Ok(response) if response.status().as_u16() == 412 => {
                return Err(Error::Server(format!(
                    "something already exists at {dest_url} — the event was not moved"
                )));
            }
            Ok(response) => {
                let status = response.status().as_u16();
                debug!(status, "MOVE refused, falling back to copy and delete");
                self.copy_then_delete(&source_url, &dest_url, existing.etag.as_deref())
                    .await?;
            }
            Err(e) => return Err(e.into()),
        }

        Ok(Event {
            calendar: target.name.clone(),
            calendar_href: target.href.clone(),
            href: util::href_path(&dest_href),
            resource_url: dest_url,
            // The etag belonged to the resource at the old path.
            etag: None,
            ..existing
        })
    }

    /// Fallback for [`CalDavClient::move_event`]: transfer the resource verbatim,
    /// then remove the original.
    ///
    /// The write goes first. If the delete then fails the event exists in both
    /// calendars, which is recoverable and visible; deleting first and failing to
    /// write would lose it outright.
    async fn copy_then_delete(
        &self,
        source_url: &str,
        dest_url: &str,
        etag: Option<&str>,
    ) -> Result<()> {
        let body = self.get_raw(source_url).await?;

        let put = self
            .client
            .put(dest_url)
            .basic_auth(&self.username, Some(&self.password))
            .header("Content-Type", "text/calendar; charset=utf-8")
            .header("If-None-Match", "*")
            .body(body);
        self.send_write(put, "PUT", dest_url).await?;

        let mut delete = self
            .client
            .delete(source_url)
            .basic_auth(&self.username, Some(&self.password));
        if let Some(etag) = etag {
            delete = delete.header("If-Match", etag);
        }
        self.send_write(delete, "DELETE", source_url)
            .await
            .map_err(|e| {
                Error::Server(format!(
                    "the event was copied to {dest_url} but the original at {source_url} \
                     could not be removed, so it now exists twice: {e}"
                ))
            })
    }

    /// GET a resource's body unchanged.
    async fn get_raw(&self, url: &str) -> Result<String> {
        let response = self
            .client
            .get(url)
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        match status.as_u16() {
            401 | 403 => Err(Error::InvalidCredentials(
                "server rejected the username/app password",
            )),
            429 => Err(Error::RateLimited),
            _ if status.is_success() => Ok(text),
            code => Err(Error::Dav {
                method: "GET".into(),
                url: url.to_string(),
                status: code,
                body: truncate(&text, 400),
            }),
        }
    }

    /// Cancel one occurrence of a recurring series by adding an `EXDATE`, leaving
    /// the rest of the series intact.
    ///
    /// `occurrence` may name a bare date, in which case it resolves against the
    /// single occurrence falling on that day. Anything ambiguous is reported with
    /// the candidates rather than guessed at — cancelling the wrong meeting is
    /// not a recoverable mistake.
    #[instrument(skip(self))]
    pub async fn exclude_occurrence(
        &self,
        uid: &str,
        calendar: Option<&str>,
        occurrence: &str,
    ) -> Result<Event> {
        let existing = self
            .get_event(uid, calendar)
            .await?
            .ok_or_else(|| Error::EventNotFound(uid.to_string()))?;
        if existing.recurrence.is_none() {
            return Err(Error::Server(format!(
                "event {uid} is not a recurring series — delete it outright instead"
            )));
        }

        let tzid = existing.start.tzid.as_deref();
        let at = resolve_occurrence(&existing, occurrence, tzid)?;
        let tz = util::resolve_tz(tzid)?;
        if ical::excluded_instants(existing.recur_source.as_deref(), tz).contains(&at) {
            return Err(Error::Server(format!(
                "the occurrence at {} is already cancelled",
                util::format_rfc3339(at)
            )));
        }

        let exdate = TimeSpec {
            instant: at,
            all_day: existing.all_day,
            tzid: tzid.filter(|_| !existing.all_day),
        }
        .render("EXDATE");

        let mut recur_extra = ical::recur_extra_lines(existing.recur_source.as_deref());
        recur_extra.push(exdate);
        self.put_event(&existing, &existing.attendees, &recur_extra)
            .await
    }

    /// Set your own `PARTSTAT` on an event you were invited to (RFC 6638 §3.2.5).
    ///
    /// Writing the reply to your own copy of the event is how CalDAV scheduling
    /// works: the server notices the changed participation status and delivers
    /// the reply to the organiser. `attendee` picks which row to update when the
    /// address on the invitation differs from the login — common on iCloud, where
    /// invitations arrive at an alias.
    #[instrument(skip(self))]
    pub async fn respond_to_invite(
        &self,
        uid: &str,
        calendar: Option<&str>,
        partstat: &str,
        attendee: Option<&str>,
    ) -> Result<Event> {
        let existing = self
            .get_event(uid, calendar)
            .await?
            .ok_or_else(|| Error::EventNotFound(uid.to_string()))?;

        let me = attendee.unwrap_or(&self.username);
        let mut attendees = existing.attendees.clone();
        let Some(row) = attendees
            .iter_mut()
            .find(|a| a.email.eq_ignore_ascii_case(me))
        else {
            let listed: Vec<&str> = existing
                .attendees
                .iter()
                .map(|a| a.email.as_str())
                .collect();
            return Err(Error::Server(format!(
                "{me} is not on the attendee list for {uid}, so there is no reply to send. \
                 Invited: {}",
                if listed.is_empty() {
                    "nobody".to_string()
                } else {
                    listed.join(", ")
                }
            )));
        };
        row.status = Some(partstat.to_ascii_uppercase());

        let recur_extra = ical::recur_extra_lines(existing.recur_source.as_deref());
        self.put_event(&existing, &attendees, &recur_extra).await
    }

    /// Re-write a stored event, changing only the attendee list and the
    /// recurrence properties this tool doesn't model.
    ///
    /// Shared by the writes that adjust one facet of an event rather than merging
    /// a whole field set over it, so none of them has to restate how an event is
    /// rebuilt and addressed.
    async fn put_event(
        &self,
        existing: &Event,
        attendees: &[crate::models::Attendee],
        recur_extra: &[String],
    ) -> Result<Event> {
        let start = existing.start.instant.ok_or_else(|| {
            Error::Server("stored event has an unparseable start; set --start to fix it".into())
        })?;
        let tzid = existing.start.tzid.as_deref();
        let spec = |instant| TimeSpec {
            instant,
            all_day: existing.all_day,
            tzid: tzid.filter(|_| !existing.all_day),
        };

        let ics = ical::build_vcalendar(&VEventSpec {
            uid: &existing.id,
            summary: existing.summary.as_deref().unwrap_or("(no title)"),
            start: spec(start),
            end: spec(existing.end.instant.unwrap_or(start)),
            description: existing.description.as_deref(),
            location: existing.location.as_deref(),
            url: existing.url.as_deref(),
            status: existing.status.as_deref(),
            recurrence: existing.recurrence.as_deref(),
            recur_extra,
            attendees,
            organizer: existing.organizer.as_ref(),
            categories: &existing.categories,
            sequence: existing.sequence.saturating_add(1),
            stamp: Utc::now(),
        });

        let url = existing.resource_url.clone();
        let mut request = self
            .client
            .put(&url)
            .basic_auth(&self.username, Some(&self.password))
            .header("Content-Type", "text/calendar; charset=utf-8")
            .body(ics.clone());
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
        .map(|mut e| {
            e.resource_url = url;
            e
        })
        .ok_or_else(|| Error::Server("built an event the parser rejected".into()))
    }

    /// Delete an event by UID.
    #[instrument(skip(self))]
    pub async fn delete_event(&self, uid: &str, calendar: Option<&str>) -> Result<Event> {
        let existing = self
            .get_event(uid, calendar)
            .await?
            .ok_or_else(|| Error::EventNotFound(uid.to_string()))?;

        let url = existing.resource_url.clone();
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

/// Which occurrence of a series `occurrence` names, as an instant.
///
/// An exact instant match wins. Failing that, a value that lands on a day with
/// exactly one occurrence resolves to it — which is what lets a caller say
/// `2026-08-03` without knowing what time the series runs at. Anything genuinely
/// ambiguous is refused with the candidates listed: guessing which of two
/// meetings to cancel is not a mistake worth making silently.
///
/// Public because it reads nothing from the network: a preview can resolve which
/// occurrence it is about to describe without writing, and get the same answer
/// the write will.
pub fn resolve_occurrence(
    event: &Event,
    occurrence: &str,
    tzid: Option<&str>,
) -> Result<DateTime<Utc>> {
    let parsed = util::parse_datetime(occurrence, tzid)?;
    // All-day times are anchored at UTC midnight so the date survives the round
    // trip; resolving their "day" through a zone would move it.
    let tz = match event.all_day {
        true => util::resolve_tz(None)?,
        false => util::resolve_tz(tzid)?,
    };

    // Expanded with its exclusions stripped: an occurrence the user already
    // cancelled is still one they can name, and telling them so beats claiming
    // the series never ran then. Whether it is live is the caller's question.
    let rule_only = Event {
        recur_source: ical::recur_source_without_exclusions(event.recur_source.as_deref()),
        ..event.clone()
    };
    // A day either side covers the local day whatever zone the series is
    // authored in, without having to reason about which one that is.
    let occurrences: Vec<DateTime<Utc>> = recur::expand_all(
        vec![rule_only],
        parsed.instant - Duration::days(1),
        parsed.instant + Duration::days(2),
    )
    .into_iter()
    .filter_map(|e| e.start.instant)
    .collect();

    if occurrences.contains(&parsed.instant) {
        return Ok(parsed.instant);
    }

    let day = |at: DateTime<Utc>| at.with_timezone(&tz).date_naive();
    let wanted = day(parsed.instant);
    let same_day: Vec<DateTime<Utc>> = occurrences
        .iter()
        .copied()
        .filter(|at| day(*at) == wanted)
        .collect();

    match same_day.as_slice() {
        [only] => Ok(*only),
        [] => Err(Error::Server(format!(
            "the series has no occurrence at {occurrence:?}"
        ))),
        many => Err(Error::Server(format!(
            "{occurrence:?} matches {} occurrences — name one exactly: {}",
            many.len(),
            many.iter()
                .map(|at| util::format_rfc3339(*at))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// A URL path segment for a calendar named `name`.
///
/// Lowercase ASCII alphanumerics and dashes only. Anything else — spaces,
/// punctuation, non-Latin scripts — collapses to a dash, so a name that reduces
/// to nothing falls back to a UUID rather than an empty segment. The display name
/// carries the real name; this only has to be a stable, addressable id.
fn slugify(name: &str) -> String {
    let mut slug = String::new();
    for ch in name.chars() {
        match ch {
            c if c.is_ascii_alphanumeric() => slug.push(c.to_ascii_lowercase()),
            _ if !slug.ends_with('-') => slug.push('-'),
            _ => {}
        }
    }
    let slug = slug.trim_matches('-').to_string();
    match slug.is_empty() {
        true => uuid::Uuid::new_v4().to_string(),
        false => slug,
    }
}

/// The properties a PROPPATCH refused, as `name: status`.
///
/// Empty for a success, and for the servers that answer a bare `200` with no
/// body at all — nothing to object to is not the same as a silent failure, and
/// only a propstat carrying a non-2xx status is evidence of one.
fn proppatch_failures(xml: &str) -> Vec<String> {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return Vec::new();
    };
    doc.descendants()
        .filter(|n| n.has_tag_name((DAV_NS, "propstat")))
        .filter_map(|propstat| {
            let status = propstat
                .children()
                .find(|n| n.has_tag_name((DAV_NS, "status")))
                .and_then(|n| n.text())?
                .trim();
            // "HTTP/1.1 403 Forbidden" — the code is the second token.
            let code: u16 = status.split_whitespace().nth(1)?.parse().ok()?;
            if (200..300).contains(&code) {
                return None;
            }
            let names: Vec<String> = propstat
                .children()
                .filter(|n| n.has_tag_name((DAV_NS, "prop")))
                .flat_map(|prop| prop.children().filter(|n| n.is_element()))
                .map(|n| n.tag_name().name().to_string())
                .collect();
            Some(format!(
                "{} ({status})",
                match names.is_empty() {
                    true => "unnamed property".to_string(),
                    false => names.join(", "),
                }
            ))
        })
        .collect()
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

/// Where an unaddressed event goes: the account's own default calendar, else
/// the first calendar it can actually be written to. A read-only default is
/// skipped — some servers keep pointing at a calendar the user has since lost
/// write access to — as is a task-only collection, which would swallow the
/// event somewhere no calendar app shows it.
/// Match a calendar in an already-fetched listing, by id, display name, or
/// href. Split out from [`CalDavClient::find_calendar`] so the GraphQL layer can
/// resolve names against its own per-request listing rather than the client's
/// process-lifetime cache.
pub fn resolve_calendar(calendars: &[Calendar], name: Option<&str>) -> Result<Calendar> {
    match name.map(str::trim).filter(|s| !s.is_empty()) {
        None => pick_default(calendars)
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

fn pick_default(calendars: &[Calendar]) -> Option<&Calendar> {
    let usable = |c: &&Calendar| !c.read_only && c.supports_events;
    calendars
        .iter()
        .find(|c| c.is_default && usable(c))
        .or_else(|| calendars.iter().find(usable))
        .or_else(|| calendars.first())
}

/// The account's default calendar for new events, from wherever the server
/// chose to answer `schedule-default-calendar-URL`.
///
/// Servers disagree on shape, so read the value rather than a fixed structure:
/// RFC 6638 wraps the URL in a `DAV:href`, iCloud puts the path straight in the
/// element, and a server echoes the property back empty in the 404 propstat of
/// every collection that hasn't got it — so the first element carrying the name
/// is not necessarily the one carrying the value.
fn parse_default_href(xml: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    doc.descendants()
        .filter(|n| n.has_tag_name((CALDAV_NS, "schedule-default-calendar-URL")))
        .find_map(|n| {
            n.descendants()
                .find(|c| c.has_tag_name((DAV_NS, "href")))
                .and_then(|h| h.text())
                .or_else(|| n.text())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .map(util::href_path)
}

/// The scheduling inbox collection, if this account has one.
fn parse_inbox_href(xml: &str, home: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    doc.descendants()
        .filter(|n| n.has_tag_name((DAV_NS, "response")))
        .find(|response| {
            response
                .descendants()
                .any(|n| n.has_tag_name((CALDAV_NS, "schedule-inbox")))
        })?
        .children()
        .find(|n| n.has_tag_name((DAV_NS, "href")))?
        .text()
        .map(|href| util::resolve_url(home, href))
}

/// Flag the calendar `href` points at.
///
/// Matches on the full path first, then on the last segment alone: a server is
/// free to answer with a different host or path prefix than the one it listed
/// calendars under, and the collection id is what survives that.
fn mark_default(calendars: &mut [Calendar], href: &str) {
    let target = util::href_path(href);
    let trimmed = target.trim_end_matches('/');
    if let Some(c) = calendars
        .iter_mut()
        .find(|c| c.href.trim_end_matches('/') == trimmed)
    {
        c.is_default = true;
        return;
    }
    let id = util::last_segment(&target);
    if !id.is_empty()
        && let Some(c) = calendars.iter_mut().find(|c| c.id == id)
    {
        c.is_default = true;
    }
}

fn parse_calendars(xml: &str, home: &str) -> Vec<Calendar> {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return Vec::new();
    };
    let home_path = util::href_path(home);
    let mut out = Vec::new();

    // `schedule-default-calendar-URL` hangs off the scheduling inbox, itself a
    // child of the calendar-home — so a Depth:1 listing carries it without an
    // extra round trip. The inbox is skipped as a calendar below (its
    // resourcetype is schedule-inbox), so scan the whole document for it.
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
            // Set afterwards by `mark_default` — which collection is the
            // default depends on a property that may not be in this document.
            is_default: false,
            // Resolve against the home URL, which carries the partition host
            // iCloud redirected us to — not the URL the user configured.
            url: util::resolve_url(home, &href),
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

        out.extend(
            ical::parse_events(data, &calendar.name, &calendar.href, href, etag)
                .into_iter()
                .map(|mut e| {
                    e.resource_url = util::resolve_url(&calendar.url, href);
                    e
                }),
        );
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
    <propstat><status>HTTP/1.1 404 Not Found</status><prop>
      <cal:schedule-default-calendar-URL/>
    </prop></propstat>
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
    <href>/1234/calendars/inbox/</href>
    <propstat><prop>
      <resourcetype><collection/><cal:schedule-inbox/></resourcetype>
      <cal:schedule-default-calendar-URL><href>/1234/calendars/home/</href></cal:schedule-default-calendar-URL>
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
        parse_listing(CALENDARS_XML)
    }

    /// The same pipeline `list_calendars` runs: parse, sort, then flag the
    /// default the server named.
    fn parse_listing(xml: &str) -> Vec<Calendar> {
        let mut c = parse_calendars(xml, "/1234/calendars/");
        c.sort_by_key(|c| c.name.to_lowercase());
        if let Some(href) = parse_default_href(xml) {
            mark_default(&mut c, &href);
        }
        c
    }

    /// iCloud omits the RFC 6638 `DAV:href` wrapper and puts the path straight
    /// in the element — and echoes the empty property into every other
    /// collection's 404 propstat first.
    #[test]
    fn reads_icloud_shaped_default_calendar_properties() {
        let xml = CALENDARS_XML
            .replace(
                "<cal:schedule-default-calendar-URL><href>/1234/calendars/home/</href></cal:schedule-default-calendar-URL>",
                "<cal:schedule-default-calendar-URL>/1234/calendars/home/</cal:schedule-default-calendar-URL>",
            );
        let cals = parse_listing(&xml);
        let default: Vec<&str> = cals
            .iter()
            .filter(|c| c.is_default)
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(default, ["home"]);
    }

    #[test]
    fn marks_the_calendar_the_server_calls_default() {
        let cals = calendars();
        let default: Vec<&str> = cals
            .iter()
            .filter(|c| c.is_default)
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(default, ["home"]);
        // The scheduling inbox carrying the property is not itself a calendar.
        assert!(!cals.iter().any(|c| c.id == "inbox"));
    }

    #[test]
    fn the_advertised_default_beats_the_first_writable_calendar() {
        let mut cals = calendars();
        cals.sort_by_key(|c| c.name.to_lowercase());
        // "Home" is not first alphabetically once a writable calendar precedes it.
        cals.insert(
            0,
            Calendar {
                id: "admin".into(),
                href: "/1234/calendars/admin/".into(),
                url: "/1234/calendars/admin/".into(),
                name: "Admin".into(),
                description: None,
                color: None,
                read_only: false,
                supports_events: true,
                is_default: false,
            },
        );
        assert_eq!(pick_default(&cals).unwrap().id, "home");
    }

    #[test]
    fn a_read_only_default_falls_through_to_a_writable_calendar() {
        let mut cals = calendars();
        for c in &mut cals {
            c.read_only = c.is_default;
        }
        let picked = pick_default(&cals).unwrap();
        assert!(!picked.read_only);
        assert_ne!(picked.id, "home");
    }

    #[test]
    fn no_default_property_leaves_every_calendar_unmarked() {
        let xml = CALENDARS_XML.replace("schedule-default-calendar-URL", "unrelated-prop");
        assert!(!parse_listing(&xml).iter().any(|c| c.is_default));
    }

    #[test]
    fn finds_the_scheduling_inbox_to_ask_when_the_listing_is_silent() {
        // Servers that only answer on the inbox: we must be able to find it.
        let inbox = parse_inbox_href(CALENDARS_XML, "https://p42.example.com/1234/calendars/");
        assert_eq!(
            inbox.as_deref(),
            Some("https://p42.example.com/1234/calendars/inbox/")
        );
    }

    #[test]
    fn matches_a_default_answered_on_another_host_or_prefix() {
        // The inbox may name the calendar by a URL that shares nothing with the
        // listing but the collection id.
        let mut cals = calendars();
        cals.iter_mut().for_each(|c| c.is_default = false);
        mark_default(&mut cals, "https://p99.example.com/other/prefix/home/");
        let default: Vec<&str> = cals
            .iter()
            .filter(|c| c.is_default)
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(default, ["home"]);
    }

    #[test]
    fn an_unknown_default_marks_nothing() {
        let mut cals = calendars();
        cals.iter_mut().for_each(|c| c.is_default = false);
        mark_default(&mut cals, "/1234/calendars/deleted-last-week/");
        assert!(!cals.iter().any(|c| c.is_default));
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

    #[test]
    fn slugs_are_readable_url_segments() {
        assert_eq!(slugify("Work Trips"), "work-trips");
        assert_eq!(slugify("Jane's  Stuff!"), "jane-s-stuff");
        assert_eq!(slugify("  Trim me  "), "trim-me");
        // Two separators in a row don't produce an empty path component.
        assert_eq!(slugify("a // b"), "a-b");
    }

    #[test]
    fn a_name_with_no_ascii_still_yields_an_addressable_segment() {
        // An empty path segment would address the collection's parent.
        let slug = slugify("日本語");
        assert!(!slug.is_empty());
        assert!(!slug.contains('/'));
    }

    #[test]
    fn a_proppatch_that_accepted_everything_reports_no_failures() {
        let xml = r#"<multistatus xmlns="DAV:"><response><href>/c/</href>
            <propstat><prop><displayname/></prop><status>HTTP/1.1 200 OK</status></propstat>
        </response></multistatus>"#;
        assert!(proppatch_failures(xml).is_empty());
    }

    #[test]
    fn a_refused_property_is_named_with_its_status() {
        // The whole point: this arrives inside a 207, which looks like a success.
        let xml = r#"<multistatus xmlns="DAV:" xmlns:ic="http://apple.com/ns/ical/">
          <response><href>/c/</href>
            <propstat><prop><displayname/></prop><status>HTTP/1.1 200 OK</status></propstat>
            <propstat><prop><ic:calendar-color/></prop><status>HTTP/1.1 403 Forbidden</status></propstat>
        </response></multistatus>"#;
        let failures = proppatch_failures(xml);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("calendar-color"));
        assert!(failures[0].contains("403"));
    }

    #[test]
    fn a_bodiless_success_is_not_read_as_a_failure() {
        // Plenty of servers answer PROPPATCH 200 with nothing at all.
        assert!(proppatch_failures("").is_empty());
        assert!(proppatch_failures("not xml at all").is_empty());
    }

    /// A daily series at 09:00 London, running from 24 July 2026.
    fn daily_series() -> Event {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:s\r\nSUMMARY:Standup\r\n\
                   DTSTART;TZID=Europe/London:20260724T090000\r\n\
                   DTEND;TZID=Europe/London:20260724T093000\r\n\
                   RRULE:FREQ=DAILY\r\nEND:VEVENT\r\nEND:VCALENDAR";
        ical::parse_events(ics, "Home", "/c/", "/c/s.ics", None)
            .into_iter()
            .next()
            .unwrap()
    }

    #[test]
    fn a_bare_date_resolves_to_that_days_occurrence() {
        // The caller doesn't have to know the series runs at 09:00 London.
        let at = resolve_occurrence(&daily_series(), "2026-07-29", Some("Europe/London")).unwrap();
        assert_eq!(util::format_rfc3339(at), "2026-07-29T08:00:00Z");
    }

    #[test]
    fn an_exact_instant_resolves_to_itself() {
        let at = resolve_occurrence(
            &daily_series(),
            "2026-07-29T08:00:00Z",
            Some("Europe/London"),
        )
        .unwrap();
        assert_eq!(util::format_rfc3339(at), "2026-07-29T08:00:00Z");
    }

    #[test]
    fn a_time_that_misses_by_a_little_still_finds_the_days_only_occurrence() {
        // Forgiving because the caller then sees the resolved instant in a
        // preview before agreeing to anything.
        let at =
            resolve_occurrence(&daily_series(), "2026-07-29 17:00", Some("Europe/London")).unwrap();
        assert_eq!(util::format_rfc3339(at), "2026-07-29T08:00:00Z");
    }

    #[test]
    fn a_day_the_series_does_not_run_is_refused() {
        // The series starts on the 24th.
        let err = resolve_occurrence(&daily_series(), "2026-07-20", Some("Europe/London"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no occurrence"), "unhelpful: {err}");
    }

    #[test]
    fn several_occurrences_in_one_day_are_refused_with_the_candidates() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:s\r\nSUMMARY:Hourly\r\n\
                   DTSTART:20260724T090000Z\r\nDTEND:20260724T093000Z\r\n\
                   RRULE:FREQ=HOURLY;COUNT=6\r\nEND:VEVENT\r\nEND:VCALENDAR";
        let event = ical::parse_events(ics, "Home", "/c/", "/c/s.ics", None)
            .into_iter()
            .next()
            .unwrap();

        let err = resolve_occurrence(&event, "2026-07-24", None)
            .unwrap_err()
            .to_string();
        // Guessing which of six to cancel is not a recoverable mistake.
        assert!(err.contains("matches 6 occurrences"), "unhelpful: {err}");
        assert!(err.contains("2026-07-24T09:00:00Z"), "no candidates: {err}");
    }
}
