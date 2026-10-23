### Changed

- **The image is now a single-stage, package-manager-free copy of a static
  musl binary onto `scratch`, not a compile on `debian:bookworm-slim`.**
  20.1MB down from a debian base. There is no build stage in the Dockerfile at
  all — CI compiles the binary once per arch and `docker build` only ever
  copies it in, so `docker build .` by hand now requires `dist/` to already be
  populated (docker builds only ever happen in CI).
- **The CA bundle is a plain `COPY --from=gcr.io/distroless/static`, not
  `apt-get install ca-certificates`.** Verified empirically on this binary:
  on bare `scratch` with no cert file, `Client::new()` panics with `"No CA
  certificates were loaded from the system"` — `rustls-platform-verifier`
  requires a system trust store and does not fall back to compiled-in webpki
  roots. With the bundle copied in, `caldav calendars` against
  `caldav.fastmail.com` completed TLS and got back the server's own auth
  rejection. Sourcing it from distroless/static avoids needing a package
  manager anywhere in the image build.
- **CI now builds and lints on every PR, not just on push to `main`.** The
  registry push and GitHub release stay gated to `main`, but a broken
  Dockerfile or a clippy/fmt regression now fails before merge. `check` is
  split into separate `test` / `lint` / `format` jobs so a formatting nit
  doesn't block the test job's cache warm-up.
- **Docker layer caching removed from the image build.** Nothing compiles
  inside the image anymore, so there was nothing left for `cache-from`/
  `cache-to` to usefully cache — `mode=max` was filling the repo-wide 10GB
  GitHub Actions cache that `Swatinem/rust-cache` shares with the Rust build
  jobs.
- The container is deliberately bare: `USER 10001:10001`, no `HOME`, no
  writable volume, no config directory. `scratch` has no `/etc/passwd`, so
  `~/.config/caldav-cli/config.toml` (which the CLI reads on other platforms)
  is unreachable in the container; this is fine since the image is normally
  driven entirely by `CALDAV_SERVER_URL` / `CALDAV_USERNAME` /
  `CALDAV_APP_PASSWORD`, and `Config::load()` degrades to defaults rather than
  erroring when the config directory can't be resolved. Verified `caldav
  --version` and `caldav --help` both exit 0 in the container with no config
  file and no `HOME` set. Credential handling in the container is moving to
  request headers entirely in a follow-up, so no HOME/config scaffolding was
  added.
