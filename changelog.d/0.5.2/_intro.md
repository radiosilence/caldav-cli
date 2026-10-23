### Changed

- **`mcp --browser` implies `--graphiql`** instead of refusing to run without
  it. Opening a browser at the IDE means serving the IDE, so making the user
  spell out both was arithmetic the tool could do itself — and it already infers
  the listener from any HTTP surface.
