# No build stage. No package manager. CI builds the static musl binaries;
# this image only copies them in. The build context is dist/, holding
# dist/<arch>/caldav for each architecture (amd64, arm64), so `docker build`
# by hand needs those populated first and `-f Dockerfile dist`; docker builds
# only ever happen in CI.

FROM scratch

ARG TARGETARCH

# VALIDATED, DO NOT "SIMPLIFY" THIS AWAY:
# rustls-platform-verifier requires a system trust store on Linux. It does NOT
# fall back to the webpki roots compiled into the binary. With no CA bundle on
# disk, reqwest panics before making a single request:
#   Client::new(): reqwest::Error { kind: Builder,
#     source: General("No CA certificates were loaded from the system") }
# Verified by running the static binary on bare scratch against a real HTTPS
# host: without this line it panics; with it, TLS completes and the server's
# own auth response comes back. Sourced from distroless/static so we need no
# package manager and no build stage — it is a plain copy from a published,
# CVE-maintained image.
COPY --from=gcr.io/distroless/static:latest \
     /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

COPY ${TARGETARCH}/caldav /caldav

EXPOSE 8080

# scratch has no /etc/passwd, so this must be a raw numeric uid.
USER 10001:10001

LABEL org.opencontainers.image.title="caldav-cli"
LABEL org.opencontainers.image.vendor="James Cleveland"
LABEL org.opencontainers.image.licenses="MIT"
LABEL org.opencontainers.image.source="https://github.com/radiosilence/caldav-cli"

ENTRYPOINT ["/caldav"]
CMD ["mcp", "--http", "0.0.0.0:8080"]
