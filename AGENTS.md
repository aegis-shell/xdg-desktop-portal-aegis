# Repository Instructions

Before writing, modifying, or archiving documentation, read and follow
`docs/dev/documentation/index.md`. AI assistants may read
`docs/dev/documentation/` and suggest changes, but must not modify files in
that directory.

Do not bypass Git hooks.

Keep the Portal source and build graph independent from the Aegis repository:

- Do not add Aegis internal crates, Aegis Git dependencies, or sibling-path
  patches.
- Put compositor integration in the Portal-owned `aegis-portal-ipc` wire
  projection and keep it limited to compositor-owned resources.
- Test wire changes with literal protocol fixtures and the independent test
  server; do not import the compositor's server implementation into tests.
