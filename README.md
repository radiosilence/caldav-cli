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
calendar = "Personal"
```

Environment variables override the file: `CALDAV_SERVER_URL`,
`CALDAV_USERNAME`, `CALDAV_APP_PASSWORD`, `CALDAV_CALENDAR`.

`calendar` is where new events go when none is named. Leave it unset and the
account's own default calendar wins — the one the server advertises via
`schedule-default-calendar-URL` and your calendar app writes to, flagged as
`isDefault` in `caldav-cli calendars`.

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

One tool, `calendar`: execute a GraphQL query or mutation. The SDL ships inside
the tool description rather than behind a separate introspection tool — ~2k
tokens up front against a discovery round trip on every session.

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

Server-side expansion is unreliable in practice — iCloud especially — so
anything the server hands back unexpanded is expanded **client-side** instead,
and the result is the same shape either way. `RRULE`/`EXDATE`/`RDATE`
evaluation is done by the [`rrule`](https://crates.io/crates/rrule) crate
rather than hand-rolled; recurrence is a large corner of RFC 5545 and getting
it subtly wrong means silently showing someone the wrong day. Occurrences are
expanded in the series' own timezone, so a weekly 09:00 meeting stays at 09:00
across a DST boundary.

Server-side overrides are respected: an edited occurrence replaces its
generated slot, and one marked `CANCELLED` removes it.

To **edit** a series, fetch it unexpanded (`events(expand: false)` or
`caldav-cli list --no-expand`) and update the master event.

### Hosted mode

`caldav-cli mcp --http ADDR` serves streamable HTTP at `/mcp` with **no**
credentials baked in. Each request must carry them, injected by a trusted
upstream after authenticating the user:

| Header               | Required | Meaning                              |
| -------------------- | -------- | ------------------------------------ |
| `X-CalDAV-Username`  | yes      | Account username                     |
| `X-CalDAV-Password`  | yes      | App-specific password                |
| `X-CalDAV-Url`       | no       | Server base URL; defaults to iCloud  |
| `X-CalDAV-Calendar`  | no       | Calendar for new events; defaults to the account's own |

Username and password must both arrive as headers to be used — a partial
header set never mixes with configured credentials, which would otherwise
authenticate as the wrong account.

This is the transport `jaritanet-mcp-gateway` puts behind OAuth. Because
CalDAV needs three values rather than one bearer token, the gateway stores a
credential *set* for this MCP.

## Coverage

CalDAV is a large surface and most of it is calendar-management plumbing this
tool has no use for. What's implemented is the read/write path for events;
what's missing is missing on purpose unless marked otherwise.

### Protocol

| Feature | Spec | Status |
| --- | --- | --- |
| Principal + calendar-home discovery | RFC 4791 §6, RFC 6764 well-known | Full, memoised per client |
| List collections, names, colours, descriptions | RFC 4791, Apple `ic:` ext | Full |
| Read-only detection | `current-user-privilege-set` | Full; absent privileges assumed writable |
| `calendar-query` time-range REPORT | RFC 4791 §7.8 | Full |
| Server-side recurrence `<C:expand>` | RFC 4791 §9.6.5 | Requested, with client-side fallback |
| Free/busy REPORT | RFC 4791 §7.10 | Tried first, derived from events when refused |
| Server-side `text-match` search | RFC 4791 §7.8.5 | **Not used** — per-property and inconsistently implemented, so search is client-side |
| Optimistic concurrency | `ETag` / `If-Match` | Full on update and delete |
| `calendar-multiget` | RFC 4791 §7.9 | Not implemented — the time-range query already returns the data |
| Sync tokens / incremental sync | RFC 6578 | Not implemented; every read is a fresh window query |
| Create/delete/rename calendars | `MKCALENDAR`, `PROPPATCH` | Not implemented |
| Scheduling — invites, RSVP, inbox/outbox | RFC 6638 | Not implemented. Attendees and their `PARTSTAT` are read and written as event properties; whether that generates invitations is the server's implicit-scheduling behaviour, not something this client drives |
| Sharing and ACLs | RFC 3744, Apple ext | Not implemented beyond the read-only flag |

### Event data

| Feature | Status |
| --- | --- |
| `VEVENT` | Read and written |
| `VTODO` / `VJOURNAL` | Not supported; collections holding only these are skipped on read |
| Summary, description, location, URL, status, categories | Read and written |
| Organizer, attendees, with `CN` / `ROLE` / `PARTSTAT` | Read and written |
| All-day (`VALUE=DATE`) and `TZID` local times | Read and written |
| `DURATION` as an alternative to `DTEND` | Read; always written as `DTEND` |
| `RRULE`, `EXDATE`, `RDATE`, `EXRULE` | Read and expanded; only `RRULE` is settable |
| `RECURRENCE-ID` overrides | Respected on read — an edited occurrence replaces its generated slot, a cancelled one disappears |
| Editing a single occurrence of a series | **Not supported.** Updates target the master event and rewrite the whole resource, which drops sibling override components |
| `VALARM` reminders | Not parsed and not written — **an update strips existing alarms** |
| `ATTACH`, `GEO`, `CLASS`, `TRANSP`, `X-` properties | Not modelled — **also dropped on update** |
| `VTIMEZONE` components | Not emitted. Writes reference an IANA `TZID` without defining it, which every tested server accepts but is not strictly conformant |

The three bolded rows share one cause: an update rebuilds the iCalendar object
from the parsed model rather than patching the original text, so anything
outside the model is lost. That is fine for events this tool created and lossy
for events it didn't — worth knowing before pointing it at a calendar full of
invitations with alarms on them.

## Apple / iCloud

Apple publishes **no** REST API, SDK, OAuth flow, or developer program for
iCloud Calendar — CalDAV is the only programmatic access, and it isn't
officially documented. `EventKit` exists but is local-only (an app running on
the user's Mac or iPhone), so it's no help to a server-side tool. That makes
CalDAV the target by necessity, not preference.

Apple's server is a fork of the discontinued Apple CalendarServer and has
drifted. The quirks that actually bite, and what this client does about them:

| Quirk | Handling |
| --- | --- |
| **Partition hosts.** You authenticate against `caldav.icloud.com`, but your `calendar-home-set` comes back on a per-account shard like `p42-caldav.icloud.com`, and every later request must address *that* host. | Discovery keeps absolute URLs and resolves each href against the response it came from, never against the configured server URL. |
| **Requires a `User-Agent`.** A request without one is refused outright — and most HTTP clients (reqwest included) send none by default. | Every request identifies as `caldav-cli/<version>`. |
| **`<C:expand>` is unreliable**, so an agenda can come back as master events at the wrong times. | Expansion falls back to client-side, transparently. |
| **No free/busy.** iCloud doesn't answer the free-busy REPORT for a personal calendar home. | Falls back to deriving busy periods from the events. |
| **App-specific passwords only** — the Apple ID password is rejected, and there is no OAuth. | `auth` verifies credentials against the server before storing them, so a wrong password fails immediately with a clear message instead of a confusing discovery error. |
| **Eventual consistency** — a write is not always visible on the next read. | Writes return the event as written rather than re-reading it. |

Fastmail, Nextcloud, and Radicale are better-behaved and work through the same
code path; iCloud is simply the one that needs the accommodations.

## Design notes

- **iCalendar parsing is hand-rolled; recurrence is not.** The VEVENT and
  VFREEBUSY subset is small and well-specified, so folding, parameter quoting,
  TEXT escaping, and `VALUE=DATE`/`TZID`/UTC time forms are handled here and
  covered by tests. Recurrence *rules* are a different matter — that is
  delegated to `rrule`, which is mature and widely used.
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
cargo test          # 137 tests, including an end-to-end suite against a mock server
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

## License

MIT
