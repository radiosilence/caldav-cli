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

#[tokio::test]
async fn discovery_runs_once_per_client() {
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

    let propfinds = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r: &Request| r.method.as_str() == "PROPFIND")
        .count();
    // Four PROPFINDs total for the whole loop — the well-known probe, the
    // root, the principal, and the calendar home — not four per call.
    assert_eq!(propfinds, 4);
}
