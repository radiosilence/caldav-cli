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
