# Changelog

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **A default calendar for new events.** `createEvent` / `caldav-cli create`
  with no calendar named now lands where the user's calendar app would put it,
  rather than in whichever writable collection sorted first alphabetically.
  Calendars carry `isDefault`, and it is honoured on writes only — reads still
  span the account, since scoping an agenda to one calendar hides the rest
  without saying so.
- **An override**, for accounts whose own default isn't where automation should
  write: `X-CalDAV-Calendar` per request in hosted mode, or `calendar` in
  `config.toml` / `CALDAV_CALENDAR`. The MCP server announces the choice in its
  `schema_sdl` output, so a model knows where it is writing before it writes.
- **Discovery that doesn't assume a server's shape**, which iCloud rewards:
  it answers `schedule-default-calendar-URL` as bare element text rather than
  the `DAV:href` RFC 6638 specifies, and echoes the property name back empty in
  the `404` propstat of every collection that hasn't got it. The value is read
  in whichever shape arrives, from the calendar-home listing or — only when
  that says nothing — the scheduling inbox, the one location the RFC requires.

### Changed

- **Docker images tagged by version.** `ghcr.io/radiosilence/caldav-cli` now
  gets `vX.Y.Z`, `vX.Y`, `vX`, and `latest` tags alongside `main` and
  `sha-<short>`, cut only on the push that first introduces that version in
  `Cargo.toml` so the tags never drift onto a later, unrelated commit.

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
