### Changed

- **Linux release binaries now come from the image build (`build-image`),
  not a separate `x86_64-unknown-linux-gnu` compile in `build-binaries`.**
  The two jobs were compiling linux amd64 twice per push, and the musl
  binary built for the image was uploaded as a raw artifact that
  `create-release` never picked up (it only attaches `**/*.tar.gz`), so
  those musl builds were compiled and then discarded. `build-image` now
  packages its musl binary into the same tarball format as the other
  release assets, and `create-release` waits on `build-image` so those
  tarballs exist before a release is cut.
- **Linux arm64 gets a release binary for the first time**, packaged from
  the arm64 leg of `build-image` the same way as amd64.
- The `linux-x86_64` release asset keeps its filename but is now the
  statically linked musl binary instead of a glibc build — strictly more
  portable, and the same binary the published container images already run.
