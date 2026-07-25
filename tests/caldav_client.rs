//! End-to-end tests against a simulated CalDAV server.
//!
//! These exercise the parts unit tests can't reach: the discovery walk, the
//! request bodies we actually put on the wire, and the read → mutate → re-read
//! round trip.

use caldav_cli::caldav::CalDavClient;
use caldav_cli::models::EventFields;
use chrono::{TimeZone, Utc};
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const PRINCIPAL_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:">
  <response>
    <href>/</href>
    <propstat><prop>
      <current-user-principal><href>/1234/principal/</href></current-user-principal>
    </prop><status>HTTP/1.1 200 OK</status></propstat>
  </response>
</multistatus>"#;

const HOME_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <response>
    <href>/1234/principal/</href>
    <propstat><prop>
      <c:calendar-home-set><href>/1234/calendars/</href></c:calendar-home-set>
    </prop><status>HTTP/1.1 200 OK</status></propstat>
  </response>
</multistatus>"#;

const CALENDARS_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:ic="http://apple.com/ns/ical/">
  <response>
    <href>/1234/calendars/</href>
    <propstat><prop><resourcetype><collection/></resourcetype></prop></propstat>
  </response>
  <response>
    <href>/1234/calendars/home/</href>
    <propstat><prop>
      <displayname>Home</displayname>
      <resourcetype><collection/><c:calendar/></resourcetype>
      <ic:calendar-color>#FF2968</ic:calendar-color>
      <c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set>
      <current-user-privilege-set>
        <privilege><read/></privilege><privilege><write/></privilege>
      </current-user-privilege-set>
    </prop></propstat>
  </response>
  <response>
    <href>/1234/calendars/team/</href>
    <propstat><prop>
      <displayname>Team</displayname>
      <resourcetype><collection/><c:calendar/></resourcetype>
      <c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set>
      <current-user-privilege-set><privilege><read/></privilege></current-user-privilege-set>
    </prop></propstat>
  </response>
</multistatus>"#;

fn events_xml(ics: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <response>
    <href>/1234/calendars/home/evt-1.ics</href>
    <propstat><prop>
      <getetag>"etag-1"</getetag>
      <c:calendar-data>{ics}</c:calendar-data>
    </prop><status>HTTP/1.1 200 OK</status></propstat>
  </response>
</multistatus>"#
    )
}

const STANDUP_ICS: &str = "BEGIN:VCALENDAR
VERSION:2.0
BEGIN:VEVENT
UID:evt-1
SUMMARY:Standup
LOCATION:Room 4
DTSTART;TZID=Europe/London:20260724T090000
DTEND;TZID=Europe/London:20260724T093000
END:VEVENT
END:VCALENDAR";

const EMPTY_MULTISTATUS: &str =
    r#"<?xml version="1.0" encoding="UTF-8"?><multistatus xmlns="DAV:"></multistatus>"#;

fn ok(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(207)
        .set_body_string(body)
        .insert_header("Content-Type", "application/xml")
}

/// Mount the three discovery responses every test needs.
async fn mount_discovery(server: &MockServer) {
    Mock::given(method("PROPFIND"))
        .and(path("/"))
        .respond_with(ok(PRINCIPAL_XML))
        .mount(server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/1234/principal/"))
        .respond_with(ok(HOME_XML))
        .mount(server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/1234/calendars/"))
        .respond_with(ok(CALENDARS_XML))
        .mount(server)
        .await;
}

fn client(server: &MockServer) -> CalDavClient {
    CalDavClient::new(server.uri(), "me@example.com".into(), "app-password".into())
}

fn window() -> (chrono::DateTime<Utc>, chrono::DateTime<Utc>) {
    (
        Utc.with_ymd_and_hms(2026, 7, 24, 0, 0, 0).unwrap(),
        Utc.with_ymd_and_hms(2026, 7, 25, 0, 0, 0).unwrap(),
    )
}

#[tokio::test]
async fn discovers_calendars_through_the_principal_walk() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;

    let calendars = client(&server).list_calendars().await.unwrap().to_vec();

    assert_eq!(calendars.len(), 2);
    let home = calendars.iter().find(|c| c.id == "home").unwrap();
    assert_eq!(home.name, "Home");
    assert_eq!(home.color.as_deref(), Some("#FF2968"));
    assert!(!home.read_only);
    assert!(calendars.iter().find(|c| c.id == "team").unwrap().read_only);
}

#[tokio::test]
async fn discovery_falls_back_from_well_known_to_root() {
    let server = MockServer::start().await;
    // The well-known path 404s, as it does on several real servers.
    Mock::given(method("PROPFIND"))
        .and(path("/.well-known/caldav"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    mount_discovery(&server).await;

    assert_eq!(client(&server).list_calendars().await.unwrap().len(), 2);
}

#[tokio::test]
async fn sends_basic_auth_on_every_request() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        // base64("me@example.com:app-password")
        .and(header(
            "authorization",
            "Basic bWVAZXhhbXBsZS5jb206YXBwLXBhc3N3b3Jk",
        ))
        .and(path("/"))
        .respond_with(ok(PRINCIPAL_XML))
        .mount(&server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/1234/principal/"))
        .respond_with(ok(HOME_XML))
        .mount(&server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/1234/calendars/"))
        .respond_with(ok(CALENDARS_XML))
        .mount(&server)
        .await;

    assert!(client(&server).list_calendars().await.is_ok());
}

#[tokio::test]
async fn rejected_credentials_surface_as_an_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let err = client(&server).list_calendars().await.unwrap_err();
    // A 401 during discovery must surface as an auth problem, not the generic
    // "discovery failed" — the password is what the user needs to go fix.
    let msg = err.to_string();
    assert!(
        msg.contains("Invalid credentials"),
        "unhelpful error: {msg}"
    );
}

#[tokio::test]
async fn lists_events_in_a_window_with_a_time_range_filter() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/home/"))
        .and(body_string_contains("VEVENT"))
        .and(body_string_contains("time-range"))
        .respond_with(ok(&events_xml(STANDUP_ICS)))
        .mount(&server)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/team/"))
        .respond_with(ok(EMPTY_MULTISTATUS))
        .mount(&server)
        .await;

    let (start, end) = window();
    let events = client(&server)
        .events_in_range(None, start, end, true, 50)
        .await
        .unwrap();

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].summary.as_deref(), Some("Standup"));
    assert_eq!(events[0].calendar, "Home");
    // 09:00 BST normalised to UTC.
    assert_eq!(
        events[0].start.date_time.as_deref(),
        Some("2026-07-24T08:00:00Z")
    );
    assert_eq!(events[0].etag.as_deref(), Some("\"etag-1\""));
}

#[tokio::test]
async fn requests_expansion_and_falls_back_when_the_server_rejects_it() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;
    // Servers that don't implement <expand> reject the whole REPORT.
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/home/"))
        .and(body_string_contains("<c:expand"))
        .respond_with(ResponseTemplate::new(400).set_body_string("expand not supported"))
        .mount(&server)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/home/"))
        .respond_with(ok(&events_xml(STANDUP_ICS)))
        .mount(&server)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/team/"))
        .respond_with(ok(EMPTY_MULTISTATUS))
        .mount(&server)
        .await;

    let (start, end) = window();
    let events = client(&server)
        .events_in_range(None, start, end, true, 50)
        .await
        .unwrap();

    // The retry without <expand> still returns the event.
    assert_eq!(events.len(), 1);
}

#[tokio::test]
async fn one_failing_calendar_does_not_sink_the_whole_range() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/home/"))
        .respond_with(ok(&events_xml(STANDUP_ICS)))
        .mount(&server)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/team/"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let (start, end) = window();
    let events = client(&server)
        .events_in_range(None, start, end, true, 50)
        .await
        .unwrap();

    assert_eq!(events.len(), 1);
}

#[tokio::test]
async fn creates_an_event_and_puts_valid_icalendar() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;
    Mock::given(method("PUT"))
        .and(header("if-none-match", "*"))
        .and(body_string_contains("BEGIN:VEVENT"))
        .and(body_string_contains("SUMMARY:Coffee"))
        .and(body_string_contains(
            "DTSTART;TZID=Europe/London:20260724T150000",
        ))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;

    let fields = EventFields {
        summary: Some("Coffee"),
        start: Some("2026-07-24 15:00"),
        duration_minutes: Some(30),
        tzid: Some("Europe/London"),
        location: Some("The usual"),
        ..Default::default()
    };
    let event = client(&server)
        .create_event(Some("Home"), &fields)
        .await
        .unwrap();

    assert_eq!(event.summary.as_deref(), Some("Coffee"));
    assert_eq!(event.calendar, "Home");
    // 15:00 BST is 14:00Z.
    assert_eq!(
        event.start.date_time.as_deref(),
        Some("2026-07-24T14:00:00Z")
    );
    assert_eq!(event.end.date_time.as_deref(), Some("2026-07-24T14:30:00Z"));
    // The event was addressed under the chosen calendar.
    assert!(event.href.starts_with("/1234/calendars/home/"));
}

#[tokio::test]
async fn refuses_to_create_in_a_read_only_calendar() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;

    let fields = EventFields {
        summary: Some("Nope"),
        start: Some("2026-07-24T09:00:00Z"),
        ..Default::default()
    };
    let err = client(&server)
        .create_event(Some("Team"), &fields)
        .await
        .unwrap_err();

    assert!(err.to_string().contains("read-only"));
}

#[tokio::test]
async fn create_defaults_to_the_first_writable_calendar() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;

    let fields = EventFields {
        summary: Some("Anywhere"),
        start: Some("2026-07-24T09:00:00Z"),
        ..Default::default()
    };
    let event = client(&server).create_event(None, &fields).await.unwrap();

    // "Home" is writable; "Team" is not.
    assert_eq!(event.calendar, "Home");
}

#[tokio::test]
async fn updates_merge_over_stored_values_and_send_if_match() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/home/"))
        .respond_with(ok(&events_xml(STANDUP_ICS)))
        .mount(&server)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/team/"))
        .respond_with(ok(EMPTY_MULTISTATUS))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/1234/calendars/home/evt-1.ics"))
        // Optimistic concurrency against the etag we read.
        .and(header("if-match", "\"etag-1\""))
        .and(body_string_contains("SUMMARY:Standup (moved)"))
        // Untouched fields survive the merge.
        .and(body_string_contains("LOCATION:Room 4"))
        // SEQUENCE is bumped so attendees' clients see the change.
        .and(body_string_contains("SEQUENCE:1"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let fields = EventFields {
        summary: Some("Standup (moved)"),
        ..Default::default()
    };
    let event = client(&server)
        .update_event("evt-1", None, &fields)
        .await
        .unwrap();

    assert_eq!(event.summary.as_deref(), Some("Standup (moved)"));
    assert_eq!(event.location.as_deref(), Some("Room 4"));
    // Start was not touched, so it keeps its stored value.
    assert_eq!(
        event.start.date_time.as_deref(),
        Some("2026-07-24T08:00:00Z")
    );
}

#[tokio::test]
async fn moving_an_event_preserves_its_duration() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/home/"))
        .respond_with(ok(&events_xml(STANDUP_ICS)))
        .mount(&server)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/team/"))
        .respond_with(ok(EMPTY_MULTISTATUS))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    // The stored event is 30 minutes long; only the start moves.
    let fields = EventFields {
        start: Some("2026-07-24T14:00:00Z"),
        ..Default::default()
    };
    let event = client(&server)
        .update_event("evt-1", None, &fields)
        .await
        .unwrap();

    assert_eq!(
        event.start.date_time.as_deref(),
        Some("2026-07-24T14:00:00Z")
    );
    assert_eq!(event.end.date_time.as_deref(), Some("2026-07-24T15:00:00Z"));
}

#[tokio::test]
async fn a_concurrent_edit_is_reported_not_clobbered() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/home/"))
        .respond_with(ok(&events_xml(STANDUP_ICS)))
        .mount(&server)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/team/"))
        .respond_with(ok(EMPTY_MULTISTATUS))
        .mount(&server)
        .await;
    // The If-Match no longer matches: someone else changed the event.
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(412))
        .mount(&server)
        .await;

    let fields = EventFields {
        summary: Some("Mine"),
        ..Default::default()
    };
    let err = client(&server)
        .update_event("evt-1", None, &fields)
        .await
        .unwrap_err();

    assert!(err.to_string().contains("changed on the server"));
}

#[tokio::test]
async fn deletes_by_uid_and_returns_what_was_removed() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/home/"))
        .respond_with(ok(&events_xml(STANDUP_ICS)))
        .mount(&server)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/team/"))
        .respond_with(ok(EMPTY_MULTISTATUS))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/1234/calendars/home/evt-1.ics"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let deleted = client(&server).delete_event("evt-1", None).await.unwrap();
    assert_eq!(deleted.summary.as_deref(), Some("Standup"));
}

#[tokio::test]
async fn deleting_an_unknown_uid_is_an_error_not_a_silent_success() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;
    Mock::given(method("REPORT"))
        .respond_with(ok(EMPTY_MULTISTATUS))
        .mount(&server)
        .await;

    let err = client(&server)
        .delete_event("does-not-exist", None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("does-not-exist"));
}

#[tokio::test]
async fn get_event_matches_the_uid_exactly() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;
    // A server whose text-match is a substring match returns a near-miss too.
    let both = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <response><href>/1234/calendars/home/evt-10.ics</href><propstat><prop>
    <c:calendar-data>BEGIN:VCALENDAR
BEGIN:VEVENT
UID:evt-10
SUMMARY:Wrong one
DTSTART:20260724T090000Z
END:VEVENT
END:VCALENDAR</c:calendar-data>
  </prop></propstat></response>
  <response><href>/1234/calendars/home/evt-1.ics</href><propstat><prop>
    <c:calendar-data>{STANDUP_ICS}</c:calendar-data>
  </prop></propstat></response>
</multistatus>"#
    );
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/home/"))
        .respond_with(ok(&both))
        .mount(&server)
        .await;

    let event = client(&server)
        .get_event("evt-1", Some("Home"))
        .await
        .unwrap()
        .expect("found");
    assert_eq!(event.id, "evt-1");
    assert_eq!(event.summary.as_deref(), Some("Standup"));
}

#[tokio::test]
async fn search_filters_the_window_client_side() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/home/"))
        .respond_with(ok(&events_xml(STANDUP_ICS)))
        .mount(&server)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/team/"))
        .respond_with(ok(EMPTY_MULTISTATUS))
        .mount(&server)
        .await;

    let (start, end) = window();
    let c = client(&server);

    // Matches the location, not just the title.
    assert_eq!(
        c.search_events("room 4", None, start, end, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        c.search_events("dentist", None, start, end, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn free_busy_falls_back_to_events_when_the_report_is_unsupported() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;
    // iCloud answers the free-busy REPORT with an error on a personal home.
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/home/"))
        .respond_with(ok(&events_xml(STANDUP_ICS)))
        .mount(&server)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/team/"))
        .respond_with(ok(EMPTY_MULTISTATUS))
        .mount(&server)
        .await;

    let (start, end) = window();
    let periods = client(&server).free_busy(start, end).await.unwrap();

    assert_eq!(periods.len(), 1);
    assert_eq!(periods[0].start, "2026-07-24T08:00:00Z");
    assert_eq!(periods[0].end, "2026-07-24T08:30:00Z");
    assert_eq!(periods[0].status, "BUSY");
}

#[tokio::test]
async fn free_busy_prefers_the_servers_own_report() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "BEGIN:VCALENDAR\r\nBEGIN:VFREEBUSY\r\n\
             FREEBUSY;FBTYPE=BUSY:20260724T110000Z/20260724T120000Z\r\n\
             END:VFREEBUSY\r\nEND:VCALENDAR\r\n",
        ))
        .mount(&server)
        .await;

    let (start, end) = window();
    let periods = client(&server).free_busy(start, end).await.unwrap();

    assert_eq!(periods.len(), 1);
    assert_eq!(periods[0].start, "2026-07-24T11:00:00Z");
}

/// The principal walk is cached for the client's life; the calendar listing is
/// deliberately not.
///
/// Caching the listing meant a pooled client — which is what the MCP server
/// holds — never saw a calendar created, renamed, or deleted after start-up.
/// Deduplicating it belongs to the caller's scope (a per-request loader in
/// GraphQL, a one-command process in the CLI), not to the connection.
#[tokio::test]
async fn discovery_is_cached_but_the_calendar_listing_is_refetched() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;
    Mock::given(method("REPORT"))
        .respond_with(ok(EMPTY_MULTISTATUS))
        .mount(&server)
        .await;

    let c = client(&server);
    let (start, end) = window();
    for _ in 0..3 {
        c.events_in_range(None, start, end, true, 10).await.unwrap();
    }

    let propfinds: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r: &Request| r.method.as_str() == "PROPFIND")
        .map(|r| r.url.path().to_string())
        .collect();

    // The well-known probe, the root, and the principal — once for the loop,
    // not once per call.
    let walk = propfinds
        .iter()
        .filter(|p| p.as_str() != "/1234/calendars/")
        .count();
    assert_eq!(walk, 3, "{propfinds:?}");

    // The listing, once per call, so each one sees current calendars.
    let listings = propfinds
        .iter()
        .filter(|p| p.as_str() == "/1234/calendars/")
        .count();
    assert_eq!(listings, 3, "{propfinds:?}");
}

/// iCloud shards accounts onto partition hosts: you authenticate against
/// `caldav.icloud.com` but your calendar home comes back on
/// `pNN-caldav.icloud.com`, and every subsequent request must address *that*
/// host. Two mock servers stand in for the two hosts.
#[tokio::test]
async fn follows_the_calendar_home_onto_a_different_host() {
    let entry = MockServer::start().await;
    let partition = MockServer::start().await;

    // Entry host: answers the principal lookup, and nothing else.
    Mock::given(method("PROPFIND"))
        .and(path("/"))
        .respond_with(ok(PRINCIPAL_XML))
        .mount(&entry)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/1234/principal/"))
        .respond_with(ok(&format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <response><href>/1234/principal/</href><propstat><prop>
    <c:calendar-home-set><href>{}/1234/calendars/</href></c:calendar-home-set>
  </prop><status>HTTP/1.1 200 OK</status></propstat></response>
</multistatus>"#,
            partition.uri()
        )))
        .mount(&entry)
        .await;

    // Partition host: everything from the home set onward lives here.
    Mock::given(method("PROPFIND"))
        .and(path("/1234/calendars/"))
        .respond_with(ok(CALENDARS_XML))
        .mount(&partition)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/home/"))
        .respond_with(ok(&events_xml(STANDUP_ICS)))
        .mount(&partition)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/team/"))
        .respond_with(ok(EMPTY_MULTISTATUS))
        .mount(&partition)
        .await;
    Mock::given(method("PUT"))
        .and(path("/1234/calendars/home/evt-1.ics"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&partition)
        .await;

    // The client is only ever told about the entry host.
    let client = CalDavClient::new(entry.uri(), "me@example.com".into(), "app-password".into());
    let (start, end) = window();

    let events = client
        .events_in_range(None, start, end, true, 50)
        .await
        .unwrap();
    assert_eq!(events.len(), 1, "reads must follow onto the partition host");

    // Writes too — the PUT is only mounted on the partition host.
    let updated = client
        .update_event(
            "evt-1",
            None,
            &EventFields {
                summary: Some("Moved"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.summary.as_deref(), Some("Moved"));

    // The entry host must have seen discovery only — no calendar traffic.
    let entry_paths: Vec<String> = entry
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r: &Request| r.url.path().to_string())
        .collect();
    assert!(
        !entry_paths.iter().any(|p| p.contains("/calendars/")),
        "calendar traffic leaked to the entry host: {entry_paths:?}"
    );
}

#[tokio::test]
async fn sends_a_user_agent() {
    // iCloud returns 403 to a request with no User-Agent, and reqwest sends
    // none unless asked.
    let server = MockServer::start().await;
    mount_discovery(&server).await;

    client(&server).list_calendars().await.unwrap();

    let ua = server.received_requests().await.unwrap()[0]
        .headers
        .get("user-agent")
        .map(|v| v.to_str().unwrap().to_string());
    assert!(
        ua.as_deref().is_some_and(|v| v.starts_with("caldav/")),
        "expected a caldav User-Agent, got {ua:?}"
    );
}

const WEEKLY_ICS: &str = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:standup\r\nSUMMARY:Standup\r\n\
DTSTART;TZID=Europe/London:20260706T090000\r\nDTEND;TZID=Europe/London:20260706T093000\r\n\
RRULE:FREQ=WEEKLY;BYDAY=MO\r\nEND:VEVENT\r\nEND:VCALENDAR";

/// The important half of the expansion story: when a server refuses
/// `<C:expand>` (iCloud is unreliable here), the caller still gets one result
/// per occurrence — expanded client-side — not a lone master event.
#[tokio::test]
async fn expansion_falls_back_to_the_client_and_still_yields_occurrences() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/home/"))
        .and(body_string_contains("<c:expand"))
        .respond_with(ResponseTemplate::new(403).set_body_string("expand not supported"))
        .mount(&server)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/home/"))
        .respond_with(ok(&events_xml(WEEKLY_ICS)))
        .mount(&server)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/team/"))
        .respond_with(ok(EMPTY_MULTISTATUS))
        .mount(&server)
        .await;

    let events = client(&server)
        .events_in_range(
            None,
            Utc.with_ymd_and_hms(2026, 7, 6, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 7, 21, 0, 0, 0).unwrap(),
            true,
            50,
        )
        .await
        .unwrap();

    // Three Mondays, each an occurrence of the same series.
    assert_eq!(events.len(), 3);
    assert!(events.iter().all(|e| e.id == "standup"));
    let starts: Vec<&str> = events
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
}

/// With expansion off, the same series comes back as the single master event
/// carrying its rule — what you need before editing the series.
#[tokio::test]
async fn unexpanded_queries_return_the_master_event() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/home/"))
        .respond_with(ok(&events_xml(WEEKLY_ICS)))
        .mount(&server)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/team/"))
        .respond_with(ok(EMPTY_MULTISTATUS))
        .mount(&server)
        .await;

    let events = client(&server)
        .events_in_range(
            None,
            Utc.with_ymd_and_hms(2026, 7, 6, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 7, 21, 0, 0, 0).unwrap(),
            false,
            50,
        )
        .await
        .unwrap();

    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].recurrence.as_deref(),
        Some("FREQ=WEEKLY;BYDAY=MO")
    );
    assert!(events[0].recurrence_id.is_none());
}

// ============ Recurrence exceptions ============

/// A weekly series that already has one occurrence cancelled — the shape that
/// used to lose its exception on every unrelated edit.
const SERIES_ICS: &str = "BEGIN:VCALENDAR
VERSION:2.0
BEGIN:VEVENT
UID:evt-1
SUMMARY:Standup
DTSTART;TZID=Europe/London:20260724T090000
DTEND;TZID=Europe/London:20260724T093000
RRULE:FREQ=DAILY
EXDATE;TZID=Europe/London:20260727T090000
END:VEVENT
END:VCALENDAR";

/// Mount discovery plus a series in Home, and capture what gets PUT back.
async fn mount_series(server: &MockServer) {
    mount_discovery(server).await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/home/"))
        .respond_with(ok(&events_xml(SERIES_ICS)))
        .mount(server)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/team/"))
        .respond_with(ok(EMPTY_MULTISTATUS))
        .mount(server)
        .await;
}

/// The body of the single PUT the test provoked.
async fn put_body(server: &MockServer) -> String {
    let requests = server.received_requests().await.unwrap();
    let put = requests
        .iter()
        .find(|r| r.method == "PUT")
        .expect("no PUT was made");
    String::from_utf8(put.body.clone()).unwrap()
}

#[tokio::test]
async fn an_unrelated_edit_keeps_the_occurrences_the_user_cancelled() {
    let server = MockServer::start().await;
    mount_series(&server).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let fields = EventFields {
        location: Some("Room 5"),
        ..Default::default()
    };
    client(&server)
        .update_event("evt-1", None, &fields)
        .await
        .unwrap();

    // Rebuilding the series without its EXDATE resurrected every cancelled
    // occurrence — silently, on an edit that had nothing to do with recurrence.
    let body = put_body(&server).await;
    assert!(
        body.contains("EXDATE;TZID=Europe/London:20260727T090000"),
        "the exception was dropped: {body}"
    );
    assert!(body.contains("RRULE:FREQ=DAILY"));
}

#[tokio::test]
async fn replacing_the_rule_drops_exceptions_to_the_old_one() {
    let server = MockServer::start().await;
    mount_series(&server).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let fields = EventFields {
        recurrence: Some("FREQ=WEEKLY;BYDAY=TH"),
        ..Default::default()
    };
    client(&server)
        .update_event("evt-1", None, &fields)
        .await
        .unwrap();

    // The old exception described a daily occurrence that no longer exists.
    let body = put_body(&server).await;
    assert!(body.contains("RRULE:FREQ=WEEKLY;BYDAY=TH"));
    assert!(!body.contains("EXDATE"), "stale exception kept: {body}");
}

#[tokio::test]
async fn cancelling_one_occurrence_adds_an_exdate_matching_the_series() {
    let server = MockServer::start().await;
    mount_series(&server).await;
    Mock::given(method("PUT"))
        .and(header("if-match", "\"etag-1\""))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    // A bare date: the series runs once that day, so it is unambiguous.
    client(&server)
        .exclude_occurrence("evt-1", None, "2026-07-29")
        .await
        .unwrap();

    let body = put_body(&server).await;
    // Written in the series' own zone and value type — servers ignore an EXDATE
    // whose form doesn't match the DTSTART it qualifies.
    assert!(
        body.contains("EXDATE;TZID=Europe/London:20260729T090000"),
        "wrong EXDATE form: {body}"
    );
    // And the one that was already there survives.
    assert!(body.contains("EXDATE;TZID=Europe/London:20260727T090000"));
    assert!(body.contains("RRULE:FREQ=DAILY"));
}

#[tokio::test]
async fn cancelling_an_already_cancelled_occurrence_is_refused() {
    let server = MockServer::start().await;
    mount_series(&server).await;

    let err = client(&server)
        .exclude_occurrence("evt-1", None, "2026-07-27")
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("already cancelled"), "unhelpful: {err}");
}

#[tokio::test]
async fn cancelling_an_occurrence_of_a_one_off_is_refused() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/home/"))
        .respond_with(ok(&events_xml(STANDUP_ICS)))
        .mount(&server)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/team/"))
        .respond_with(ok(EMPTY_MULTISTATUS))
        .mount(&server)
        .await;

    let err = client(&server)
        .exclude_occurrence("evt-1", None, "2026-07-24")
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("not a recurring series"), "unhelpful: {err}");
    // And nothing was written.
    let requests = server.received_requests().await.unwrap();
    assert!(!requests.iter().any(|r| r.method == "PUT"));
}

#[tokio::test]
async fn a_date_naming_no_occurrence_is_refused() {
    let server = MockServer::start().await;
    mount_series(&server).await;

    // The series is daily from the 24th; nothing runs before it starts.
    let err = client(&server)
        .exclude_occurrence("evt-1", None, "2026-07-20")
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("no occurrence"), "unhelpful: {err}");
}

// ============ Replying to invitations ============

/// An invitation with two attendees, one of them us.
const INVITE_ICS: &str = "BEGIN:VCALENDAR
VERSION:2.0
BEGIN:VEVENT
UID:evt-1
SUMMARY:Design review
DTSTART:20260724T090000Z
DTEND:20260724T100000Z
ORGANIZER;CN=Alice:mailto:alice@example.com
ATTENDEE;CN=Alice;PARTSTAT=ACCEPTED:mailto:alice@example.com
ATTENDEE;CN=Me;PARTSTAT=NEEDS-ACTION:mailto:me@example.com
END:VEVENT
END:VCALENDAR";

async fn mount_event(server: &MockServer, ics: &str) {
    mount_discovery(server).await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/home/"))
        .respond_with(ok(&events_xml(ics)))
        .mount(server)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/team/"))
        .respond_with(ok(EMPTY_MULTISTATUS))
        .mount(server)
        .await;
}

#[tokio::test]
async fn replying_sets_only_your_own_partstat() {
    let server = MockServer::start().await;
    mount_event(&server, INVITE_ICS).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let event = client(&server)
        .respond_to_invite("evt-1", None, "DECLINED", None)
        .await
        .unwrap();

    let body = put_body(&server).await;
    assert!(body.contains("PARTSTAT=DECLINED:mailto:me@example.com"));
    // The organiser's own acceptance is not ours to change.
    assert!(body.contains("PARTSTAT=ACCEPTED:mailto:alice@example.com"));
    // SEQUENCE bumps so the organiser's client sees a new revision.
    assert!(body.contains("SEQUENCE:1"));
    let me = event
        .attendees
        .iter()
        .find(|a| a.email == "me@example.com")
        .unwrap();
    assert_eq!(me.status.as_deref(), Some("DECLINED"));
}

#[tokio::test]
async fn replying_as_an_alias_targets_that_row() {
    let server = MockServer::start().await;
    mount_event(&server, INVITE_ICS).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    // The login is me@example.com; reply as the organiser's row instead.
    client(&server)
        .respond_to_invite("evt-1", None, "TENTATIVE", Some("alice@example.com"))
        .await
        .unwrap();

    let body = put_body(&server).await;
    assert!(body.contains("PARTSTAT=TENTATIVE:mailto:alice@example.com"));
    assert!(body.contains("PARTSTAT=NEEDS-ACTION:mailto:me@example.com"));
}

#[tokio::test]
async fn replying_to_an_event_you_are_not_on_lists_who_is() {
    let server = MockServer::start().await;
    mount_event(&server, INVITE_ICS).await;

    let err = client(&server)
        .respond_to_invite("evt-1", None, "ACCEPTED", Some("nobody@example.com"))
        .await
        .unwrap_err()
        .to_string();

    // The addresses are the actionable part: they're what the caller picks from.
    assert!(err.contains("not on the attendee list"), "unhelpful: {err}");
    assert!(err.contains("alice@example.com"), "no candidates: {err}");
    let requests = server.received_requests().await.unwrap();
    assert!(!requests.iter().any(|r| r.method == "PUT"));
}

// ============ Moving events between calendars ============

/// Home is writable, Team is read-only, so make a second writable target.
const THREE_CALENDARS_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:ic="http://apple.com/ns/ical/">
  <response>
    <href>/1234/calendars/home/</href>
    <propstat><prop>
      <displayname>Home</displayname>
      <resourcetype><collection/><c:calendar/></resourcetype>
      <c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set>
      <current-user-privilege-set><privilege><read/></privilege><privilege><write/></privilege></current-user-privilege-set>
    </prop></propstat>
  </response>
  <response>
    <href>/1234/calendars/work/</href>
    <propstat><prop>
      <displayname>Work</displayname>
      <resourcetype><collection/><c:calendar/></resourcetype>
      <c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set>
      <current-user-privilege-set><privilege><read/></privilege><privilege><write/></privilege></current-user-privilege-set>
    </prop></propstat>
  </response>
  <response>
    <href>/1234/calendars/team/</href>
    <propstat><prop>
      <displayname>Team</displayname>
      <resourcetype><collection/><c:calendar/></resourcetype>
      <c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set>
      <current-user-privilege-set><privilege><read/></privilege></current-user-privilege-set>
    </prop></propstat>
  </response>
</multistatus>"#;

/// Discovery with three calendars, the event living in Home.
async fn mount_for_move(server: &MockServer) {
    Mock::given(method("PROPFIND"))
        .and(path("/"))
        .respond_with(ok(PRINCIPAL_XML))
        .mount(server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/1234/principal/"))
        .respond_with(ok(HOME_XML))
        .mount(server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/1234/calendars/"))
        .respond_with(ok(THREE_CALENDARS_XML))
        .mount(server)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/home/"))
        .respond_with(ok(&events_xml(STANDUP_ICS)))
        .mount(server)
        .await;
    for empty in ["/1234/calendars/work/", "/1234/calendars/team/"] {
        Mock::given(method("REPORT"))
            .and(path(empty))
            .respond_with(ok(EMPTY_MULTISTATUS))
            .mount(server)
            .await;
    }
}

#[tokio::test]
async fn moving_an_event_uses_webdav_move() {
    let server = MockServer::start().await;
    mount_for_move(&server).await;
    Mock::given(method("MOVE"))
        .and(path("/1234/calendars/home/evt-1.ics"))
        .and(header(
            "destination",
            format!("{}/1234/calendars/work/evt-1.ics", server.uri()).as_str(),
        ))
        // Never silently replace whatever is already at the destination.
        .and(header("overwrite", "F"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;

    let event = client(&server)
        .move_event("evt-1", None, "Work")
        .await
        .unwrap();

    assert_eq!(event.calendar, "Work");
    assert_eq!(event.href, "/1234/calendars/work/evt-1.ics");
    // The UID and the contents are the point of moving rather than recreating.
    assert_eq!(event.id, "evt-1");
    assert_eq!(event.location.as_deref(), Some("Room 4"));
    // The etag belonged to the old path; keeping it would break the next write.
    assert!(event.etag.is_none());
}

#[tokio::test]
async fn a_server_without_move_falls_back_to_copying_the_raw_resource() {
    let server = MockServer::start().await;
    mount_for_move(&server).await;
    Mock::given(method("MOVE"))
        .respond_with(ResponseTemplate::new(501))
        .mount(&server)
        .await;
    // The fallback reads the stored bytes rather than rebuilding from the model,
    // so anything we don't parse — here a VALARM — survives the move.
    Mock::given(method("GET"))
        .and(path("/1234/calendars/home/evt-1.ics"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(STANDUP_ICS.replace(
                "END:VEVENT",
                "BEGIN:VALARM\nTRIGGER:-PT15M\nACTION:DISPLAY\nEND:VALARM\nEND:VEVENT",
            )),
        )
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/1234/calendars/work/evt-1.ics"))
        .and(header("if-none-match", "*"))
        .and(body_string_contains("BEGIN:VALARM"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/1234/calendars/home/evt-1.ics"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let event = client(&server)
        .move_event("evt-1", None, "Work")
        .await
        .unwrap();
    assert_eq!(event.calendar, "Work");
}

#[tokio::test]
async fn a_copy_that_cannot_delete_the_original_says_the_event_exists_twice() {
    let server = MockServer::start().await;
    mount_for_move(&server).await;
    Mock::given(method("MOVE"))
        .respond_with(ResponseTemplate::new(501))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(STANDUP_ICS))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(423))
        .mount(&server)
        .await;

    let err = client(&server)
        .move_event("evt-1", None, "Work")
        .await
        .unwrap_err()
        .to_string();

    // A half-done move is recoverable, but only if we say so plainly.
    assert!(err.contains("exists twice"), "unhelpful: {err}");
}

#[tokio::test]
async fn moving_into_a_read_only_calendar_is_refused_before_anything_is_written() {
    let server = MockServer::start().await;
    mount_for_move(&server).await;

    let err = client(&server)
        .move_event("evt-1", None, "Team")
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("read-only"), "unhelpful: {err}");
    let requests = server.received_requests().await.unwrap();
    assert!(!requests.iter().any(|r| r.method == "MOVE"));
}

#[tokio::test]
async fn moving_an_event_to_where_it_already_is_is_refused() {
    let server = MockServer::start().await;
    mount_for_move(&server).await;

    let err = client(&server)
        .move_event("evt-1", None, "Home")
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("already in"), "unhelpful: {err}");
}

// ============ Calendar collections ============

/// The scheduling inbox alongside the calendars, so the default-calendar
/// property has somewhere to live.
const WITH_INBOX_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:ic="http://apple.com/ns/ical/">
  <response>
    <href>/1234/calendars/inbox/</href>
    <propstat><prop>
      <resourcetype><collection/><c:schedule-inbox/></resourcetype>
    </prop></propstat>
  </response>
  <response>
    <href>/1234/calendars/home/</href>
    <propstat><prop>
      <displayname>Home</displayname>
      <resourcetype><collection/><c:calendar/></resourcetype>
      <c:schedule-default-calendar-URL><href>/1234/calendars/home/</href></c:schedule-default-calendar-URL>
      <c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set>
      <current-user-privilege-set><privilege><read/></privilege><privilege><write/></privilege></current-user-privilege-set>
    </prop></propstat>
  </response>
  <response>
    <href>/1234/calendars/work/</href>
    <propstat><prop>
      <displayname>Work</displayname>
      <resourcetype><collection/><c:calendar/></resourcetype>
      <c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set>
      <current-user-privilege-set><privilege><read/></privilege><privilege><write/></privilege></current-user-privilege-set>
    </prop></propstat>
  </response>
</multistatus>"#;

/// A PROPPATCH answering "yes" for every property in it.
fn proppatch_ok(href: &str, props: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:ic="http://apple.com/ns/ical/">
  <response>
    <href>{href}</href>
    <propstat><prop>{props}</prop><status>HTTP/1.1 200 OK</status></propstat>
  </response>
</multistatus>"#
    )
}

async fn mount_with_inbox(server: &MockServer) {
    Mock::given(method("PROPFIND"))
        .and(path("/"))
        .respond_with(ok(PRINCIPAL_XML))
        .mount(server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/1234/principal/"))
        .respond_with(ok(HOME_XML))
        .mount(server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/1234/calendars/"))
        .respond_with(ok(WITH_INBOX_XML))
        .mount(server)
        .await;
}

#[tokio::test]
async fn setting_the_default_calendar_patches_the_scheduling_inbox() {
    let server = MockServer::start().await;
    mount_with_inbox(&server).await;
    Mock::given(method("PROPPATCH"))
        .and(path("/1234/calendars/inbox/"))
        // RFC 6638 wraps the value in a DAV:href, not bare text.
        .and(body_string_contains(
            "<c:schedule-default-calendar-URL><d:href>/1234/calendars/work/</d:href>",
        ))
        .respond_with(ok(&proppatch_ok(
            "/1234/calendars/inbox/",
            "<c:schedule-default-calendar-URL/>",
        )))
        .mount(&server)
        .await;

    let calendar = client(&server).set_default_calendar("Work").await.unwrap();
    assert_eq!(calendar.id, "work");
}

#[tokio::test]
async fn a_property_the_server_refuses_is_an_error_not_a_silent_no_op() {
    let server = MockServer::start().await;
    mount_with_inbox(&server).await;
    // The trap this exists to catch: a PROPPATCH answers 207 whatever happens,
    // and the real verdict is the status inside each propstat. iCloud refuses
    // this property exactly like this.
    Mock::given(method("PROPPATCH"))
        .respond_with(ok(r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <response>
    <href>/1234/calendars/inbox/</href>
    <propstat>
      <prop><c:schedule-default-calendar-URL/></prop>
      <status>HTTP/1.1 403 Forbidden</status>
    </propstat>
  </response>
</multistatus>"#))
        .mount(&server)
        .await;

    let err = client(&server)
        .set_default_calendar("Work")
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("refused"), "swallowed a refusal: {err}");
    assert!(
        err.contains("schedule-default-calendar-URL"),
        "doesn't say which property: {err}"
    );
    assert!(err.contains("403"), "doesn't say why: {err}");
}

#[tokio::test]
async fn setting_a_read_only_calendar_as_the_default_is_refused() {
    let server = MockServer::start().await;
    mount_for_move(&server).await;

    let err = client(&server)
        .set_default_calendar("Team")
        .await
        .unwrap_err()
        .to_string();

    // Pointing the default at a calendar you can't write to would break every
    // later create with a much more confusing error.
    assert!(err.contains("read-only"), "unhelpful: {err}");
    let requests = server.received_requests().await.unwrap();
    assert!(!requests.iter().any(|r| r.method == "PROPPATCH"));
}

#[tokio::test]
async fn creating_a_calendar_derives_a_readable_id_from_the_name() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .and(path("/"))
        .respond_with(ok(PRINCIPAL_XML))
        .mount(&server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/1234/principal/"))
        .respond_with(ok(HOME_XML))
        .mount(&server)
        .await;
    Mock::given(method("MKCALENDAR"))
        .and(path("/1234/calendars/work-trips/"))
        .and(body_string_contains(
            "<d:displayname>Work Trips</d:displayname>",
        ))
        .and(body_string_contains(
            "<ic:calendar-color>#FF2968</ic:calendar-color>",
        ))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;
    // Re-read after creating: the server decides the final display name.
    Mock::given(method("PROPFIND"))
        .and(path("/1234/calendars/"))
        .respond_with(ok(r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:ic="http://apple.com/ns/ical/">
  <response>
    <href>/1234/calendars/work-trips/</href>
    <propstat><prop>
      <displayname>Work Trips</displayname>
      <resourcetype><collection/><c:calendar/></resourcetype>
      <ic:calendar-color>#FF2968</ic:calendar-color>
      <c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set>
      <current-user-privilege-set><privilege><read/></privilege><privilege><write/></privilege></current-user-privilege-set>
    </prop></propstat>
  </response>
</multistatus>"#))
        .mount(&server)
        .await;

    let fields = caldav_cli::models::CalendarFields {
        color: Some("#FF2968"),
        ..Default::default()
    };
    let calendar = client(&server)
        .create_calendar("Work Trips", &fields)
        .await
        .unwrap();

    assert_eq!(calendar.id, "work-trips");
    assert_eq!(calendar.name, "Work Trips");
}

#[tokio::test]
async fn a_name_colliding_with_an_existing_collection_is_refused_clearly() {
    let server = MockServer::start().await;
    mount_with_inbox(&server).await;
    // RFC 4791 §5.3.1.2: 405 when the path is already occupied.
    Mock::given(method("MKCALENDAR"))
        .respond_with(ResponseTemplate::new(405))
        .mount(&server)
        .await;

    let err = client(&server)
        .create_calendar("Work", &Default::default())
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("already exists"), "unhelpful: {err}");
    assert!(err.contains("work"), "doesn't name the id: {err}");
}

#[tokio::test]
async fn renaming_a_calendar_patches_only_what_changed() {
    let server = MockServer::start().await;
    mount_with_inbox(&server).await;
    Mock::given(method("PROPPATCH"))
        .and(path("/1234/calendars/work/"))
        .and(body_string_contains(
            "<d:displayname>Client work</d:displayname>",
        ))
        .respond_with(ok(&proppatch_ok(
            "/1234/calendars/work/",
            "<d:displayname/>",
        )))
        .mount(&server)
        .await;

    let fields = caldav_cli::models::CalendarFields {
        name: Some("Client work"),
        ..Default::default()
    };
    assert!(
        client(&server)
            .update_calendar("Work", &fields)
            .await
            .is_ok()
    );

    let requests = server.received_requests().await.unwrap();
    let patch = requests.iter().find(|r| r.method == "PROPPATCH").unwrap();
    let body = String::from_utf8(patch.body.clone()).unwrap();
    // Nothing else was named, so nothing else is touched — and in particular
    // no empty <d:remove> block, which some servers reject outright.
    assert!(!body.contains("calendar-color"), "over-patched: {body}");
    assert!(!body.contains("<d:remove>"), "spurious remove: {body}");
}

#[tokio::test]
async fn clearing_a_calendar_property_removes_it_rather_than_setting_it_empty() {
    let server = MockServer::start().await;
    mount_with_inbox(&server).await;
    Mock::given(method("PROPPATCH"))
        .and(body_string_contains(
            "<d:remove><d:prop><ic:calendar-color/>",
        ))
        .respond_with(ok(&proppatch_ok(
            "/1234/calendars/work/",
            "<ic:calendar-color/>",
        )))
        .mount(&server)
        .await;

    let fields = caldav_cli::models::CalendarFields {
        color: Some(""),
        ..Default::default()
    };
    assert!(
        client(&server)
            .update_calendar("Work", &fields)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn deleting_the_default_calendar_is_refused() {
    let server = MockServer::start().await;
    mount_with_inbox(&server).await;

    // Home is the server-advertised default. Deleting it would leave the account
    // with nowhere for a new event to go.
    let err = client(&server)
        .delete_calendar("Home")
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("default calendar"), "unhelpful: {err}");
    let requests = server.received_requests().await.unwrap();
    assert!(!requests.iter().any(|r| r.method == "DELETE"));
}

#[tokio::test]
async fn deleting_a_calendar_removes_the_collection() {
    let server = MockServer::start().await;
    mount_with_inbox(&server).await;
    Mock::given(method("DELETE"))
        .and(path("/1234/calendars/work/"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let gone = client(&server).delete_calendar("Work").await.unwrap();
    // Returned as it was: there is nothing left to read it back from.
    assert_eq!(gone.name, "Work");
}
