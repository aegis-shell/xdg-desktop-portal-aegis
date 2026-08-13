# ADR-0011: Wallpaper wire reconciliation and the protocol-25 baseline

- Status: Accepted
- Date: 2026-08-13
- Supersedes: the wallpaper design point of
  [ADR-0007](0007-full-stack-interface-ownership.md)

## Context

ADR-0007 made Wallpaper a native interface on the strength of a
protocol-26 `SetWallpaper` operation — sealed-memfd image transport, a
placement field, a `WallpaperApplied` reply — that this repository
designed itself. That op was projected *ahead of the compositor*,
violating the repository's own coordination rule ("land and tag the
compositor side before releasing a Portal version that requires it"),
and it shipped in no Aegis release: against every real compositor the
unknown request failed closed, so the Wallpaper portal worked against no
real system at all.

The compositor's actual operation has existed since protocol 17:
`SetWallpaper { path }` answered by `WallpaperSet`. The compositor gates
it on the `control` capability, a live lease, an explicit `SetWallpaper`
op in the connection's scope (the `aegis-portal` scope has it), a bounded
(≤4096 bytes), absolute, lexically normalized path, and an unlocked
session; it decodes the image on its main loop and the reply is the
authoritative decode-and-swap receipt. There is no placement field
compositor-side.

The same audit corrected the protocol↔release mapping: Aegis
`v0.0.11`–`v0.0.14` speak protocol 24, `v0.0.15` speaks 25, and
`v0.0.16`–`v0.0.21` speak 27. The compatibility table's
"`v0.0.15`–`v0.0.18` ↔ protocol 25" annotation was wrong for `v0.0.16`
and newer.

## Decision

1. The `aegis-portal-ipc` projection speaks the compositor's real
   path-based `SetWallpaper`. The literal fixtures were derived from the
   real schema by serializing the compositor's own types, and are pinned
   in both directions (`{"type":"SetWallpaper","path":...}` ↔
   `{"type":"WallpaperSet"}`). The client mirrors the compositor's path
   rule so a request it would reject never crosses the socket, and the
   independent test server mirrors the real dispatch gates in their real
   order (control, live lease, explicit scope op, valid path, active
   session).
2. The projection is re-baselined to `PROTOCOL_VERSION = 25`
   (negotiating down to 24): the dmabuf slot stream is the newest
   projected feature. Upstream protocol 26 (`CaptureWindow`) and 27
   (`LaunchApp`, `Focus.reveal`) are deliberately not projected — no
   Portal interface needs them. The wallpaper op needs no version gate:
   the compositor has spoken it since protocol 17, before this
   projection's floor.
3. The daemon stages the image at
   `$XDG_RUNTIME_DIR/aegis-portal/wallpaper/current.<ext>` — directory
   0700, file 0600, atomic replace, a 64 MiB staging cap — and keeps the
   file after a successful reply, because the compositor may keep
   streaming a video wallpaper from the path; the staging directory is
   wiped at daemon startup. `set-on` is still validated (an unknown value
   answers response 2) but is no longer forwarded: the compositor has a
   single wallpaper concept.
4. The compatibility reference adopts the audited per-tag mapping above.

## Alternatives

- **Land the sealed-memfd op in the compositor first, then keep it.**
  Rejected: the compositor already owns a simpler path-based op that
  covers the portal's need and works against every released compositor
  today; adding a second wallpaper op duplicates wire surface and
  authority for no new capability.
- **Drop native Wallpaper until a memfd op ships.** Rejected: it removes
  a routed interface the full-stack boundary (ADR-0007) commits to
  serving, when the long-standing compositor op already serves it.
- **Keep forwarding `set-on`.** Rejected: the wire op carries no
  placement and the compositor has a single wallpaper concept;
  silently mapping the value would lie to applications about where the
  wallpaper landed. Validation stays so misspellings still fail.

## Consequences

- Wallpaper application works against every supported Aegis release
  instead of none; no compositor release or upgrade is required.
- `set-on` degrades to advisory-only: validated, never forwarded.
- The staged image is a session-lifetime, portal-owned artifact in the
  runtime directory; its lifetime is one staged file kept per successful
  swap, with stale staging wiped at startup.
- The independent test server now mirrors the real dispatch gates, so a
  scope-, lease-, or path-gate regression fails in this repository's
  tests before it reaches a compositor.
- Projecting upstream protocols 26/27 is deferred until a Portal
  interface needs them — and then lands compositor-side first, per the
  strengthened coordination rule in
  [Cross-Repository Protocol Development](../dev/cross-repository-development.md).
