# Changelog

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-07-25

### Added

- **The GraphQL surface is a graph.** Every logical edge is traversable, every
  collection is a Relay connection, and everything below a list is lazy and
  batched. The motivation is call count: "what's on today, whose calendar is
  it, and does anything clash" was `agenda` → `calendars` → an `events` call
  per result, with the model composing each round trip. It is now one query
  costing one `PROPFIND` and one REPORT per event-holding calendar.
- **New edges.** `Event.calendar` resolves the collection itself rather than
  just its name; `Event.series` walks from an expanded occurrence to the master
  carrying the `RRULE`; `Event.occurrences` evaluates that rule with no request
  at all; `Event.conflicts` finds what overlaps; `Calendar.events` reads one
  collection; `Query.calendar(id:)` looks one up, or returns the default when
  `id` is omitted.
- **Filters compose.** `EventFilter` is a tree: scalars on one object AND
  together, and `and`/`or`/`not` nest arbitrarily, so "confirmed, involving
  Alice either way, but not all-day" is one filter rather than three queries.
  Matching is client-side by necessity — CalDAV ANDs every sibling filter
  (RFC 4791 §9.7), has no OR, and implements `text-match` inconsistently across
  iCloud, Fastmail, Google and Nextcloud. The time range still goes on the
  wire, since that is the one filter every server agrees on. `EventSort` orders
  by start, end, summary, created or last-modified.
- **`calendarsQueried`** on every event connection, reporting how many
  collections a page cost. CalDAV has no cross-collection query, so an
  account-wide read is one REPORT per calendar — said out loud rather than
  hidden. Naming `calendar:` brings it to one.
- **Field audit.** `Event` now exposes `href`, `etag`, `sequence`,
  `durationMinutes`, `isRecurring` and `calendarName`; `Calendar` exposes
  `url`. All were already on the wire and being discarded.
- **A default calendar for new events.** `createEvent` / `caldav-cli create`
  with no calendar named now lands where the user's calendar app would put it,
  rather than in whichever writable collection sorted first alphabetically.
  Calendars carry `isDefault`, and it is honoured on writes only — reads still
  span the account, since scoping an agenda to one calendar hides the rest
  without saying so.
- **An override**, for accounts whose own default isn't where automation should
  write: `X-CalDAV-Calendar` per request in hosted mode, or `calendar` in
  `config.toml` / `CALDAV_CALENDAR`. The MCP server announces the choice in the
  `calendar` tool description, so a model knows where it is writing before it
  writes.
- **Discovery that doesn't assume a server's shape**, which iCloud rewards:
  it answers `schedule-default-calendar-URL` as bare element text rather than
  the `DAV:href` RFC 6638 specifies, and echoes the property name back empty in
  the `404` propstat of every collection that hasn't got it. The value is read
  in whichever shape arrives, from the calendar-home listing or — only when
  that says nothing — the scheduling inbox, the one location the RFC requires.
- **Docker images tagged by version.** `ghcr.io/radiosilence/caldav-cli` now
  gets `vX.Y.Z`, `vX.Y`, `vX`, and `latest` tags alongside `main` and
  `sha-<short>`, cut only on the push that first introduces that version in
  `Cargo.toml` so the tags never drift onto a later, unrelated commit.

### Changed

- **`createEvent` writes without a confirmation round trip**, dropping its
  `action` and `confirmationToken` arguments. The two-phase guard exists for
  changes that destroy state; a new event destroys nothing, is visible the
  moment it lands, and is deletable — so the model creates it and says what it
  created, and a wrong guess is corrected rather than pre-empted. `updateEvent`
  and `deleteEvent` are unchanged: they overwrite or remove something already
  there, and a delete can't be undone.
- **MCP tools renamed** to `calendar_schema` and `calendar`, from `schema_sdl`
  and `graphql` — clients render the tool name, and "Schema sdl" / "Graphql"
  described the transport rather than what the tool reaches. Breaking for
  anything naming the old tools; the queries themselves are unchanged.
- **A near-free idle cost for the MCP server.** Instructions and tool
  descriptions load into every session, most of which never mention a calendar,
  so the usage rules and examples moved out of them and into the
  `calendar_schema` response, alongside the SDL they annotate. Connecting the
  server now costs ~230 tokens instead of ~750, and everything substantial is
  paid only by sessions that touch a calendar.
- **Protocol version follows the SDK** instead of pinning `2024-11-05`, so
  clients get the newest version both ends know. Older clients are unaffected:
  the server echoes back whatever version they ask for.
- **Every read goes through a DataLoader.** No resolver touches the CalDAV
  client directly. `calendar-multiget` is the one genuine batch CalDAV offers —
  many hrefs, one REPORT — and `Event.series` uses it, so a page of occurrences
  costs one request per calendar rather than one per occurrence. The rest have
  no plural form, so those loaders do what is actually available: deduplicate
  repeated keys and issue the batch's requests concurrently instead of one
  after another. Loaders are per request, so their cache is request-scoped.
- **Per-calendar reads run concurrently.** `events_in_range` walked its
  calendars in a serial loop; an account-wide read now issues its REPORTs
  together, at most six in flight. This is the CLI's win too, not just
  GraphQL's.
- **Collections are connections**, taking `first`/`last`/`after`/`before` and
  carrying `totalCount`/`pageInfo`/`edges`/`nodes`. Cursors are event ids (an
  occurrence adds its `RECURRENCE-ID`, since a series repeats its UID) rather
  than offsets, so a cursor names a specific event and stays legible to a model
  composing the next page. CalDAV has no windowed query, so paging is slicing:
  `totalCount` is free and exact, and a cursor whose event has gone gives a
  "restart pagination" error rather than a quietly different page.
- **The `calendar_schema` guidance matches the new schema** — examples updated
  to connections, plus how to page, when `totalCount` is free, and that nested
  fields batch so one query beats a follow-up. A test executes every documented
  example against the real schema, since a stale example costs a model a failed
  round trip.
- **Query cost is guidance, not a cap.** Fields declare costs and the
  descriptions surface them, but nothing is refused for being expensive — a
  caller told "too complex" has to guess at a threshold it cannot see. Depth
  stays capped at 15: the graph has cycles by design and nothing else bounds
  them.

### Fixed

- **All-day events landed a day early anywhere east of Greenwich.** An
  iCalendar `DATE` has no timezone — it means the same day everywhere — but it
  was being resolved through one anyway. `RRuleSet` anchors a date-only
  `DTSTART` at midnight in the *machine's local zone*, so under BST a weekly
  Monday bin collection came back as Sunday, and an occurrence on the window's
  first day was dropped for sorting before it. The same applied to date-valued
  `EXDATE`s, whose exclusions then missed the day they named, and to
  non-recurring all-day events on any calendar publishing `X-WR-TIMEZONE`.
  Dates are now anchored at UTC midnight everywhere, so the date survives the
  round trip. Timed series still expand in their own zone, so 09:00 stays 09:00
  across a DST boundary.
- **The calendar listing was cached for the life of the process.** Clients are
  pooled per credential, so a long-running MCP server never saw a calendar
  created, renamed, or deleted after start-up — for writes as well as reads.
  The client no longer memoises it; deduplication belongs to the caller's
  scope, which for GraphQL is the per-request loader and for the CLI is a
  process that lives one command. Principal and calendar-home discovery are
  still cached, because those don't change.

### Removed

- `MAX_EVENTS` no longer truncates GraphQL reads. It existed to stop a
  decade-wide range flooding a model's context; pagination does that now, and a
  silent truncation would have made `totalCount` lie. The CLI's `--limit` is
  unchanged.

### Breaking

- Collections need `nodes { ... }` around their selection and default to 25
  items (max 100), where the old flat lists defaulted to 100.
- `Event.calendar` was the calendar's display name; it is now the `Calendar`
  itself. The old value is `calendarName`.
- `searchEvents` is deprecated in favour of `events(filter: { text: ... })`,
  which narrows per field and composes with `and`/`or`/`not`. It still works,
  mapping `query` onto one filter leaf, and now returns a connection.
- `events`/`agenda`/`searchEvents` no longer take `limit`; use `first`.
- `CalDavClient::list_calendars` returns `Vec<Calendar>` rather than
  `&[Calendar]`, following the cache removal.

## [0.1.0] - 2026-07-24

The initial feature set rather than a set of changes against a released
version.

### Added

- **One binary, two front ends.** A JSON-output CLI (`{"success": true, "data": ...}`)
  and an MCP server over the same CalDAV client, mirroring `fastmail-cli` so
  both tools are learned once.
- **Portable discovery** — RFC 6764 well-known bootstrap → `current-user-principal`
  → `calendar-home-set` → collections, memoised per client so the round trips
  happen once per process.
- **Reads**: `calendars`, `list`, `agenda`, `get`, `search`, `free-busy`, each
  scopable to one calendar and to a time window.
- **Writes**: `create`, `update`, `delete`. Updates change only the fields
  passed and send `If-Match` with the etag that was read, so a concurrent edit
  is reported rather than silently clobbered.
- **Flexible time input** — ISO 8601, `YYYY-MM-DD HH:MM`, bare dates, keywords
  (`today`, `tomorrow`), and relative offsets (`+2h`, `+3d`), interpreted in an
  IANA zone so local wall-clock times survive a DST boundary.
- **Recurrence expansion.** Server-side `<C:expand>` is requested and, when the
  server refuses or ignores it, occurrences are expanded client-side instead —
  the caller gets the same shape either way. `RRULE`/`EXDATE`/`RDATE` evaluation
  is delegated to the `rrule` crate; getting RFC 5545 subtly wrong means showing
  someone the wrong day.
- **MCP server** exposing two tools — `schema_sdl` and `graphql` — over one
  composable schema instead of a tool per operation.
- **Two-phase writes in MCP.** Every mutation takes `PREVIEW` (renders the
  change, returns a one-shot token fingerprinted over the arguments) or
  `CONFIRM`. A confirm whose arguments drifted from its preview is rejected. A
  calendar is shared state; nothing should move without the user seeing it
  described first.
- **Hosted mode** (`mcp --http`) taking credentials per request via
  `X-CalDAV-Username` / `-Password` / `-Url`, for running behind an OAuth
  gateway. Username and password must *both* arrive as headers to be used, so a
  partial header set can never mix with configured credentials and authenticate
  as the wrong account.
- **Credential storage** in `~/.config/caldav-cli/config.toml` (dir `0700`, file
  `0600`), overridable by `CALDAV_*` environment variables. `auth` verifies
  against the server before writing, so a typo fails loudly instead of leaving a
  broken config behind.
- **Shell completions** for bash, zsh, and fish.
- **iCloud accommodations**, each load-bearing rather than an edge case, since
  CalDAV is the only programmatic access Apple offers:
  - Absolute URLs preserved end to end, because authentication happens against
    `caldav.icloud.com` but the calendar home comes back on a per-account
    partition host that all later requests must address.
  - A `User-Agent` on every request — iCloud refuses requests without one, and
    most HTTP clients send none by default.
  - Busy periods derived from events when the free/busy REPORT goes unanswered,
    which it does for a personal calendar home.
  - Writes return the event as written rather than re-reading it, because a
    write is not always visible on the next read.
- **Graceful degradation on read** — one unreadable calendar is logged and
  skipped rather than sinking the whole range.
