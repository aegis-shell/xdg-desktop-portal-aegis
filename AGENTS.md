# Repository Instructions

Before writing, modifying, or archiving documentation, read and follow
`docs/dev/documentation/index.md`. AI assistants may read
`docs/dev/documentation/` and suggest changes, but must not modify files in
that directory.

Do not bypass Git hooks. Enable them once per clone with
`scripts/setup-dev.sh` (idempotent; sets `core.hooksPath` to
`.githooks`).

Keep the Portal source and build graph independent from the Aegis repository:

- Do not add Aegis internal crates, Aegis Git dependencies, or sibling-path
  patches.
- Put compositor integration in the Portal-owned `aegis-portal-ipc` wire
  projection and keep it limited to compositor-owned resources.
- Test wire changes with literal protocol fixtures and the independent test
  server; do not import the compositor's server implementation into tests.

The prompter UI builds on the optics stack (iris/lens), resolved from the
tagged `ming2k/optics` release — an independent third repository, so the
rules above do not cover it. For joint development against a sibling optics
checkout:

- Enable local mode with `cp .cargo/optics-local.toml .cargo/config.toml`.
  The generated `.cargo/config.toml` is Git-ignored; keep it that way.
- Leave `Cargo.lock` in the state the local patch produces while local mode
  is active; do not commit the path-resolved lockfile.
- Promote an Optics release by bumping every tagged dependency in
  `Cargo.toml` together and regenerating the canonical lockfile; keep
  `scripts/optics-release-ref.sh`'s expected package count in sync.
- `aegis-portal-prompter/build.rs` re-emits the `-sys` crates' rpath
  metadata so the binary finds the chosen liblens/libflux/libiris at
  runtime; the direct `flux-sys`/`iris-sys`/`lens-sys` dependencies exist
  only to make that metadata visible — do not prune them.
