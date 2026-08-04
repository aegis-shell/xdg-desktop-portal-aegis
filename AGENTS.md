# Repository Instructions

Before writing, modifying, or archiving documentation, read and follow
`docs/dev/documentation/index.md`. AI assistants may read
`docs/dev/documentation/` and suggest changes, but must not modify files in
that directory.

Do not bypass Git hooks.

Local Aegis mode is active when `.cargo/config.toml` contains
`[patch."https://github.com/aegis-shell/aegis"]`. In that mode:

- Treat `.cargo/config.toml` and the path-resolved `Cargo.lock` as local
  worktree state. They must not be staged or committed.
- Do not use `--no-verify`, force-add either file, or otherwise defeat the
  pre-commit hook.
- Update the canonical committed lockfile only after disabling local Aegis
  mode and resolving the tagged Aegis dependencies.
