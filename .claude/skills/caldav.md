---
name: caldav
description: Complete reference for caldav-cli — all commands, flags, config, and common patterns
---

# caldav-cli — Complete Reference

caldav-cli is a Rust CLI for CalDAV calendars (iCloud by default). All output is
JSON: `{"success": true, "data": {...}}`.

## Setup

```bash
caldav-cli auth --username you@icloud.com          # password read from stdin
caldav-cli auth --username you@fastmail.com --server-url https://caldav.fastmail.com
```

CalDAV needs an **app-specific password**, not the account password:

- iCloud — appleid.apple.com → Sign-In and Security → App-Specific Passwords
- Fastmail — Settings → Privacy & Security → App passwords

Config lives at `~/.config/caldav-cli/config.toml`:

```toml
[core]
server_url = "https://caldav.icloud.com"
username = "you@icloud.com"
app_password = "abcd-efgh-ijkl-mnop"
```

Or via env: `CALDAV_SERVER_URL`, `CALDAV_USERNAME`, `CALDAV_APP_PASSWORD`

Debug: `RUST_LOG=debug caldav-cli [cmd]`

---

## Command Reference

### Calendars

```bash
caldav-cli calendars           # ids, names, colours, readOnly, supportsEvents
```

Use the `id` (or the display name) anywhere `-c/--calendar` is accepted.

### Reading

```bash
caldav-cli agenda [--days N] [--tz TZ] [-c CAL] [-l LIMIT]    # default: today
caldav-cli list [-c CAL] [--start S] [--end E] [--days N] [--tz TZ] [-l N] [--no-expand]
caldav-cli get EVENT_UID [-c CAL]
caldav-cli search QUERY [-c CAL] [--start S] [--end E] [--days N] [-l N]
caldav-cli free-busy [--start S] [--end E] [--days N] [--tz TZ]
```

Window defaults: start = today, span = 7 days (`agenda` defaults to 1 day).
`--end` beats `--days`.

`search` only looks inside the window — widen `--days` to search further ahead
(e.g. `--days 365` for "when is my next dentist appointment").

### Writing

```bash
caldav-cli create [-c CAL] --summary "Coffee" --start "tomorrow 15:00" --duration 30 --tz Europe/London
caldav-cli update EVENT_UID --start "+1d" --location "Room 5"
caldav-cli delete EVENT_UID -y
```

Shared event flags:

```
--summary --start --end --duration MINUTES --all-day --tz
--description --location --url --status (CONFIRMED|TENTATIVE|CANCELLED)
--recurrence 'FREQ=WEEKLY;BYDAY=MO'
--attendee 'Jane Doe <jane@x.test>'   (repeatable)
--category work                        (repeatable)
```

`--end` and `--duration` are mutually exclusive. With neither, a new event
lasts 1 hour (1 day if all-day).

On `update`, only the flags you pass change. Omitting `--attendee`/`--category`
keeps the existing lists. Moving only `--start` preserves the stored duration.

`delete` requires `-y`.

---

## Time formats

Accepted anywhere a time is:

| Form                | Example                                             |
| ------------------- | --------------------------------------------------- |
| ISO 8601            | `2026-07-24T09:00:00Z`, `2026-07-24T09:00:00+01:00` |
| Date + time         | `2026-07-24 09:00`, `2026-07-24T09:00`              |
| Bare date (all-day) | `2026-07-24`, `24/07/2026`                          |
| Keyword             | `now`, `today`, `tomorrow`, `yesterday`             |
| Relative            | `+90m`, `-2h`, `+3d`, `+1w`                         |

Values without an offset are read in `--tz` (UTC when absent). **Pass `--tz`
whenever the user means a local wall-clock time** — otherwise "9am" becomes
9am UTC. It also decides where "today" starts.

---

## Recurring events

`agenda` and `list` return one result per occurrence, each with a
`recurrenceId`. All occurrences share the series `id` (the UID). Expansion is
asked of the server and redone client-side when the server won't do it (iCloud
often won't) — the output shape is the same either way, and occurrences keep
their local wall-clock time across DST.

To edit a series, fetch the master first:

```bash
caldav-cli list --no-expand -c Home --days 30     # master events, with rrule
caldav-cli update SERIES_UID --start "2026-08-01 10:00"
```

Editing an expanded occurrence's UID edits the whole series.

---

## Common patterns

```bash
# What's on today, in local time
caldav-cli agenda --tz Europe/London

# The week ahead, work calendar only
caldav-cli list -c Work --days 7 --tz Europe/London

# Find a free slot before proposing a meeting
caldav-cli free-busy --days 3 --tz Europe/London

# Next occurrence of something, searching a year out
caldav-cli search dentist --days 365

# Book something and check it landed
caldav-cli create --summary "1:1" --start "tomorrow 14:00" --duration 45 \
  --tz Europe/London --attendee 'Jane Doe <jane@x.test>'
caldav-cli agenda --days 2 --tz Europe/London
```

---

## MCP mode

```bash
caldav-cli mcp                        # stdio, credentials from config
caldav-cli mcp --http 0.0.0.0:8080    # hosted, credentials per request
```

One tool: `calendar` (run a query or mutation). Its description carries the
full GraphQL SDL, so there is nothing to introspect first.

In HTTP mode credentials come from `X-CalDAV-Username`, `X-CalDAV-Password`,
and optionally `X-CalDAV-Url` — injected by a trusted upstream, never by the
client. Both username and password must be present for the headers to be used.

**All mutations are two-phase.** `action: PREVIEW` returns a description of the
change plus a one-shot `confirmationToken`; `action: CONFIRM` with that token
applies it. Show the user the preview before confirming — always.

---

## Failure modes worth recognising

| Message                        | Cause                                                |
| ------------------------------ | ---------------------------------------------------- |
| `Invalid credentials`          | Wrong username or a non-app-specific password. iCloud has no OAuth — an app-specific password from appleid.apple.com is required |
| `CalDAV discovery failed`      | Wrong server URL, or the host isn't a CalDAV server   |
| `calendar '...' is read-only`  | Shared/subscribed calendar; pick a writable one       |
| `changed on the server`        | Someone else edited it — re-read the event and retry  |
| `unknown timezone id`          | `--tz` needs an IANA name (`Europe/London`, not `BST`)|
