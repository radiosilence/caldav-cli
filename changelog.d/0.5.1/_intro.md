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
