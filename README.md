# caldav-cli

A CLI and MCP server for CalDAV calendars — iCloud by default, any RFC 4791
server by configuration.

Built to the same shape as [`fastmail-cli`](https://github.com/radiosilence/fastmail-cli):
a Rust binary that is both a scriptable JSON-output CLI and a
[Model Context Protocol](https://modelcontextprotocol.io) server exposing one
composable GraphQL interface instead of a tool per operation.

```bash
caldav-cli agenda --days 1 --tz Europe/London
caldav-cli create --summary "Coffee" --start "tomorrow 15:00" --duration 30 --tz Europe/London
caldav-cli mcp                       # stdio MCP server for Claude
caldav-cli mcp --http 0.0.0.0:8080   # hosted mode, credentials per request
```

## Why this exists

Calendars are the other half of "what is my day". `fastmail-cli` handles mail
and contacts over JMAP and CardDAV; this handles events over CalDAV. Both
plug into [`jaritanet-mcp-gateway`](https://github.com/radiosilence/jaritanet-mcp-gateway)
as OAuth-fronted backends, so Claude reaches them with credentials injected
per request and never sees the secret.

## Install

```bash
cargo install --git https://github.com/radiosilence/caldav-cli
```

Or grab a binary from [releases](https://github.com/radiosilence/caldav-cli/releases).

## Setup

CalDAV uses HTTP Basic auth with an **app-specific password** — iCloud rejects
your normal Apple ID password for CalDAV, and Fastmail rejects API tokens.

| Provider  | Server URL                     | Where to get a password                                        |
| --------- | ------------------------------ | -------------------------------------------------------------- |
| iCloud    | `https://caldav.icloud.com` (default) | [appleid.apple.com](https://appleid.apple.com) → Sign-In and Security → App-Specific Passwords |
| Fastmail  | `https://caldav.fastmail.com`  | [Settings → Privacy & Security → App passwords](https://app.fastmail.com/settings/security/apps) |
| Google    | `https://apidata.googleusercontent.com/caldav/v2` | App password (requires 2FA) |
| Nextcloud | `https://<host>/remote.php/dav` | Settings → Security → Devices & sessions |

```bash
# Reads the password from stdin so it stays out of `ps` and shell history
caldav-cli auth --username you@icloud.com

# Or name a different server
caldav-cli auth --username you@fastmail.com --server-url https://caldav.fastmail.com
```

`auth` verifies the credentials against the server before writing anything, so
a typo fails loudly instead of leaving a broken config behind.

Config lands in `~/.config/caldav-cli/config.toml` (dir `0700`, file `0600`):

```toml
[core]
server_url = "https://caldav.icloud.com"
username = "you@icloud.com"
app_password = "abcd-efgh-ijkl-mnop"
```

Environment variables override the file: `CALDAV_SERVER_URL`,
`CALDAV_USERNAME`, `CALDAV_APP_PASSWORD`.

Debug the wire traffic with `RUST_LOG=debug caldav-cli [cmd]`.

## Commands

All output is JSON: `{"success": true, "data": ...}`.

```bash
caldav-cli calendars                      # discover calendars and their ids

caldav-cli agenda [--days N] [--tz TZ] [-c CAL] [-l LIMIT]
caldav-cli list [-c CAL] [--start S] [--end E] [--days N] [--tz TZ] [-l N] [--no-expand]
caldav-cli get EVENT_UID [-c CAL]
caldav-cli search QUERY [-c CAL] [--start S] [--end E] [--days N] [-l N]
caldav-cli free-busy [--start S] [--end E] [--days N] [--tz TZ]

caldav-cli create [-c CAL] --summary S --start S [OPTIONS]
caldav-cli update EVENT_UID [OPTIONS]
caldav-cli delete EVENT_UID -y

caldav-cli completions bash|zsh|fish
caldav-cli mcp [--http ADDR]
```

Event options shared by `create` and `update`:

```
--summary --start --end --duration MINUTES --all-day --tz
--description --location --url --status --recurrence
--attendee 'Name <email>'   (repeatable)
--category TAG              (repeatable)
```

On `update`, only the flags you pass change; everything else is preserved, and
the event's stored duration is kept when you move only its start.

### Times

Anywhere a time is accepted:

| Form              | Example                                     |
| ----------------- | ------------------------------------------- |
| ISO 8601          | `2026-07-24T09:00:00Z`, `2026-07-24T09:00:00+01:00` |
| Date + time       | `2026-07-24 09:00`, `2026-07-24T09:00`      |
| Bare date (all-day) | `2026-07-24`, `24/07/2026`                |
| Keyword           | `now`, `today`, `tomorrow`, `yesterday`     |
| Relative          | `+90m`, `-2h`, `+3d`, `+1w`                 |

Values without an offset are interpreted in `--tz` (UTC when absent). Pass
`--tz` whenever the user means a local wall-clock time — the event is then
written with a `TZID`, so it moves correctly across DST.

## MCP server

Two tools, following `fastmail-cli`'s design:

- `schema_sdl` — the full GraphQL SDL, for discovering what's available
- `graphql` — execute a query or mutation

```bash
claude mcp add --scope user caldav -- caldav-cli mcp
```

```graphql
{ calendars { id name color readOnly } }

{ agenda(days: 1, tz: "Europe/London") {
    id summary location start { dateTime date allDay } end { dateTime } } }

{ searchEvents(query: "dentist", days: 90) { id summary start { dateTime date } } }

{ freeBusy(start: "today", days: 3) { start end status } }
```

### Writes are two-phase

Every mutation takes an `action`. `PREVIEW` renders what would change and
returns a one-shot `confirmationToken`; `CONFIRM` applies it. The token is
bound to a fingerprint of the arguments, so a confirm whose arguments drifted
from its preview is rejected rather than silently doing something else.

```graphql
mutation { createEvent(action: PREVIEW, summary: "Coffee",
    start: "tomorrow 15:00", durationMinutes: 30, tz: "Europe/London") {
  preview confirmationToken } }

mutation { createEvent(action: CONFIRM, summary: "Coffee",
    start: "tomorrow 15:00", durationMinutes: 30, tz: "Europe/London",
    confirmationToken: "...") { event { id summary } } }
```

`updateEvent` previews a before → after diff; `deleteEvent` previews the event
it is about to remove. A calendar is shared, visible state — nothing should
move without the user seeing it described first.

### Recurring events

`events` and `agenda` ask the server to expand recurring series, so each
occurrence in the window comes back as its own result carrying a
`recurrenceId`. All occurrences of a series share the series `id` (its UID).

To **edit** a series, fetch it unexpanded (`events(expand: false)` or
`caldav-cli list --no-expand`) and update the master event. Servers that don't
implement `<expand>` are detected and the query is retried unexpanded.

### Hosted mode

`caldav-cli mcp --http ADDR` serves streamable HTTP at `/mcp` with **no**
credentials baked in. Each request must carry them, injected by a trusted
upstream after authenticating the user:

| Header               | Required | Meaning                              |
| -------------------- | -------- | ------------------------------------ |
| `X-CalDAV-Username`  | yes      | Account username                     |
| `X-CalDAV-Password`  | yes      | App-specific password                |
| `X-CalDAV-Url`       | no       | Server base URL; defaults to iCloud  |

Username and password must both arrive as headers to be used — a partial
header set never mixes with configured credentials, which would otherwise
authenticate as the wrong account.

This is the transport `jaritanet-mcp-gateway` puts behind OAuth. Because
CalDAV needs three values rather than one bearer token, the gateway stores a
credential *set* for this MCP.

## Design notes

- **iCalendar is hand-rolled.** We touch a small, well-specified subset
  (VEVENT and VFREEBUSY); a full library would bring far more surface than the
  job needs. Folding, parameter quoting, TEXT escaping, and `VALUE=DATE` vs
  `TZID` vs UTC forms are all covered by tests.
- **Discovery is the portable walk**: `current-user-principal` →
  `calendar-home-set` → collections, with an RFC 6764 well-known bootstrap
  first. It is memoised per client, so the three round trips happen once.
- **Search is client-side.** CalDAV's server-side `text-match` is per-property
  and inconsistently implemented, so a window is fetched once and filtered
  across title, notes, location, categories, and attendees.
- **Free/busy degrades gracefully.** The standard REPORT is tried first;
  iCloud doesn't answer it for a personal calendar home, so busy periods are
  derived from the events instead.
- **Writes use optimistic concurrency.** Updates send `If-Match` with the etag
  that was read; a concurrent edit returns an error telling you to re-read
  rather than silently clobbering someone else's change.
- **One unreadable calendar doesn't sink the agenda** — it's logged and skipped.

## Development

```bash
cargo test          # 118 tests, including an end-to-end suite against a mock server
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

## License

MIT
