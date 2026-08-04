# Cross-Repository Protocol Development

The Portal and Aegis repositories have independent source trees, dependency
graphs, lockfiles, versions, and release lifecycles. They integrate at runtime
through the narrow Aegis IPC contract described in
[ADR-0004](../adr/0004-portal-ownership-and-runtime-ipc-boundary.md).

## Dependency Boundary

| Concern | Portal ownership | Aegis ownership |
|---------|------------------|-----------------|
| Public portal ABI | D-Bus adapters, request lifecycle, result encoding | None |
| Portal UI | FileChooser, Account confirmation, Secret password input | Window parenting through standard Wayland protocols |
| Runtime wire client | Protocol-24 projection and sealed-memfd receiver | Protocol server and authorization |
| Compositor resources | Validation, persistence, PipeWire publication | Settings, pixels, target selection, capture consent, frame streams |
| Source dependencies | Portal workspace crates and registry packages | No Portal build dependency |

Do not add Aegis internal crates, Git dependencies, or sibling path patches
to this repository. A local Aegis checkout is optional and never changes
Portal dependency resolution.

## Daily Development

Run the canonical dependency graph in every worktree:

```bash
cargo check --locked --workspace
cargo test --locked --workspace
```

`Cargo.lock` is committed and authoritative. A linked Git worktree may still
be useful for ordinary branch isolation, but it does not need a particular
directory name or adjacent Aegis checkout.

## Compatible Aegis Changes

An internal Aegis refactor requires no Portal change when all serialized
protocol-24 requests, responses, events, blob framing, scope behavior, and
authorization remain compatible. Validate the Portal independently:

```bash
cargo test --locked -p aegis-portal-ipc --features test-server
cargo test --locked -p xdg-desktop-portal-aegis --test media
```

The tests use literal wire fixtures and a minimal server owned by this
repository. They do not import Aegis model, authority, client, or server code.
This separation prevents a matching bug in a shared implementation from
making both sides pass.

Before declaring a new Aegis release compatible, compare its advertised IPC
version and run the Portal against that released compositor. Add the verified
release to the
[Compatibility Reference](../reference/compatibility.md). Do not infer wire
compatibility from Aegis package versioning alone.

## Incompatible Wire Changes

Coordinate a wire change in this order:

1. Define the smallest compositor-owned operation and reject Portal-owned
   filesystem, account, secret, email, or policy state at the boundary.
2. Change the Aegis protocol version when an existing version cannot decode
   or preserve the new semantics safely.
3. Update `aegis-portal-ipc` with only the required projection.
4. Add literal request, response, event, and blob fixtures before using the
   new operation in a D-Bus adapter.
5. Extend the independent test server and run daemon-level tests.
6. Test against a tagged Aegis release that implements the same protocol.
7. Update the compatibility reference and changelog in both repositories.

Land and tag the compositor side before releasing a Portal version that
requires it. A temporary development branch may coordinate both changes, but
the Portal branch must remain buildable without fetching or locating the
Aegis source tree.

## Release Validation

Run the complete Portal graph from a clean checkout:

```bash
cargo check --locked --workspace
cargo test --locked --workspace
cargo build --locked --release --workspace
cargo tree --workspace
```

The dependency tree and `Cargo.lock` must contain no Aegis repository source
or internal Aegis crate. Then run the Meson packaging checks and the real
runtime compatibility tests listed in the
[Release Checklist](release-checklist.md).
