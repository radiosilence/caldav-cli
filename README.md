# caldav-cli

A CLI and MCP server for CalDAV calendars — iCloud by default, any RFC 4791
server by configuration.

Built to the same shape as [`fastmail-cli`](https://github.com/radiosilence/fastmail-cli):
a Rust binary that is both a scriptable JSON-output CLI and a
[Model Context Protocol](https://modelcontextprotocol.io) server exposing one
composable GraphQL interface instead of a tool per operation.

```bash
caldav agenda --days 1 --tz Europe/London
caldav create --summary "Coffee" --start "tomorrow 15:00" --duration 30 --tz Europe/London
caldav mcp                             # stdio MCP server for Claude
caldav mcp --browser                    # GraphiQL in your browser
caldav mcp --http 0.0.0.0:8080         # hosted mode, credentials per request
```

## Why this exists

Calendars are the other half of "what is my day". `fastmail-cli` handles mail
and contacts over JMAP and CardDAV; this handles events over CalDAV. Both
plug into [`jaritanet-mcp-gateway`](https://github.com/radiosilence/jaritanet-mcp-gateway)
as OAuth-fronted backends, so Claude reaches them with credentials injected
per request and never sees the secret.

## Install

```bash
mise use -g "github:radiosilence/caldav-cli"
```

Tracks the [release](https://github.com/radiosilence/caldav-cli/releases)
tarballs, which hold a single binary named `caldav` — no `[exe=…]` needed.

From source instead:

```bash
cargo install --git https://github.com/radiosilence/caldav-cli
```

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
caldav auth --username you@icloud.com

# Or name a different server
caldav auth --username you@fastmail.com --server-url https://caldav.fastmail.com
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
`isDefault` in `caldav calendars`.

Debug the wire traffic with `RUST_LOG=debug caldav [cmd]`.

## Commands

All output is JSON: `{"success": true, "data": ...}`.

```bash
caldav calendars                      # discover calendars and their ids

caldav agenda [--days N] [--tz TZ] [-c CAL] [-l LIMIT]
caldav list [-c CAL] [--start S] [--end E] [--days N] [--tz TZ] [-l N] [--no-expand]
caldav get EVENT_UID [-c CAL]
caldav search QUERY [-c CAL] [--start S] [--end E] [--days N] [-l N]
caldav free-busy [--start S] [--end E] [--days N] [--tz TZ]

caldav create [-c CAL] --summary S --start S [OPTIONS]
caldav update EVENT_UID [OPTIONS]
caldav delete EVENT_UID -y

caldav completions bash|zsh|fish
caldav mcp [--http [ADDR]] [--graphql] [--graphiql] [--browser]
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

- `calendar_schema` — the GraphQL SDL
- `calendar` — execute a query or mutation

The schema is ~5k tokens, so it stays behind a tool call rather than in the
tool descriptions, which every session loads whether or not it goes near a
calendar. Connecting this server costs ~230 tokens until something actually
asks about the calendar.

```bash
claude mcp add --scope user caldav -- caldav mcp
```

```graphql
{ calendars { nodes { id name color readOnly } } }

{ agenda(days: 1, tz: "Europe/London") {
    nodes { id summary location start { dateTime date allDay } end { dateTime } } } }

{ events(filter: { text: "dentist" }, days: 90) {
    nodes { id summary start { dateTime date } } } }

{ freeBusy(start: "today", days: 3) { nodes { start end status } } }
```

### It's a graph

Every collection is a connection (`nodes`, `edges`, `pageInfo`, `totalCount`,
`first`/`after`), and everything below one is lazy — a field that isn't
selected issues no request.

The motivation is call count. "What's on today, whose calendar is it, and does
anything clash" used to be `agenda` → `calendars` → an `events` call per
result, with the model composing each round trip. Now it's one query:

```graphql
{
  agenda(days: 1, tz: "Europe/London") {
    totalCount
    calendarsQueried
    nodes {
      summary
      durationMinutes
      calendar { name color readOnly }
      conflicts { totalCount nodes { summary calendarName } }
    }
  }
}
```

Cost: one `PROPFIND` for the calendar listing, shared by every calendar lookup
in the document → one `calendar-query` REPORT per event-holding calendar →
**nothing** for the conflicts, because they resolve the window the agenda
already fetched.

Filters are a tree rather than flat arguments. Scalars on one object AND
together; `and`/`or`/`not` nest arbitrarily:

```graphql
{
  events(
    days: 14
    filter: {
      status: CONFIRMED
      or: [{ attendee: "alice@example.com" }, { organizer: "alice@example.com" }]
      not: [{ allDay: true }]
    }
    sort: [{ property: START, ascending: true }]
    first: 20
  ) { totalCount nodes { summary start { dateTime } } }
}
```

Pagination is by event id, so `after:` is an id you have already seen:

```graphql
{ events(days: 30, first: 25, after: "evt-abc") {
    totalCount pageInfo { hasNextPage endCursor } edges { cursor node { summary } } } }
```

### What each field costs

CalDAV is stingier than JMAP about batching, so the schema says what it can and
can't collapse rather than implying it away:

| Read | Request | Batching |
| --- | --- | --- |
| Calendar listing | one `PROPFIND` | whole list — one call however many fields ask |
| `events` / `agenda` / `Calendar.events` | `calendar-query` REPORT, one per collection | no plural form exists; identical windows deduplicate, distinct ones go out concurrently |
| `Event.series` | `calendar-multiget` REPORT | a true batch — every href in one collection, one request |
| `event(id:)` | `calendar-query` REPORT | CalDAV ANDs all sibling filters (RFC 4791 §9.7), so there is no OR over UIDs: one request per calendar until it hits. Pass `calendar:` to make it exactly one |
| `Event.occurrences` | none | evaluated from the `RRULE` already in hand |

Two consequences worth knowing, both stated in the schema:

- **`totalCount` is free and exact.** CalDAV has no windowed query — the range
  arrives whole — so the count is a length, not extra work for the server.
- **Paging is slicing, and a cursor lasts as long as its event.** If the event
  is gone you get a "restart pagination" error, not a quietly different page.

`calendarsQueried` on every event connection reports how many collections were
hit, so the cost isn't hidden. Naming `calendar:` brings it to one.

Query cost is declared per field but **not** capped — refusing an
expensive-but-legitimate query leaves the caller guessing at a threshold it
can't see. Depth is capped at 15, because the graph has cycles by design
(`event → calendar → events`) and nothing else bounds them.

### Adding is one step; changing is two

`createEvent`, `createCalendar` and `setDefaultCalendar` write immediately. A
wrong new event is visible and deletable, a wrong new calendar likewise, and a
misdirected default is one call to put back — so a confirmation round trip buys
nothing the user's own eyes don't. The model reports what it did and gets
corrected if it guessed badly. `setDefaultCalendar` returns `previousDefault` so
that correction is one call away.

```graphql
mutation { createEvent(summary: "Coffee", start: "tomorrow 15:00",
    durationMinutes: 30, tz: "Europe/London") { event { id summary } } }
```

Everything that overwrites or removes existing state takes an `action`:
`updateEvent`, `deleteEvent`, `moveEvent`, `deleteOccurrence`,
`respondToInvite`, `updateCalendar`, `deleteCalendar`. `PREVIEW` renders what
would change — a before → after diff, the event about to go, the occurrence a
bare date resolved to, how many events a calendar deletion would take with it —
and returns a one-shot `confirmationToken`; `CONFIRM` applies it. The token is
bound to a fingerprint of the arguments, so a confirm whose arguments drifted
from its preview is rejected rather than silently doing something else.

```graphql
mutation { deleteEvent(action: PREVIEW, id: "...") { preview confirmationToken } }

mutation { deleteEvent(action: CONFIRM, id: "...",
    confirmationToken: "...") { success } }
```

`respondToInvite` is in that list because it sends a message to another person:
the server turns your changed `PARTSTAT` into a reply delivered to the
organiser. `attendee` picks which row is you when the invitation arrived at an
alias rather than the login address, which is the normal case on iCloud.

### Calendars, not just events

Collections are writable too — `createCalendar`, `updateCalendar`,
`deleteCalendar`, `setDefaultCalendar`. `updateCalendar` takes typed fields
(`name`, `description`, `color`, `order`) rather than exposing raw DAV property
names. That is deliberate: property names and their namespaces aren't in the
SDL, so a caller can't discover them, and a PROPPATCH answers `207` whatever it
accepted — the real verdict is the per-`propstat` status. A misnamed property
would be a silent no-op. Those statuses are read, so a refusal is an error
naming the property and the code.

`setDefaultCalendar` writes `schedule-default-calendar-URL` on the scheduling
inbox (RFC 6638), which is the property the user's *own* calendar apps read to
decide where a new event goes — so it reaches well beyond this tool. Not every
server lets it be set; iCloud generally refuses, and the refusal comes back in
`error` rather than being swallowed.

Note the distinction from the configured `calendar`: that one decides where
*this tool* puts an event when the model doesn't name one, and comes from config
or the `X-CalDAV-Calendar` header. `setDefaultCalendar` changes the account.

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
`caldav list --no-expand`) and update the master event. From an expanded
occurrence, `series { ... }` walks to that master directly — a whole page of
occurrences resolves through one `calendar-multiget`.

To **cancel one occurrence**, use `deleteOccurrence`, which adds an `EXDATE` to
the master rather than touching the series otherwise — `deleteEvent` would take
the lot. `occurrence` accepts a bare date when the series runs once that day, so
a caller needn't know what time it runs at; the `PREVIEW` names the instant it
resolved to, and a date matching several occurrences is refused with them listed
rather than guessed at.

```graphql
mutation { deleteOccurrence(action: PREVIEW, id: "...",
    occurrence: "2026-08-03") { preview confirmationToken } }
```

`Event.occurrences` goes the other way: it evaluates the rule already in hand,
so "where does this series actually fall over the next quarter" costs no
request at all.

```graphql
{ events(expand: false, days: 1, filter: { recurring: true }) {
    nodes { summary recurrence
      occurrences(days: 90) { totalCount nodes { start { dateTime } } } } } }
```

### HTTP surfaces

Three independent surfaces, each opt-in, sharing one port (default
`127.0.0.1:8080`, or pass an address to `--http`):

| Flag         | Serves                                                  |
| ------------ | ------------------------------------------------------- |
| `--http`     | MCP streamable-HTTP at `/mcp`                           |
| `--graphql`  | plain GraphQL-over-HTTP at `/graphql`                   |
| `--graphiql` | the GraphiQL IDE at `/`; implies `--graphql`             |
| `--browser`  | opens the IDE once bound; implies `--graphiql`           |

```bash
caldav mcp                                   # stdio MCP, no listener
caldav mcp --browser                         # just the IDE, opened for you
caldav mcp --graphiql --http                 # the IDE and /mcp, nothing opened
caldav mcp --http                            # just /mcp
caldav mcp --http 0.0.0.0:8080 --graphql     # both, explicit address
```

Asking for any surface binds the listener; there is nowhere to mount an HTTP
route over stdio. Only `--http` puts MCP on it — the transport a model connects
through and a browsable endpoint for you are separate things. `--browser`
implies `--graphiql`, since the IDE is what it opens.

`/graphql` is plain GraphQL-over-HTTP, which is what a browser speaks; `/mcp` is
MCP JSON-RPC, which it doesn't. That is why GraphiQL needs its own route rather
than pointing at the MCP one. Both share the schema, the client cache and the
credential resolution below, so the IDE sees exactly what a model sees.

**Introspection needs no credentials**: it is answered from the schema without
touching CalDAV, so GraphiQL's docs, autocomplete and explorer work before you
have a working app password. Queries selecting any real field authenticate as
normal, on first use rather than at startup — a wrong password shows up in the
response pane instead of stopping the server booting.

#### Credentials

A request's own headers win; the configured credentials are the fallback:

| Header              | Required | Meaning                                                |
| ------------------- | -------- | ------------------------------------------------------ |
| `X-CalDAV-Username` | yes      | Account username                                       |
| `X-CalDAV-Password` | yes      | App-specific password                                  |
| `X-CalDAV-Url`      | no       | Server base URL; defaults to iCloud                    |
| `X-CalDAV-Calendar` | no       | Calendar for new events; defaults to the account's own  |

Username and password must both arrive as headers to be used — a partial header
set never mixes with configured credentials, which would otherwise authenticate
as the wrong account. Header credentials never inherit the configured calendar
either, so one account's request can't land an event in another's.

Running it yourself, the fallback means your own account with no ceremony. In a
hosted deployment there is no local config, so the fallback is absent and every
request must carry the headers, injected by a trusted upstream after it has
authenticated the caller. That is the transport `jaritanet-mcp-gateway` puts
behind OAuth; because CalDAV needs three values rather than one bearer token,
the gateway stores a credential *set* for this MCP.

Do **not** expose this to the internet without such an auth layer in front —
the headers are trusted unconditionally. Equally, do not run it with local
credentials present on a non-loopback address: anything that can reach the port
gets your calendar without needing a header at all.

## Coverage

CalDAV is a large surface. What's implemented is the read/write path for events
and the calendars holding them; what's missing is missing on purpose unless
marked otherwise. Sync tokens, sharing, and the scheduling inbox/outbox are the
notable absences.

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
| `calendar-multiget` | RFC 4791 §7.9 | Full — the one place CalDAV batches, used to resolve a page of occurrences to their masters in one request |
| Sync tokens / incremental sync | RFC 6578 | Not implemented; every read is a fresh window query |
| Create/delete/rename calendars | `MKCALENDAR`, `PROPPATCH`, `DELETE` | Full. Display name, description, colour and sidebar order; per-`propstat` statuses are read, so a refused property is an error rather than a silent no-op |
| Default calendar | RFC 6638 §9.2 `schedule-default-calendar-URL` | Read, and settable via `PROPPATCH` on the scheduling inbox. Many servers — iCloud included — refuse the write; the refusal is reported |
| Move a resource between collections | `MOVE` (RFC 4918 §9.9) | Full, falling back to a verbatim copy plus delete. Both paths move the stored bytes, so nothing outside our model is lost |
| Scheduling — RSVP | RFC 6638 §3.2.5 | `respondToInvite` writes your own `PARTSTAT`, which is how a reply is generated. Whether the server actually delivers one is its implicit-scheduling behaviour |
| Scheduling — inbox/outbox, `iTIP` freebusy | RFC 6638 | Not implemented. The inbox is located only to read and write the default-calendar property |
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
| `RRULE` | Read, expanded, and settable |
| `EXDATE`, `RDATE`, `EXRULE` | Read, expanded, and preserved verbatim across an update. `EXDATE` is also writable through `deleteOccurrence`; `RDATE`/`EXRULE` are carried but not authored. Replacing the `RRULE` drops all three, since exceptions to a rule that no longer exists describe nothing |
| `RECURRENCE-ID` overrides | Respected on read — an edited occurrence replaces its generated slot, a cancelled one disappears |
| Cancelling a single occurrence | Supported via `deleteOccurrence`, which adds an `EXDATE` |
| Editing a single occurrence of a series | **Not supported.** Updates target the master event and rewrite the whole resource, which drops sibling override components |
| `VALARM` reminders | Not parsed and not written — **an update strips existing alarms** |
| `ATTACH`, `GEO`, `CLASS`, `TRANSP`, `X-` properties | Not modelled — **also dropped on update** |
| `VTIMEZONE` components | Not emitted. Writes reference an IANA `TZID` without defining it, which every tested server accepts but is not strictly conformant |

The three bolded rows share one cause: an update rebuilds the iCalendar object
from the parsed model rather than patching the original text, so anything
outside the model is lost. That is fine for events this tool created and lossy
for events it didn't — worth knowing before pointing it at a calendar full of
invitations with alarms on them.

`moveEvent` is the exception, and deliberately so: it relocates the stored
resource rather than rebuilding it, so an event with alarms survives a move even
though it would not survive an edit.

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
| **Requires a `User-Agent`.** A request without one is refused outright — and most HTTP clients (reqwest included) send none by default. | Every request identifies as `caldav/<version>`. |
| **`<C:expand>` is unreliable**, so an agenda can come back as master events at the wrong times. | Expansion falls back to client-side, transparently. |
| **No free/busy.** iCloud doesn't answer the free-busy REPORT for a personal calendar home. | Falls back to deriving busy periods from the events. |
| **A `prop-filter` on `UID` is refused** with `412`, though the same filter on `SUMMARY` is answered — so the standard way to fetch one event by its id is unavailable. | A lookup asks for the UID-named resource directly first, tries the UID filter for servers that honour it, and reads the collection to match client-side when the filter is refused. |
| **App-specific passwords only** — the Apple ID password is rejected, and there is no OAuth. | `auth` verifies credentials against the server before storing them, so a wrong password fails immediately with a clear message instead of a confusing discovery error. |
| **Eventual consistency** — a write is not always visible on the next read. | Writes return the event as written rather than re-reading it. |
| **Refuses to let the default calendar be set.** The `PROPPATCH` comes back `207` with a `403` inside, which looks like a success unless you read the propstats. | Propstat statuses are checked, so `setDefaultCalendar` reports the refusal instead of claiming to have worked. |

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
- **Property writes are checked per property, not per response.** A `PROPPATCH`
  answers `207` whichever properties it accepted, with the real verdict inside
  each `propstat` — so trusting the HTTP status turns a wholly-refused patch
  into a reported success. The statuses are read and a refusal names the
  property and the code.
- **The mutation surface is typed, not a `PROPPATCH` passthrough.** A generic
  property-setting mutation would be more powerful, but DAV property names and
  namespaces don't appear in the SDL, so a model composing a query can't
  discover them and a misnamed one is dropped without complaint. The generic
  primitive lives in the client, where the callers know the names.
- **One unreadable calendar doesn't sink the agenda** — it's logged and skipped.

## Development

```bash
cargo test          # 251 tests, including an end-to-end suite against a mock server
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

## License

MIT
