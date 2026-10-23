### Added

- **A browsable GraphQL endpoint.** `--graphql` serves plain GraphQL-over-HTTP
  at `/graphql`, `--graphiql` adds the GraphiQL IDE at `/`, and `--browser`
  opens it once the port is bound. Each surface is independent of `--http`,
  which serves MCP's own transport at `/mcp`: the endpoint a model connects
  through and one you can poke at in a browser are different things that share
  a port. They also share the schema and the client cache, so the IDE sees
  exactly what a model sees. `/mcp` speaks JSON-RPC, which a browser doesn't,
  which is why the IDE needs its own route.
- **Introspection needs no credentials.** It is answered from the schema
  without touching CalDAV, so GraphiQL's docs, autocomplete and explorer work
  before you have a working app password. Anything selecting a real field
  authenticates as normal.
- **`--http` takes an optional address**, defaulting to `127.0.0.1:8080`.

### Changed

- **HTTP mode falls back to the local credentials** when a request carries no
  `X-CalDAV-*` headers, so running it yourself needs no ceremony. A hosted
  deployment ships no config, so the fallback is absent there and every request
  must still carry its own headers. Do not run it with local credentials on a
  non-loopback address: anything that can reach the port gets your calendar.
