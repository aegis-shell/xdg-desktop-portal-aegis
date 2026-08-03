# Portal and Aegis Cross-Repository Development

Use one long-lived linked Git worktree for Portal and Aegis development. The
primary Portal worktree remains on `main` with the canonical remote dependency
graph. The development worktree normally remains on the long-lived local `dev`
branch and resolves the live sibling Aegis sources through Cargo `[patch]`.

This mirrors the Aegis↔Optics workflow documented in the Aegis repository; the
Portal plays the downstream role here, depending on `aegis-authority`,
`aegis-core`, `aegis-ipc`, and `aegis-logging` instead of on Optics bindings.

## Dependency Modes

| Concern | Primary worktree | Portal development worktree |
|---------|------------------|-----------------------------|
| Aegis crates | Tagged Aegis Git source | Sibling `../aegis` paths through `[patch]` |
| `Cargo.lock` | Canonical and committed | Local resolution, never committed |
| Cargo configuration | `.cargo/config.toml` absent | Generated from `.cargo/aegis-local.toml` |
| Build cache | Primary `target/` | Worktree-local `target/` |

Do not set a shared `CARGO_TARGET_DIR` for these worktrees. Separate target
directories prevent the canonical and patched dependency graphs from reusing
the same incremental artifacts.

## Create the Development Worktree

Create the linked worktree once from the primary Portal worktree:

```bash
git worktree add -b dev ../xdg-desktop-portal-aegis-dev main
```

The expected directory layout is:

```text
projects/
├── aegis/
├── xdg-desktop-portal-aegis/
└── xdg-desktop-portal-aegis-dev/
```

The primary `xdg-desktop-portal-aegis/` worktree keeps `main` checked out. The
`xdg-desktop-portal-aegis-dev/` worktree permanently keeps the local `dev`
branch checked out.

Enter the development worktree, install the local patch configuration, and
enable the repository hooks once:

```bash
cd ../xdg-desktop-portal-aegis-dev
cp .cargo/aegis-local.toml .cargo/config.toml
git config core.hooksPath .githooks
```

These commands:

1. Copy the reviewed local `[patch]` template to the ignored
   `.cargo/config.toml`.
2. Enable the repository-owned pre-commit hook.

Do not create `.cargo/config.toml` in the primary Portal worktree. Confirm that
the linked worktree has an Aegis sibling before continuing:

```bash
test -f ../aegis/Cargo.toml
```

### Keep the sibling Aegis checkout at the pinned version

The Portal workspace pins one exact Aegis version (`aegis-authority`/
`aegis-core`/`aegis-ipc`/`aegis-logging`, currently `=0.0.11`). The `[patch]`
only takes effect when the sibling `../aegis` checkout is at that same
version — Cargo rejects a path patch whose version does not satisfy the
`=x.y.z` requirement and silently falls back to the tagged Git source.

So before resolving the worktree-local graph, put the sibling Aegis checkout at
the pinned tag:

```bash
cd ../aegis
git switch main
git checkout vX.Y.Z          # the tag Portal pins in Cargo.toml
```

The primary Aegis checkout serving the Portal plays the *release baseline*
role; day-to-day Aegis development happens in its own `aegis-dev` worktree and
does not disturb this checkout. See the
[Compatibility Reference](../reference/compatibility.md) for the authoritative
Portal↔Aegis tag pairing.

Resolve the worktree-local Cargo graph:

```bash
cd ../xdg-desktop-portal-aegis-dev
cargo check --workspace
```

The first Cargo command intentionally omits `--locked` because `[patch]`
changes the local lockfile from Git package identities to path package
identities. After that resolution succeeds, ordinary commands may use
`--locked` until an Aegis manifest changes.

Verify the selected source:

```bash
cargo tree -i aegis-core
cargo tree -i aegis-ipc
cargo tree -i aegis-logging
cargo tree -i aegis-authority
```

All four trees must show paths below the sibling `aegis` checkout (not the
`https://github.com/ming2k/aegis` Git source).

## Daily Development

Resolve and build the Portal against the sibling Aegis sources:

```bash
cargo check --locked --workspace
cargo test --locked --workspace
```

If an Aegis crate version or dependency changes, resolve once without
`--locked`:

```bash
cargo check --workspace
```

Keep the sibling Aegis checkout at the Portal-pinned tag while local mode is
active. To exercise an unreleased Aegis change here instead, point the patch
at the Aegis development worktree and reconcile the version pin first (see
[Coordinate with an unreleased Aegis](#coordinate-with-an-unreleased-aegis)
below).

## Commit Portal Changes

Stage changes normally:

```bash
git add .
git commit -m "feat: ..."
```

While `.cargo/config.toml` contains the local Aegis patch, the tracked
pre-commit hook automatically unstages:

- `Cargo.lock`; and
- `.cargo/config.toml`, if it was force-added.

The hook prints the excluded paths and lets the remaining commit proceed.
Do not use `--no-verify`. Local patch state is not part of a Portal commit.

Commits created on `dev` already belong to the shared Git repository. No
file-copy, push, or pull step is required for a local merge. Uncommitted files
in the development worktree do not enter `main`; Git merges commits, not
worktree state.

## Synchronize with Portal Main

Update `main` in the primary worktree first. This may be a local merge or a
pull when remote changes exist:

```bash
cd ../xdg-desktop-portal-aegis
git switch main
git pull --ff-only
```

Skip the pull when `main` is already current locally. Then restore the
disposable local lockfile and rebase the development branch onto the shared
local `main` branch:

```bash
cd ../xdg-desktop-portal-aegis-dev
git restore Cargo.lock
git rebase main
cargo check --workspace
```

The ignored `.cargo/config.toml` remains in the linked worktree across the
rebase.

## Coordinate with an unreleased Aegis

Some Portal work may be coordinated with an unreleased Aegis IPC protocol.
That Aegis code lives in the Aegis `aegis-dev` worktree, ahead of the tag
Portal currently pins. The current FileChooser process boundary is not such
a change: it uses no private Aegis file-picking operation. Two clean options
exist when a future change does require unreleased IPC:

1. **Pin-baseline mode (default).** Develop the Portal against the released
   Aegis tag in `../aegis`. Land the Aegis protocol change, cut and tag the
   Aegis release, then advance the Portal pin (see
   [Promote an Aegis Release](#promote-an-aegis-release)). This keeps every
   committed Portal state buildable against a published Aegis.

2. **Chase mode (temporary).** Point the local patch at the Aegis development
   worktree and reconcile the version so the patch is actually used:

   ```bash
   # In .cargo/config.toml, change the four paths from ../aegis to ../aegis-dev,
   # and widen the version requirement in Cargo.toml for the joint build, e.g.
   #   aegis-ipc = { path = "../aegis-dev/crates/aegis-ipc" }   # via [patch]
   # Never commit the widened requirement; restore the exact pin before commit.
   ```

   The pre-commit hook still excludes `Cargo.lock`, but the version change in
   `Cargo.toml` is *not* automatically excluded — restore the exact `=x.y.z`
   pin and `git restore Cargo.toml` before committing so the canonical pin is
   never weakened. Prefer option 1 whenever the change can land behind the
   released tag.

## Promote an Aegis Release

Land and tag Aegis before advancing the Portal pin. Use one immutable tag for
every Aegis crate the Portal depends on.

Confirm that the sibling checkout is at the release tag:

```bash
test "$(git -C ../aegis rev-parse HEAD)" = \
  "$(git -C ../aegis rev-list -n 1 vX.Y.Z)"
```

Disable local mode and restore the canonical Portal lockfile:

```bash
mv .cargo/config.toml /tmp/portal-aegis-local.toml
git restore Cargo.lock
```

Update the `aegis-authority`, `aegis-core`, `aegis-ipc`, and `aegis-logging`
dependencies in the workspace `Cargo.toml` to the new `=X.Y.Z` / tag
`vX.Y.Z`, and update the
[Compatibility Reference](../reference/compatibility.md) table.

Resolve and validate the remote graph:

```bash
cargo check --workspace
cargo check --locked --workspace
cargo test --locked --workspace
cargo build --locked --release --workspace
```

Confirm that `cargo tree -i aegis-ipc` now reports the tagged Git source.
Review `Cargo.lock`, then commit the canonical dependency update:

```bash
git add .
git commit -m "build: adopt Aegis vX.Y.Z"
```

The local patch configuration is absent at this point, so the hook permits
the canonical `Cargo.lock` update.

## Merge Locally and Reuse the Worktree

Fast-forward `main` to the completed `dev` commits from the primary worktree:

```bash
cd ../xdg-desktop-portal-aegis
git switch main
git merge --ff-only dev
```

This operation uses commits already stored in the shared local repository. It
does not contact a remote server.

Both branches now point at the same commit. Continue with the next change in
the existing development worktree:

```bash
cd ../xdg-desktop-portal-aegis-dev
git restore Cargo.lock
cp .cargo/aegis-local.toml .cargo/config.toml
cargo check --workspace
```

The copy command is harmless when local mode remained enabled. It also
restores local mode when the previous change removed `.cargo/config.toml`
during Aegis release promotion.

## Optional Pull Request Workflow

Use a remote pull request only when CI, review, backup, or collaboration
requires it. Start that change on a temporary feature branch from canonical
`main` instead of advancing `dev`:

```bash
cd ../xdg-desktop-portal-aegis-dev
git restore Cargo.lock
git switch -c feat/<topic> main
cargo check --workspace

# After committing the reviewed change:
git push -u origin feat/<topic>

# After the remote pull request is merged:
cd ../xdg-desktop-portal-aegis
git switch main
git pull --ff-only

cd ../xdg-desktop-portal-aegis-dev
git restore Cargo.lock
git switch dev
git merge --ff-only main
cp .cargo/aegis-local.toml .cargo/config.toml
cargo check --workspace
```

The development worktree remains in place and returns to `dev` after the pull
request. Delete the temporary branch when it is no longer needed.

## Remove the Development Worktree

Remove the worktree only when cross-repository development is no longer
needed:

```bash
cd ../xdg-desktop-portal-aegis-dev
mv .cargo/config.toml /tmp/portal-aegis-local.toml
git restore Cargo.lock

cd ../xdg-desktop-portal-aegis
git worktree remove ../xdg-desktop-portal-aegis-dev
```

## Test an Unreleased Aegis Commit in CI

When Portal CI must run before an Aegis tag exists, temporarily point the
Aegis Git dependencies at the same fixed `rev`. Do not use a moving branch.
Replace the fixed revision with the final release tag before merging Portal.

## Recover the Canonical Mode

If a local patch was enabled in the wrong worktree, move it out of the way
and restore the committed lockfile:

```bash
mv .cargo/config.toml /tmp/portal-aegis-local.toml
git restore Cargo.lock
cargo check --locked --workspace
```

The final command verifies the tagged remote dependency graph.
