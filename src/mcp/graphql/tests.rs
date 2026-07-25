//! GraphQL tests that assert on the **requests actually made**.
//!
//! Running the real schema against a wiremock CalDAV server is the only way the
//! batching and laziness claims mean anything: a test that only checked the
//! shape of the response would pass just as happily with an N+1 underneath. So
//! these count HTTP calls.

use std::sync::Arc;

use serde_json::Value;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{CalDavSchema, build_schema, request};
use crate::caldav::CalDavClient;

const PRINCIPAL_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:">
  <response><href>/</href><propstat><prop>
    <current-user-principal><href>/1234/principal/</href></current-user-principal>
  </prop><status>HTTP/1.1 200 OK</status></propstat></response>
</multistatus>"#;

const HOME_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <response><href>/1234/principal/</href><propstat><prop>
    <c:calendar-home-set><href>/1234/calendars/</href></c:calendar-home-set>
  </prop><status>HTTP/1.1 200 OK</status></propstat></response>
</multistatus>"#;

/// Three collections: two that hold events, one task list that doesn't. The
/// task list is here so "which calendars get queried" is a real question.
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
  <response>
    <href>/1234/calendars/tasks/</href>
    <propstat><prop>
      <displayname>Reminders</displayname>
      <resourcetype><collection/><c:calendar/></resourcetype>
      <c:supported-calendar-component-set><c:comp name="VTODO"/></c:supported-calendar-component-set>
      <current-user-privilege-set><privilege><read/></privilege></current-user-privilege-set>
    </prop></propstat>
  </response>
</multistatus>"#;

fn ics(uid: &str, summary: &str, start: &str, end: &str, extra: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nSUMMARY:{summary}\r\n\
         DTSTART:{start}\r\nDTEND:{end}\r\n{extra}END:VEVENT\r\nEND:VCALENDAR"
    )
}

/// Wrap iCalendar payloads in the multistatus a REPORT answers with.
fn report_of(entries: &[(&str, String)]) -> String {
    let responses: String = entries
        .iter()
        .map(|(href, body)| {
            format!(
                r#"<response><href>{href}</href><propstat><prop>
                <getetag>"etag-{href}"</getetag>
                <c:calendar-data>{body}</c:calendar-data>
                </prop><status>HTTP/1.1 200 OK</status></propstat></response>"#
            )
        })
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">{responses}</multistatus>"#
    )
}

fn ok(body: String) -> ResponseTemplate {
    ResponseTemplate::new(207)
        .set_body_string(body)
        .insert_header("Content-Type", "application/xml")
}

async fn mount_discovery(server: &MockServer) {
    Mock::given(method("PROPFIND"))
        .and(path("/"))
        .respond_with(ok(PRINCIPAL_XML.into()))
        .mount(server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/1234/principal/"))
        .respond_with(ok(HOME_XML.into()))
        .mount(server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/1234/calendars/"))
        .respond_with(ok(CALENDARS_XML.into()))
        .mount(server)
        .await;
}

/// Discovery, plus a day's events in each event-holding calendar and a weekly
/// series in Home so recurrence has something to chew on.
async fn mount(server: &MockServer) {
    mount_discovery(server).await;

    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/home/"))
        .respond_with(ok(report_of(&[
            (
                "/1234/calendars/home/standup.ics",
                ics(
                    "standup",
                    "Standup",
                    "20260727T090000Z",
                    "20260727T093000Z",
                    "RRULE:FREQ=WEEKLY;BYDAY=MO\r\n",
                ),
            ),
            (
                "/1234/calendars/home/lunch.ics",
                ics(
                    "lunch",
                    "Lunch",
                    "20260727T120000Z",
                    "20260727T130000Z",
                    "CATEGORIES:Personal\r\n",
                ),
            ),
        ])))
        .mount(server)
        .await;

    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/team/"))
        .respond_with(ok(report_of(&[(
            "/1234/calendars/team/review.ics",
            ics(
                "review",
                "Design review",
                "20260727T093000Z",
                "20260727T103000Z",
                "ATTENDEE;CN=Alice:mailto:alice@example.com\r\n",
            ),
        )])))
        .mount(server)
        .await;

    // The task list holds no events; a REPORT against it must never happen.
    Mock::given(method("REPORT"))
        .and(path("/1234/calendars/tasks/"))
        .respond_with(ok(report_of(&[])))
        .mount(server)
        .await;
}

struct Harness {
    server: MockServer,
    schema: CalDavSchema,
}

/// The window every fixture event lives in — Monday 27 July 2026.
const DAY: &str = "2026-07-27";

impl Harness {
    async fn start() -> Self {
        let server = MockServer::start().await;
        mount(&server).await;
        Self {
            server,
            schema: build_schema(),
        }
    }

    fn client(&self) -> Arc<CalDavClient> {
        Arc::new(CalDavClient::new(
            self.server.uri(),
            "me@example.com".into(),
            "app-password".into(),
        ))
    }

    /// Execute a query against a fresh client and loader set, exactly as the MCP
    /// tool does.
    async fn run(&self, query: &str) -> Value {
        let response = self
            .schema
            .execute(request(query, self.client(), None))
            .await;
        assert!(
            response.errors.is_empty(),
            "query failed: {:?}",
            response.errors
        );
        response.data.into_json().unwrap()
    }

    async fn run_expecting_error(&self, query: &str) -> String {
        let response = self
            .schema
            .execute(request(query, self.client(), None))
            .await;
        assert!(!response.errors.is_empty(), "expected an error");
        response.errors[0].message.clone()
    }

    /// Every request the server saw, as `(METHOD, path)`.
    async fn calls(&self) -> Vec<(String, String)> {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| (r.method.to_string(), r.url.path().to_string()))
            .collect()
    }

    async fn count(&self, wanted: &str, fragment: &str) -> usize {
        self.calls()
            .await
            .iter()
            .filter(|(m, p)| m == wanted && p.contains(fragment))
            .count()
    }

    /// How many event REPORTs went out, across all collections.
    async fn reports(&self) -> usize {
        self.count("REPORT", "/calendars/").await
    }

    /// How many times the calendar listing was fetched.
    async fn listings(&self) -> usize {
        self.calls()
            .await
            .iter()
            .filter(|(m, p)| m == "PROPFIND" && p == "/1234/calendars/")
            .count()
    }
}

// ============ The calendar loader ============

#[tokio::test]
async fn every_calendar_path_shares_one_listing() {
    let h = Harness::start().await;
    // Four separate routes to the calendar listing in one document: the
    // top-level list, a lookup by name, the default, and the `calendar` edge
    // hanging off events in two different collections.
    h.run(&format!(
        r#"{{
            calendars {{ nodes {{ name }} }}
            named: calendar(id: "Home") {{ name }}
            fallback: calendar {{ name }}
            events(start: "{DAY}", days: 1) {{ nodes {{ calendar {{ name color }} }} }}
        }}"#
    ))
    .await;

    assert_eq!(h.listings().await, 1, "calls: {:?}", h.calls().await);
}

#[tokio::test]
async fn calendars_are_refetched_for_each_request() {
    // The client used to memoise the listing for its whole life, so a
    // long-running server never saw a calendar created or renamed. Loaders are
    // per request, so each request must go and look again.
    let h = Harness::start().await;
    h.run("{ calendars { nodes { name } } }").await;
    h.run("{ calendars { nodes { name } } }").await;

    assert_eq!(h.listings().await, 2);
}

#[tokio::test]
async fn task_only_collections_are_never_queried() {
    let h = Harness::start().await;
    h.run(&format!(
        r#"{{ events(start: "{DAY}", days: 1) {{ nodes {{ id }} }} }}"#
    ))
    .await;

    assert_eq!(h.count("REPORT", "/calendars/tasks/").await, 0);
    assert_eq!(
        h.reports().await,
        2,
        "one REPORT per event-holding calendar"
    );
}

// ============ The window loader ============

#[tokio::test]
async fn repeated_windows_collapse_into_one_fetch() {
    let h = Harness::start().await;
    // Three asks for the same window over the same calendars. Without the
    // loader that is six REPORTs.
    h.run(&format!(
        r#"{{
            a: events(start: "{DAY}", days: 1) {{ nodes {{ id }} }}
            b: events(start: "{DAY}", days: 1) {{ nodes {{ summary }} }}
            c: events(start: "{DAY}", days: 1) {{ totalCount }}
        }}"#
    ))
    .await;

    assert_eq!(h.reports().await, 2, "calls: {:?}", h.calls().await);
}

#[tokio::test]
async fn a_different_window_is_a_different_fetch() {
    let h = Harness::start().await;
    h.run(&format!(
        r#"{{
            today: events(start: "{DAY}", days: 1) {{ nodes {{ id }} }}
            week: events(start: "{DAY}", days: 7) {{ nodes {{ id }} }}
        }}"#
    ))
    .await;

    // Two windows × two calendars, and nothing deduplicated away.
    assert_eq!(h.reports().await, 4);
}

#[tokio::test]
async fn expanded_and_unexpanded_reads_do_not_share_a_fetch() {
    let h = Harness::start().await;
    h.run(&format!(
        r#"{{
            occurrences: events(start: "{DAY}", days: 30, expand: true) {{ totalCount }}
            masters: events(start: "{DAY}", days: 30, expand: false) {{ totalCount }}
        }}"#
    ))
    .await;

    // They ask the server different questions, so they must not share a key.
    assert_eq!(h.reports().await, 4);
}

#[tokio::test]
async fn naming_a_calendar_queries_only_that_one() {
    let h = Harness::start().await;
    let data = h
        .run(&format!(
            r#"{{ events(calendar: "Home", start: "{DAY}", days: 1) {{ calendarsQueried nodes {{ id }} }} }}"#
        ))
        .await;

    assert_eq!(h.reports().await, 1);
    assert_eq!(data["events"]["calendarsQueried"], 1);
    assert_eq!(h.count("REPORT", "/calendars/home/").await, 1);
}

#[tokio::test]
async fn walking_from_calendars_into_events_costs_one_report_each() {
    let h = Harness::start().await;
    let data = h
        .run(&format!(
            r#"{{ calendars {{ nodes {{ name events(start: "{DAY}", days: 1) {{ totalCount }} }} }} }}"#
        ))
        .await;

    assert_eq!(h.listings().await, 1);
    // Home and Team get a REPORT each. The task list is still listed as a
    // calendar — it just has no events to ask for.
    assert_eq!(h.reports().await, 3, "calls: {:?}", h.calls().await);
    assert_eq!(data["calendars"]["nodes"].as_array().unwrap().len(), 3);
}

// ============ Look-ahead ============

#[tokio::test]
async fn selecting_no_events_fetches_no_events() {
    let h = Harness::start().await;
    let data = h
        .run(&format!(
            r#"{{ events(start: "{DAY}", days: 1) {{ calendarsQueried pageInfo {{ hasNextPage }} }} }}"#
        ))
        .await;

    assert_eq!(h.reports().await, 0, "calls: {:?}", h.calls().await);
    // The calendar count is still honest: it comes from the listing, not a fetch.
    assert_eq!(data["events"]["calendarsQueried"], 2);
}

#[tokio::test]
async fn counting_still_requires_the_fetch() {
    // CalDAV has no `calculateTotal` to opt into — the count is the length of
    // what came back — so selecting it does mean fetching. Stated, not implied.
    let h = Harness::start().await;
    let data = h
        .run(&format!(
            r#"{{ events(start: "{DAY}", days: 1) {{ totalCount }} }}"#
        ))
        .await;

    assert_eq!(h.reports().await, 2);
    assert_eq!(data["events"]["totalCount"], 3);
}

// ============ Lazy edges ============

#[tokio::test]
async fn nested_edges_cost_nothing_until_selected() {
    let h = Harness::start().await;
    h.run(&format!(
        r#"{{ events(start: "{DAY}", days: 1) {{ nodes {{ id summary attendees {{ email }} }} }} }}"#
    ))
    .await;

    // The listing, then one REPORT per event-holding calendar. No `series`, no
    // `conflicts`, so nothing more.
    assert_eq!(h.reports().await, 2);
    assert_eq!(h.listings().await, 1);
}

#[tokio::test]
async fn occurrences_expand_locally_and_fetch_nothing() {
    let h = Harness::start().await;
    let data = h
        .run(&format!(
            r#"{{ events(calendar: "Home", start: "{DAY}", days: 1, expand: false, filter: {{ recurring: true }}) {{
                nodes {{
                  summary
                  occurrences(start: "{DAY}", days: 21) {{ totalCount calendarsQueried nodes {{ recurrenceId }} }}
                }}
            }} }}"#
        ))
        .await;

    // One REPORT for the masters. The occurrences come out of the RRULE that
    // arrived with them.
    assert_eq!(h.reports().await, 1, "calls: {:?}", h.calls().await);

    let occurrences = &data["events"]["nodes"][0]["occurrences"];
    assert_eq!(occurrences["calendarsQueried"], 0);
    assert_eq!(occurrences["totalCount"], 3, "three Mondays in three weeks");
}

#[tokio::test]
async fn conflicts_across_a_page_share_one_fetch_per_calendar() {
    let h = Harness::start().await;
    let data = h
        .run(&format!(
            r#"{{ events(start: "{DAY}", days: 1) {{ nodes {{ summary conflicts {{ totalCount nodes {{ summary }} }} }} }} }}"#
        ))
        .await;

    // The whole point of snapping the conflict window to whole days: three
    // events each asking "what clashes with me" resolve the same window, so it
    // stays at one REPORT per calendar rather than one per event.
    assert_eq!(h.reports().await, 2, "calls: {:?}", h.calls().await);

    // Standup 09:00–09:30 and the review 09:30–10:30 are back-to-back, and
    // lunch is hours later, so nothing clashes.
    let nodes = data["events"]["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 3);
    for node in nodes {
        assert_eq!(
            node["conflicts"]["totalCount"], 0,
            "{} should not clash",
            node["summary"]
        );
    }
}

#[tokio::test]
async fn overlapping_events_are_reported_as_conflicts() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;
    Mock::given(method("REPORT"))
        .and(path_regex(r"/1234/calendars/(home|team|tasks)/"))
        .respond_with(ok(report_of(&[
            (
                "/1234/calendars/home/a.ics",
                ics("a", "Deep work", "20260727T090000Z", "20260727T110000Z", ""),
            ),
            (
                "/1234/calendars/home/b.ics",
                ics("b", "Interview", "20260727T100000Z", "20260727T110000Z", ""),
            ),
        ])))
        .mount(&server)
        .await;

    let schema = build_schema();
    let client = Arc::new(CalDavClient::new(
        server.uri(),
        "me@example.com".into(),
        "app-password".into(),
    ));
    let response = schema
        .execute(request(
            &format!(
                r#"{{ events(calendar: "Home", start: "{DAY}", days: 1) {{
                       nodes {{ summary conflicts(calendar: "Home") {{ totalCount nodes {{ summary }} }} }} }} }}"#
            ),
            client,
            None,
        ))
        .await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.into_json().unwrap();

    let nodes = data["events"]["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 2);
    // Each sees the other, and neither sees itself.
    assert_eq!(nodes[0]["conflicts"]["totalCount"], 1);
    assert_eq!(nodes[0]["conflicts"]["nodes"][0]["summary"], "Interview");
    assert_eq!(nodes[1]["conflicts"]["totalCount"], 1);
    assert_eq!(nodes[1]["conflicts"]["nodes"][0]["summary"], "Deep work");
}

#[tokio::test]
async fn the_series_edge_batches_into_one_multiget() {
    let h = Harness::start().await;
    h.run(&format!(
        r#"{{ events(calendar: "Home", start: "{DAY}", days: 21, filter: {{ recurring: true }}) {{
               nodes {{ recurrenceId series {{ id recurrence }} }} }} }}"#
    ))
    .await;

    // Every occurrence's master lives at the same resource, so the whole page
    // resolves through one `calendar-multiget` rather than one request each.
    let multigets = h
        .server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| String::from_utf8_lossy(&r.body).contains("calendar-multiget"))
        .count();
    assert_eq!(multigets, 1, "calls: {:?}", h.calls().await);
}

// ============ Pagination ============

#[tokio::test]
async fn pages_walk_with_id_cursors() {
    let h = Harness::start().await;
    let first = h
        .run(&format!(
            r#"{{ events(start: "{DAY}", days: 1, first: 2) {{
                   totalCount pageInfo {{ hasNextPage endCursor }} edges {{ cursor node {{ summary }} }} }} }}"#
        ))
        .await;

    assert_eq!(first["events"]["totalCount"], 3);
    assert_eq!(first["events"]["pageInfo"]["hasNextPage"], true);
    let cursor = first["events"]["pageInfo"]["endCursor"].as_str().unwrap();

    let second = h
        .run(&format!(
            r#"{{ events(start: "{DAY}", days: 1, first: 2, after: "{cursor}") {{
                   pageInfo {{ hasNextPage }} nodes {{ summary }} }} }}"#
        ))
        .await;
    let nodes = second["events"]["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(second["events"]["pageInfo"]["hasNextPage"], false);

    // The pages partition the result set: no event appears twice.
    let seen: Vec<&str> = first["events"]["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["node"]["summary"].as_str().unwrap())
        .collect();
    assert!(!seen.contains(&nodes[0]["summary"].as_str().unwrap()));
}

#[tokio::test]
async fn a_stale_cursor_says_how_to_recover() {
    let h = Harness::start().await;
    let err = h
        .run_expecting_error(&format!(
            r#"{{ events(start: "{DAY}", days: 1, after: "deleted-event") {{ nodes {{ id }} }} }}"#
        ))
        .await;
    assert!(err.contains("Restart pagination"), "{err}");
}

#[tokio::test]
async fn first_and_last_together_are_rejected() {
    let h = Harness::start().await;
    let err = h
        .run_expecting_error(&format!(
            r#"{{ events(start: "{DAY}", days: 1, first: 1, last: 1) {{ nodes {{ id }} }} }}"#
        ))
        .await;
    assert!(err.contains("not both"), "{err}");
}

#[tokio::test]
async fn page_size_is_clamped_rather_than_refused() {
    let h = Harness::start().await;
    let data = h
        .run(&format!(
            r#"{{ events(start: "{DAY}", days: 1, first: 5000) {{ totalCount nodes {{ id }} }} }}"#
        ))
        .await;
    assert_eq!(data["events"]["nodes"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn calendars_paginate_too() {
    let h = Harness::start().await;
    let data = h
        .run("{ calendars(first: 2) { totalCount pageInfo { hasNextPage } nodes { name } } }")
        .await;

    assert_eq!(data["calendars"]["totalCount"], 3);
    assert_eq!(data["calendars"]["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(data["calendars"]["pageInfo"]["hasNextPage"], true);
}

// ============ Filters ============

#[tokio::test]
async fn filters_narrow_without_extra_requests() {
    let h = Harness::start().await;
    let data = h
        .run(&format!(
            r#"{{ events(start: "{DAY}", days: 1, filter: {{
                   or: [{{ summary: "standup" }}, {{ attendee: "alice@example.com" }}]
                 }}) {{ totalCount nodes {{ summary }} }} }}"#
        ))
        .await;

    assert_eq!(h.reports().await, 2, "filtering is local; no extra calls");
    assert_eq!(data["events"]["totalCount"], 2);
}

#[tokio::test]
async fn not_branches_exclude() {
    let h = Harness::start().await;
    let data = h
        .run(&format!(
            r#"{{ events(start: "{DAY}", days: 1, filter: {{ not: [{{ category: "Personal" }}] }}) {{ nodes {{ summary }} }} }}"#
        ))
        .await;

    let summaries: Vec<&str> = data["events"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["summary"].as_str().unwrap())
        .collect();
    assert!(!summaries.contains(&"Lunch"), "{summaries:?}");
}

#[tokio::test]
async fn sorting_is_applied_across_calendars() {
    let h = Harness::start().await;
    let data = h
        .run(&format!(
            r#"{{ events(start: "{DAY}", days: 1, sort: [{{ property: SUMMARY, ascending: false }}]) {{ nodes {{ summary }} }} }}"#
        ))
        .await;

    let summaries: Vec<&str> = data["events"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["summary"].as_str().unwrap())
        .collect();
    assert_eq!(summaries, ["Standup", "Lunch", "Design review"]);
}

#[tokio::test]
async fn the_deprecated_search_shim_still_works() {
    let h = Harness::start().await;
    let data = h
        .run(&format!(
            r#"{{ searchEvents(query: "standup", start: "{DAY}", days: 1) {{ totalCount nodes {{ summary }} }} }}"#
        ))
        .await;

    assert_eq!(data["searchEvents"]["totalCount"], 1);
    assert_eq!(data["searchEvents"]["nodes"][0]["summary"], "Standup");
}

// ============ Single-event lookup ============

#[tokio::test]
async fn naming_the_calendar_turns_a_uid_sweep_into_one_request() {
    let h = Harness::start().await;
    h.run(r#"{ event(id: "standup", calendar: "Home") { summary } }"#)
        .await;

    assert_eq!(h.reports().await, 1, "calls: {:?}", h.calls().await);
}

#[tokio::test]
async fn repeated_uids_in_one_query_are_deduplicated() {
    let h = Harness::start().await;
    h.run(
        r#"{
            a: event(id: "standup", calendar: "Home") { summary }
            b: event(id: "standup", calendar: "Home") { location }
        }"#,
    )
    .await;

    assert_eq!(h.reports().await, 1, "calls: {:?}", h.calls().await);
}

// ============ Guardrails ============

#[tokio::test]
async fn depth_is_capped_before_anything_is_fetched() {
    let h = Harness::start().await;
    // The graph cycles: event → calendar → events → calendar → …
    let deep = format!(
        "{{ events {{ nodes {{ {} id {} }} }} }}",
        "calendar { events { nodes { ".repeat(9),
        "} } }".repeat(9)
    );
    let err = h.run_expecting_error(&deep).await;
    assert!(err.to_lowercase().contains("too deep"), "{err}");
    assert_eq!(h.reports().await, 0, "rejected during validation");
}

#[tokio::test]
async fn an_expensive_query_is_allowed_to_run() {
    // Costs are declared as guidance, not enforced as a cap — a model composing
    // a query can't guess a threshold it cannot see.
    let h = Harness::start().await;
    h.run(&format!(
        r#"{{ events(start: "{DAY}", days: 1, first: 100) {{
               nodes {{ summary conflicts(first: 100) {{ nodes {{ summary attendees {{ email }} }} }} }} }} }}"#
    ))
    .await;
}

/// `calendar_schema` returns the SDL and nothing else — the hand-written
/// prelude was deleted because the schema already said it. That only holds if
/// the schema really does, so the load-bearing guidance is asserted here rather
/// than assumed: a model reading the SDL must be able to work out how to page,
/// what a cursor is, and that filters nest.
#[tokio::test]
async fn the_sdl_carries_the_guidance_that_replaced_the_prelude() {
    let sdl = build_schema().sdl();
    for needle in [
        // Time grammar, which the prelude used to spell out.
        "'today', 'tomorrow'",
        // How to reach a series' master in order to edit it.
        "master event",
        // Paging: what a cursor is and that totalCount is cheap.
        "Cursors are event ids",
        "free and exact",
        // Filter composition.
        "AND-ed",
        "nest arbitrarily",
        // PREVIEW-first on the destructive mutations.
        "PREVIEW",
    ] {
        assert!(
            sdl.contains(needle),
            "the SDL no longer states {needle:?} — it was the prelude's job and \
             the prelude is gone"
        );
    }
}

// ============ Two-phase writes ============

/// PREVIEW → CONFIRM in two requests, as a model does it: the token from the
/// first is carried into the second.
///
/// Returns both results so a test can assert on the preview text as well as the
/// outcome. `confirm` is the mutation with `$t` standing in for the token.
async fn preview_then_confirm(h: &Harness, preview: &str, confirm: &str) -> (Value, Value) {
    let first = h.run(preview).await;
    let result = first
        .as_object()
        .unwrap()
        .values()
        .next()
        .expect("no mutation field in the preview response");
    let token = result["confirmationToken"]
        .as_str()
        .unwrap_or_else(|| panic!("preview issued no token: {result}"))
        .to_string();
    let second = h.run(&confirm.replace("$t", &token)).await;
    (result.clone(), second)
}

/// Accept any PUT, so a write reaches the assertion rather than an HTTP error.
async fn accept_writes(h: &Harness) {
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&h.server)
        .await;
}

#[tokio::test]
async fn confirming_without_a_token_is_refused_by_every_two_phase_mutation() {
    let h = Harness::start().await;
    accept_writes(&h).await;

    // Each of these changes or removes something, so none may act on a CONFIRM
    // the user was never shown.
    for mutation in [
        r#"updateEvent(action: CONFIRM, id: "standup", summary: "x")"#,
        r#"deleteEvent(action: CONFIRM, id: "standup")"#,
        r#"moveEvent(action: CONFIRM, id: "standup", toCalendar: "Team")"#,
        r#"deleteOccurrence(action: CONFIRM, id: "standup", occurrence: "2026-08-03")"#,
        r#"respondToInvite(action: CONFIRM, id: "review", response: ACCEPTED)"#,
    ] {
        let data = h
            .run(&format!("mutation {{ m: {mutation} {{ success error }} }}"))
            .await;
        assert_eq!(data["m"]["success"], false, "{mutation} wrote unguarded");
        assert!(
            data["m"]["error"]
                .as_str()
                .unwrap()
                .contains("confirmationToken"),
            "{mutation}: {:?}",
            data["m"]["error"]
        );
    }

    // And the calendar-shaped ones, which answer with a different result type.
    for mutation in [
        r#"updateCalendar(action: CONFIRM, id: "home", name: "x")"#,
        r#"deleteCalendar(action: CONFIRM, id: "home")"#,
    ] {
        let data = h
            .run(&format!("mutation {{ m: {mutation} {{ success error }} }}"))
            .await;
        assert_eq!(data["m"]["success"], false, "{mutation} wrote unguarded");
    }
}

#[tokio::test]
async fn cancelling_an_occurrence_previews_the_instant_it_resolved_to() {
    let h = Harness::start().await;
    accept_writes(&h).await;

    let (preview, confirmed) = preview_then_confirm(
        &h,
        // A bare date. The series is weekly on Mondays from 27 July 2026.
        r#"mutation { deleteOccurrence(action: PREVIEW, id: "standup", occurrence: "2026-08-03") { preview confirmationToken } }"#,
        r#"mutation { deleteOccurrence(action: CONFIRM, id: "standup", occurrence: "2026-08-03", confirmationToken: "$t") { success error event { id } } }"#,
    )
    .await;

    let text = preview["preview"].as_str().unwrap();
    // The resolved instant is the whole point: a bare date is ambiguous, and the
    // user has to see which occurrence they are agreeing to lose.
    assert!(
        text.contains("2026-08-03T09:00:00Z"),
        "preview hides which occurrence: {text}"
    );
    assert!(text.contains("rest of the series is untouched"), "{text}");
    assert_eq!(confirmed["deleteOccurrence"]["success"], true);

    // The write added an EXDATE and kept the rule.
    let put = h
        .server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.method == "PUT")
        .expect("nothing was written");
    let body = String::from_utf8(put.body).unwrap();
    assert!(body.contains("EXDATE:20260803T090000Z"), "{body}");
    assert!(body.contains("RRULE:FREQ=WEEKLY;BYDAY=MO"), "{body}");
}

#[tokio::test]
async fn a_confirm_naming_a_different_occurrence_than_the_preview_is_rejected() {
    let h = Harness::start().await;
    accept_writes(&h).await;

    let preview = h
        .run(
            r#"mutation { deleteOccurrence(action: PREVIEW, id: "standup", occurrence: "2026-08-03") { confirmationToken } }"#,
        )
        .await;
    let token = preview["deleteOccurrence"]["confirmationToken"]
        .as_str()
        .unwrap();

    // Same token, different occurrence — the user approved losing the 3rd.
    let data = h
        .run(&format!(
            r#"mutation {{ deleteOccurrence(action: CONFIRM, id: "standup", occurrence: "2026-08-10", confirmationToken: "{token}") {{ success error }} }}"#
        ))
        .await;

    assert_eq!(data["deleteOccurrence"]["success"], false);
    assert!(
        data["deleteOccurrence"]["error"]
            .as_str()
            .unwrap()
            .contains("Params changed"),
        "{:?}",
        data["deleteOccurrence"]["error"]
    );
    assert_eq!(h.count("PUT", "/calendars/").await, 0, "it wrote anyway");
}

#[tokio::test]
async fn replying_to_an_invite_previews_the_status_change_and_the_organiser() {
    let h = Harness::start().await;
    accept_writes(&h).await;

    // The Team fixture's review event has Alice on it and no PARTSTAT for us.
    let data = h
        .run(
            r#"mutation { respondToInvite(action: PREVIEW, id: "review", response: DECLINED, attendee: "alice@example.com") { preview } }"#,
        )
        .await;

    let text = data["respondToInvite"]["preview"].as_str().unwrap();
    assert!(text.contains("NEEDS-ACTION → DECLINED"), "{text}");
    // Sending a message to a human is exactly what needs saying out loud.
    assert!(text.contains("organiser is notified"), "{text}");
}

#[tokio::test]
async fn moving_an_event_previews_both_calendars() {
    let h = Harness::start().await;

    let data = h
        .run(
            r#"mutation { moveEvent(action: PREVIEW, id: "lunch", toCalendar: "Team") { preview } }"#,
        )
        .await;

    let text = data["moveEvent"]["preview"].as_str().unwrap();
    assert!(text.contains("From calendar: Home"), "{text}");
    assert!(text.contains("To calendar: Team"), "{text}");
}

#[tokio::test]
async fn deleting_a_calendar_previews_how_much_would_be_lost() {
    let h = Harness::start().await;

    let data = h
        .run(r#"mutation { deleteCalendar(action: PREVIEW, id: "home") { preview } }"#)
        .await;

    let text = data["deleteCalendar"]["preview"].as_str().unwrap();
    // "This deletes the calendar" says nothing about the scale of the loss.
    assert!(text.contains("event(s)"), "no count of the loss: {text}");
    assert!(text.contains("Every event in it is deleted too"), "{text}");
    assert!(text.contains("cannot be undone"), "{text}");
}

#[tokio::test]
async fn an_unknown_calendar_fails_the_preview_instead_of_issuing_a_token() {
    let h = Harness::start().await;

    let data = h
        .run(r#"mutation { deleteCalendar(action: PREVIEW, id: "nope") { success error confirmationToken } }"#)
        .await;

    assert_eq!(data["deleteCalendar"]["success"], false);
    assert!(data["deleteCalendar"]["confirmationToken"].is_null());
}
