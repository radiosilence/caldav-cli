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
