# caldav as a container. The default command runs the MCP server over HTTP —
# this is the image mcp-gateway proxies to as a backend (credentials per request
# via the X-CalDAV-* headers).
#
# This is an HTTP *client* (talks out to CalDAV servers over TLS), so unlike
# nano-web the CA bundle must be present at runtime for rustls-platform-verifier
# to find the system trust store.

# Selects which stage supplies the binary. Must be declared before the first
# FROM to be usable in one. `docker build .` compiles from source; CI passes
# prebuilt to reuse the binary the release matrix already built.
ARG BIN_SOURCE=source

# Source build.
FROM rust:1-slim AS builder

# musl-tools for the static target; cmake/clang for aws-lc-sys, the rustls
# crypto backend, which is a C/C++ build.
RUN apt-get update && apt-get install -y --no-install-recommends \
    musl-tools musl-dev cmake clang ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN rustup target add $(uname -m)-unknown-linux-musl

WORKDIR /build
COPY . .

RUN TARGET=$(uname -m)-unknown-linux-musl && \
    cargo build --release --locked --target $TARGET && \
    cp target/$TARGET/release/caldav /tmp/caldav

FROM scratch AS bin-source
COPY --from=builder /tmp/caldav /caldav

FROM scratch AS bin-prebuilt
ARG TARGETARCH
COPY dist/caldav-linux-${TARGETARCH}-musl /caldav

# Runtime stage. BuildKit only builds the stage this resolves to, so the
# source build is skipped entirely when BIN_SOURCE=prebuilt.
FROM bin-${BIN_SOURCE}

# rustls-platform-verifier reads the system trust store, so the bundle has to
# be in the image. Sourced from the builder rather than pinned separately, so
# it refreshes whenever the base image does.
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

EXPOSE 8080

LABEL org.opencontainers.image.vendor="James Cleveland"
LABEL org.opencontainers.image.licenses="MIT"
LABEL org.opencontainers.image.source="https://github.com/radiosilence/caldav-cli"

# scratch has no /etc/passwd, so USER must be a raw numeric uid.
USER 10001:10001

ENTRYPOINT ["/caldav"]
CMD ["mcp", "--http", "0.0.0.0:8080"]
