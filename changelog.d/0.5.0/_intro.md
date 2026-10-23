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
