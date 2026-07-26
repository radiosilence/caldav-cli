# Changelog

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.6.1] - 2026-07-26

### Changed

- **The image is now a single-stage, package-manager-free copy of a static
  musl binary onto `scratch`, not a compile on `debian:bookworm-slim`.**
  20.1MB down from a debian base. There is no build stage in the Dockerfile at
  all — CI compiles the binary once per arch and `docker build` only ever
  copies it in, so `docker build .` by hand now requires `dist/` to already be
  populated (docker builds only ever happen in CI).
- **The CA bundle is a plain `COPY --from=gcr.io/distroless/static`, not
  `apt-get install ca-certificates`.** Verified empirically on this binary:
  on bare `scratch` with no cert file, `Client::new()` panics with `"No CA
  certificates were loaded from the system"` — `rustls-platform-verifier`
  requires a system trust store and does not fall back to compiled-in webpki
  roots. With the bundle copied in, `caldav calendars` against
  `caldav.fastmail.com` completed TLS and got back the server's own auth
  rejection. Sourcing it from distroless/static avoids needing a package
  manager anywhere in the image build.
- **CI now builds and lints on every PR, not just on push to `main`.** The
  registry push and GitHub release stay gated to `main`, but a broken
  Dockerfile or a clippy/fmt regression now fails before merge. `check` is
  split into separate `test` / `lint` / `format` jobs so a formatting nit
  doesn't block the test job's cache warm-up.
- **Docker layer caching removed from the image build.** Nothing compiles
  inside the image anymore, so there was nothing left for `cache-from`/
  `cache-to` to usefully cache — `mode=max` was filling the repo-wide 10GB
  GitHub Actions cache that `Swatinem/rust-cache` shares with the Rust build
  jobs.
- The container is deliberately bare: `USER 10001:10001`, no `HOME`, no
  writable volume, no config directory. `scratch` has no `/etc/passwd`, so
  `~/.config/caldav-cli/config.toml` (which the CLI reads on other platforms)
  is unreachable in the container; this is fine since the image is normally
  driven entirely by `CALDAV_SERVER_URL` / `CALDAV_USERNAME` /
  `CALDAV_APP_PASSWORD`, and `Config::load()` degrades to defaults rather than
  erroring when the config directory can't be resolved. Verified `caldav
  --version` and `caldav --help` both exit 0 in the container with no config
  file and no `HOME` set. Credential handling in the container is moving to
  request headers entirely in a follow-up, so no HOME/config scaffolding was
  added.

## [0.6.0] - 2026-07-26

### Added

- **`viewer` — is this connection actually authenticated?** A status dot in a
  UI previously had to fire a real query and read the error prose to tell "your
  app password is dead, re-authenticate" from "iCloud is having a moment, wait".
  `viewer` asks the server for `current-user-principal`, which is the cheapest
  question it will only answer for a request it has authenticated, and reports
  `CONNECTED` / `INVALID_CREDENTIALS` / `UNREACHABLE` as data rather than
  raising an error — not being connected is the answer to this question, not a
  failure to answer it. `detail` carries the server's own words for a tooltip;
  the enum is what to branch on. `CalDavClient::principal` is public for it,
  and stays uncached: a probe that answers from memory isn't a probe.

### Fixed

- **A stale `Cargo.lock` no longer reaches the image build.** `check` builds and
  tests with `--locked`, so a lockfile that has drifted from `Cargo.toml` fails
  in the pull request rather than in the Docker build, which was the only step
  using `--locked` and so the only one that noticed.
- **A version tag is never cut without an image behind it.** `publish` now waits
  for the image builds as well as the tarballs. Previously they ran in parallel,
  so a failed image build still produced a GitHub release with nothing to pull.

## [0.5.3] - 2026-07-26

### Changed

- **The `mcp` flags state their own implications, and resolve them in one
  place.** `--browser` implies `--graphiql` implies `--graphql`; `--http` is the
  only flag that puts MCP on the listener, which `--help` now says outright —
  the surface a model connects through and one you can poke at in a browser are
  different things, and confusing them exposes an endpoint nobody asked for.
  `HttpSurfaces` now describes what is mounted rather than what was typed, so
  `graphql` is no longer false while `/graphql` is being served. Same routes as
  before; tests pin each documented invocation. Matches `fastmail-cli`,
  `tfl-mcp` and `mainlynorfolk-mcp`, which take the same three flags.

## [0.5.2] - 2026-07-25

### Changed

- **`mcp --browser` implies `--graphiql`** instead of refusing to run without
  it. Opening a browser at the IDE means serving the IDE, so making the user
  spell out both was arithmetic the tool could do itself — and it already infers
  the listener from any HTTP surface.

## [0.5.1] - 2026-07-25

### Fixed

- **Every write to an existing event failed on iCloud.** `update`, `delete`,
  `move`, `deleteOccurrence` and `respondToInvite` all resolve their target by
  UID first, and iCloud answers a `prop-filter` on `UID` with `412` — the same
  filter on `SUMMARY` is answered fine. Reads were unaffected, since those
  filter on a time range, so an event was visible right up until you tried to
  change it. A lookup now asks for the UID-named resource directly (one
  request, and the hit for anything Apple or this tool wrote), keeps the UID
  query for servers that honour it, and reads the collection to match
  client-side when the filter is refused — which is the only way to reach an
  event whose filename doesn't match its UID on a server that won't filter.
- **A failed lookup reported "event not found".** Errors from the per-calendar
  search were discarded, so a `412`, a `503` or a network fault all surfaced as
  a missing event, sending you to look for a data problem that wasn't there. A
  lookup that fails now fails.

## [0.5.0] - 2026-07-25

### Changed

- **The binary is `caldav`, not `caldav-cli`.** The command you type is the
  protocol it speaks; the `-cli` suffix only ever disambiguated the repository.
  Release tarballs, the container entrypoint, the MCP server identity, the
  `User-Agent` and the generated shell completions all follow. The crate and
  repository keep their names, so `cargo install --git` is unchanged — it just
  installs a differently named binary, and an existing `caldav-cli` on `PATH`
  will linger until removed.
- Config still lives at `~/.config/caldav-cli/`, deliberately: moving it would
  make existing installs re-authenticate for a cosmetic rename.

## [0.4.0] - 2026-07-25

### Added

- **Calendars are writable, not just readable.** `createCalendar`,
  `updateCalendar`, `deleteCalendar` and `setDefaultCalendar` cover the
  collection management the schema previously had no answer for — a model could
  see that a calendar was the wrong colour, or that the default pointed
  somewhere unhelpful, and do nothing about it. `updateCalendar` takes typed
  fields rather than exposing raw DAV property names: those aren't in the SDL,
  so a caller can't discover them, and a misnamed one is silently dropped.
- **`moveEvent`** relocates an event between calendars, keeping its UID.
  WebDAV `MOVE` first, falling back to transferring the raw resource for servers
  that refuse it. Both paths move the stored bytes rather than rebuilding from
  our model, so alarms, attachments and override components survive — which
  delete-then-recreate would not.
- **`deleteOccurrence`** cancels one instance of a recurring series via
  `EXDATE`, leaving the rest standing. Previously the only way to drop a single
  standup was to delete the series. `occurrence` accepts a bare date when the
  series runs once that day; the PREVIEW names the instant it resolved to, and
  an ambiguous date is refused with the candidates listed rather than guessed
  at.
- **`respondToInvite`** sets your own `PARTSTAT`, which the server turns into a
  reply to the organiser. `attendee` picks the row when the invitation went to
  an alias rather than the login address — the common case on iCloud.
- **Per-property PROPPATCH failures are errors.** A PROPPATCH answers `207`
  whatever it accepted; the real verdict is the status inside each `propstat`.
  Those are now read, so a wholly-refused patch fails loudly instead of looking
  like a success. iCloud refuses `schedule-default-calendar-URL` exactly this
  way.

### Fixed

- **An update no longer resurrects occurrences the user had cancelled.**
  `EXDATE`, `RDATE` and `EXRULE` were read and expanded but never written back,
  so rebuilding a series on any edit — a new location, a renamed title — silently
  restored every excluded instance. They are now carried across verbatim.
  Replacing the `RRULE` still drops them, since exceptions to a rule that no
  longer exists describe nothing.

### Changed

- **The two-phase guard covers every write that changes or removes something**,
  which now means moves, RSVPs, occurrence cancellations, and calendar renames
  and deletions as well as event updates and deletes. Writes that only add
  (`createEvent`, `createCalendar`) or only record a preference
  (`setDefaultCalendar`) still go straight through. `deleteCalendar`'s preview
  counts what would be lost, because "this deletes the calendar" says nothing
  about the scale of it.

## [0.3.0] - 2026-07-25

### Added

- **A browsable GraphQL endpoint.** `--graphql` serves plain GraphQL-over-HTTP
  at `/graphql`, `--graphiql` adds the GraphiQL IDE at `/`, and `--browser`
  opens it once the port is bound. Each surface is independent of `--http`,
  which serves MCP's own transport at `/mcp`: the endpoint a model connects
  through and one you can poke at in a browser are different things that share
  a port. They also share the schema and the client cache, so the IDE sees
  exactly what a model sees. `/mcp` speaks JSON-RPC, which a browser doesn't,
  which is why the IDE needs its own route.
- **Introspection needs no credentials.** It is answered from the schema
  without touching CalDAV, so GraphiQL's docs, autocomplete and explorer work
  before you have a working app password. Anything selecting a real field
  authenticates as normal.
- **`--http` takes an optional address**, defaulting to `127.0.0.1:8080`.

### Changed

- **HTTP mode falls back to the local credentials** when a request carries no
  `X-CalDAV-*` headers, so running it yourself needs no ceremony. A hosted
  deployment ships no config, so the fallback is absent there and every request
  must still carry its own headers. Do not run it with local credentials on a
  non-loopback address: anything that can reach the port gets your calendar.

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
  descriptions load into every session, most of which never mention a calendar.
  The prose that lived there — usage rules, worked examples — was restating
  what the schema's own field descriptions already say, so it is gone rather
  than relocated: `calendar_schema` returns the SDL and nothing else, and the
  handful of rules the schema couldn't express (get approval before CONFIRM)
  are now field descriptions themselves. Connecting the server costs ~230
  tokens instead of ~750, and one generated document is the only source of
  truth about the API.
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
- **The new schema documents itself.** Everything a caller needs — how to page,
  what a cursor is, that `totalCount` is free, that filters nest, what each
  field costs — lives in the field descriptions, so `calendar_schema` stays the
  single source of truth rather than growing a prelude beside it. A test asserts
  the SDL really does carry that guidance, since nothing else now would.
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
