# Aegis IPC Wire Protocol

The Portal-owned `aegis-portal-ipc` crate projects Aegis IPC for
compositor-owned portal resources only: settings, capture, picking,
streams, and wallpaper. This page summarizes the wire contract for lookup;
the field-level truth is `crates/aegis-portal-ipc/src/schema.rs`, pinned
by literal JSON fixtures and exercised against the independent test server
in `crates/aegis-portal-ipc/src/testing.rs`. Decision history lives in
[ADR-0004](../adr/0004-portal-ownership-and-runtime-ipc-boundary.md),
[ADR-0005](../adr/0005-screencast-dmabuf-slot-protocol.md),
[ADR-0006](../adr/0006-shm-consumers-switch-to-readback-transport.md),
[ADR-0007](../adr/0007-full-stack-interface-ownership.md), and
[ADR-0011](../adr/0011-wallpaper-wire-reconciliation.md); release mapping
lives in the [Compatibility Reference](compatibility.md).

## Protocol Versions

| Version | Adds | Released Aegis implementing it |
|---------|------|--------------------------------|
| 24 | Base op set below | `v0.0.11`–`v0.0.14` |
| 25 | Zero-copy dmabuf slot streams | `v0.0.15` |
| 26–27 | `CaptureWindow` (26); `LaunchApp`, `Focus.reveal` (27) — upstream, deliberately not projected | `v0.0.16`–`v0.0.21` (27) |

The handshake asks for protocol 25 (`PROTOCOL_VERSION`) and accepts a
downgrade to 24 (`MIN_PROTOCOL_VERSION`); version-gated features key off
the negotiated version. Protocols 26 and 27 exist upstream but are
deliberately not projected: no Portal interface needs them (ADR-0011).
The connection scope is `aegis-portal`.

## Operations

| Operation | Since | Purpose |
|-----------|-------|---------|
| `Hello` | 24 | Version negotiation, capabilities, scope, optional lease request |
| `GetSettings` / `Subscribe` | 24 | Desktop-preferences snapshot; `SettingsChanged` events |
| `RenewLease` | 24 | Renew the connection lease (`LeaseGrant`) |
| `CaptureOutput` | 24 | Full-output or region capture; PNG payload returned as one sealed memfd |
| `PickTarget` | 24 | Region, pixel, or window picking |
| `PickConfirm` | 24 | Capture-consent confirmation |
| `StreamOutputStart` / `StreamOutputStop` | 24 | Output (or window) frame stream; the `dmabuf` flag opts into the protocol-25 slot stream |
| `StreamBufferRelease` | 25 | Return a dmabuf slot after the PipeWire consumer released it |
| `SetWallpaper` | 17 (before the floor) | Apply the wallpaper image at a path the compositor decodes itself; answered by `WallpaperSet` (the authoritative decode-and-swap receipt) or `Error` |

Stream events are `StreamFrame` (at 25, carrying a slot index instead of a
blob) and `StreamEnded`.

## Transport Invariants

- `CaptureOutput`'s PNG payload travels on the blob channel as one sealed
  memfd. `SetWallpaper` carries no payload: it names a bounded
  (≤4096 bytes), absolute, lexically normalized path, and the client
  mirrors the compositor's rule so a request it would reject never
  crosses the socket. Compositor-side the op is gated on the `control`
  capability, a live lease, an explicit `SetWallpaper` op in the
  connection's scope, and an unlocked session.
- The copy path never memory-maps a non-`DRM_FORMAT_MOD_LINEAR`
  descriptor; frames that would need it are dropped rather than delivered
  scrambled (ADR-0006).
- At protocol 24 a dmabuf frame's descriptor follows its header and is
  mmap-copied once; at protocol 25 the compositor transfers the fixed slot
  table once, in slot order, after `StreamOutputStarted`, and frames then
  reference slots by index with no blob attached.
- Unknown fields inside known responses are ignored; unknown response
  variants and protocol-version mismatches fail closed.

## Source of Truth

- `crates/aegis-portal-ipc/src/schema.rs` — wire types and the
  version-gating constants; its unit tests pin literal v24/v25 fixtures.
  The wallpaper fixtures were derived by serializing the compositor's own
  schema types and are pinned in both directions
  (`{"type":"SetWallpaper","path":...}` ↔ `{"type":"WallpaperSet"}`).
- `crates/aegis-portal-ipc/tests/` — stream-frame and wallpaper
  conformance tests against the independent server, whose wallpaper
  dispatch mirrors the real gates (control, live lease, explicit scope
  op, valid path, active session).
- `crates/aegis-portal-ipc/src/testing.rs` — the minimal independent test
  server; tests never import the compositor's implementation.
