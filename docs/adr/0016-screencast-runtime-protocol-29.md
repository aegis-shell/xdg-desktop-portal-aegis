# ADR-0016: The ScreenCast runtime surface for protocol 29

- Status: Accepted
- Date: 2026-08-16

## Context

[ADR-0015](0015-protocol-29-projection.md) projected the compositor's
protocol-29 wire surface (output enumeration, per-output stream targets,
the stream cursor mode, the `StreamGeometryChanged` event, output picking)
and deferred its runtime consumers. Flatpak-OBS-grade screencast needs
those consumers: a real source selection, window capture, session
persistence, an embedded cursor, and surviving output geometry changes.

The ScreenCast v4+ contract also defines `persist_mode` and
`restore_token`, which the backend had been reducing to `persist_mode 0`.

## Decision

1. **Capabilities follow the negotiated protocol.** `AvailableSourceTypes`
   and `AvailableCursorModes` are 1/1 against pre-29 compositors and 3/3
   against 29+, read from the live handshake and falling back to the
   conservative values when the compositor is unreachable. The window bit
   and the Embedded cursor mode are accepted only at 29+.
2. **Source selection is a new prompter kind.** `choose_source` (process
   contract 5) renders the whole-desktop entry, one entry per connector
   when the compositor has several outputs, and a "Window…" entry when
   the client accepts window sources; a one-entry list skips the dialog,
   preserving the single-monitor single-dialog flow. A compositor
   `PickConfirm` naming the concrete target still gates every fresh
   selection; the window entry additionally runs the compositor's
   toplevel pick. `CastSource` becomes `Monitor { output: Option<String>
   }` and `Window { window: WindowId }`.
3. **Persist/restore is a small fail-closed store.** Tokens are 128-bit
   random hex, opaque to clients. Mode 1 lives in
   `$XDG_DATA_HOME/aegis-portal/screencast-restore.json` (0700/0600,
   atomic write); mode 2 lives in memory keyed by the caller's D-Bus
   unique name and is dropped when that name vanishes
   (`NameOwnerChanged`). Only monitor selections are persistable — a
   window id is not stable, so window captures never yield a token and
   report `persist_mode 0`. A valid token restores the stored selection
   with no UI; anything else (unknown, wrong app, unservable connector)
   silently degrades to the interactive flow. Tokens whose selections can
   no longer be served are pruned lazily at validation time.
4. **Geometry changes renegotiate.** On `StreamGeometryChanged` the cast
   loop stops and restarts the compositor stream with the same target,
   cursor mode, and dmabuf opt-in; requires the restarted geometry to
   match the event; swaps the `Transport` wholesale (its documented
   invariant: it always describes the live stream and the PipeWire shape
   offered for it, so fixation restarts and geometry restarts cannot
   fight over half-updated state); then re-offers the PipeWire format so
   the consumer re-fixates. A mismatched restart fails the stream
   cleanly.
5. **Damage metadata is attached per buffer.** The producer offers
   `SPA_META_VideoDamage` and writes each frame's compositor damage rects
   into the published buffer's meta block, zeroing the tail (consumers
   iterate the whole capacity; over-capacity damage collapses to one
   full-frame region, which is always a safe over-report). pipewire-rs
   0.10 exposes no safe producer-side meta API, so the writes go through
   the `spa_sys` bindings in one documented unsafe island, like the
   existing `add_buffer` patching. PipeWire attaches the meta block to
   buffers when the *consumer* requests the metadata (the OBS direction);
   the producer's own offer is harmless but not what triggers
   allocation.

## Alternatives

- **Keep the compositor confirm as the only dialog.** Rejected: OBS's
  per-output and window flows need a real chooser, and the compositor
  pick alone cannot express "the whole desktop vs one connector".
- **Persist window selections by title or app id.** Rejected: there is
  no stable identity to restore against; silently capturing a different
  window than the user consented to is worse than reporting
  `persist_mode 0`.
- **Recreate the PipeWire stream on geometry changes.** Rejected: a new
  node id would invalidate the client's captured stream handle;
  re-offering the format on the live stream is exactly what PipeWire
  renegotiation is for, and consumers (OBS) already handle it.
- **Skip the Buffers re-offer after a geometry change.** Rejected for
  the offer path but note the implementation detail: the re-offer happens
  in the `Format` `param_changed` callback, which derives the Buffers
  pod from the swapped transport with delivery-mode knowledge, rather
  than in the IPC callback that performs the restart.

## Consequences

- Single-monitor flows behave exactly as before (one consent dialog, no
  chooser); multi-output and window-capable clients gain real selection.
- The prompter process contract rises to 5; a mismatched backend/
  prompter pair keeps failing closed.
- Restore tokens survive restarts (mode 1) or exactly one app lifetime
  (mode 2); revocation means deleting the store file.
- Whole-desktop streams survive output hotplug and mode changes without
  client-visible node churn; a geometry the compositor cannot reproduce
  ends the session instead of publishing wrong-sized frames.
