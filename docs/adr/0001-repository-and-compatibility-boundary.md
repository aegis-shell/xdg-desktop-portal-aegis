# ADR-0001: Repository and compatibility boundary

- Status: Superseded by [ADR-0002](0002-resource-authority-and-file-chooser-process-boundary.md)
- Date: 2026-08-02

## Context

The Aegis portal backend has its own D-Bus ABI, PipeWire integration,
encrypted state, PAM helper, activation metadata, dependency graph, and
release lifecycle. It also consumes compositor-owned capabilities through a
private IPC protocol that evolves with Aegis.

The repository needs an ownership boundary that permits independent Portal
releases without weakening protocol compatibility or making its source
documentation depend on another repository's architectural history.

## Decision

Maintain `xdg-desktop-portal-aegis` in the independent
`aegis-shell/xdg-desktop-portal-aegis` repository with its own versions,
lockfile, CI, release artifacts, and ADR sequence.

The Portal repository owns:

- the D-Bus backend process and portal interface adapters;
- PipeWire stream publication;
- encrypted Secret storage and Secret Service compatibility;
- the optional PAM auto-unlock helper; and
- D-Bus activation and xdg-desktop-portal metadata.

The Aegis repository owns the compositor, IPC schema and grants, capture and
stream mechanisms, and compositor-hosted consent chrome. The versioned Rust
crate APIs and wire schema are the integration contract. Portal source
comments describe that contract directly and do not use external ADRs as
normative implementation documentation.

Portal and Aegis use independent version sequences. Every Portal release
pins one supported Aegis Git tag in `Cargo.toml`; the
[Compatibility Reference](../reference/compatibility.md) records the public
mapping. Portal `v0.0.1` pins Aegis `v0.0.9`.

The workspace separates components by dependency and trust boundary:

- `xdg-desktop-portal-aegis` composes the process and Aegis adapters;
- `aegis-portal-runtime` owns shared request lifecycle primitives;
- `aegis-portal-secret` owns encrypted state and both Secret APIs; and
- `aegis-pam` produces the optional login token.

Protocol-facing identifiers remain `aegis`, including the D-Bus backend
name, `aegis.portal`, `aegis-portals.conf`, and the `aegis-portal` IPC scope.
They are compatibility identifiers, not repository names.

## Alternatives

- **Use Aegis ADRs as Portal documentation.** Rejected because it makes this
  repository's implementation guidance depend on external historical paths
  and numbering.
- **Duplicate Aegis ADRs here.** Rejected because two copies would diverge
  and blur ownership. Each repository records only its own decisions.
- **Require equal version numbers.** Rejected because independent release
  lifecycles need explicit compatibility mapping, not accidental numerical
  equality.
- **Remove the exact Aegis dependency pin.** Rejected because the private IPC
  and compositor mechanisms can change incompatibly.

## Consequences

- Portal releases can advance independently while declaring one exact Aegis
  compatibility target.
- Cross-repository protocol changes require coordinated implementation and
  compatible releases in both repositories.
- Portal documentation remains self-contained; Aegis documentation can
  explain compositor-side history without becoming a Portal build contract.
- Distribution packaging must use the published compatibility mapping.
