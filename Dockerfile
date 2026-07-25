# caldav as a container. The default command runs the MCP server over HTTP —
# this is the image mcp-gateway proxies to as a backend (credentials per request
# via the X-CalDAV-* headers). No native toolchain needed: unlike fastmail-cli
# there's no kreuzberg/pdfium here, so this is a plain Rust build.

FROM rust:1-bookworm AS build
WORKDIR /app
COPY . .
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/caldav /usr/local/bin/caldav
EXPOSE 8080
RUN useradd --system --uid 10001 --create-home app
USER app
ENTRYPOINT ["caldav"]
CMD ["mcp", "--http", "0.0.0.0:8080"]
