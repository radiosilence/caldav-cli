### Changed

- **The `mcp` flags state their own implications, and resolve them in one
  place.** `--browser` implies `--graphiql` implies `--graphql`; `--http` is the
  only flag that puts MCP on the listener, which `--help` now says outright —
  the surface a model connects through and one you can poke at in a browser are
  different things, and confusing them exposes an endpoint nobody asked for.
  `HttpSurfaces` now describes what is mounted rather than what was typed, so
  `graphql` is no longer false while `/graphql` is being served. Same routes as
  before; tests pin each documented invocation. Matches `fastmail-cli`,
  `tfl-mcp` and `mainlynorfolk-mcp`, which take the same three flags.
