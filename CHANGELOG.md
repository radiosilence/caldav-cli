# Changelog

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Nothing is tagged yet, so this is the initial feature set rather than a set of
changes against a released version.

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
