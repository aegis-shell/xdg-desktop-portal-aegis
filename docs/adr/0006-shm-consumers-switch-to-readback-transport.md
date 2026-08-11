# ADR-0006: SHM consumers switch the compositor stream to the readback transport

- Status: Accepted
- Date: 2026-08-12

## Context

[ADR-0005](0005-screencast-dmabuf-slot-protocol.md) established the
protocol-25 dmabuf slot protocol and kept an mmap copy as the universal
fallback: a PipeWire consumer that does not fixate the modifier-bearing
format receives a CPU copy of the frame descriptor. That fallback assumed
the descriptor is CPU-typed pixels. It is not. The compositor deliberately
exports its slots with a device-native tiled modifier (for example
`DRM_FORMAT_MOD_I915_X_TILED`), and memory-mapping such a descriptor
returns tile-swizzled bytes. A modifier-ignorant consumer — Flatpak OBS is
the reported case — therefore received a scrambled picture: the Portal
linearly copied a tiled GPU buffer into the shared-memory pool.

The failure was silent because the delivery mode defaults to shared
memory, so the fallback never logged, and every test used
`DRM_FORMAT_MOD_LINEAR` memfd stand-ins, where the mmap copy happens to be
exact.

The compositor already owns the correct fallback: its SHM readback
transport renders or copies on the GPU and delivers tightly packed,
sealed-memfd frames — the same shape wlroots portals receive from
`wlr-screencopy` SHM frames. The wire protocol selects it with the
`dmabuf` flag on `StreamOutputStart`. Only the Portal's fixation handling
failed to use it.

## Decision

The delivery transport is chosen from the fixated PipeWire format, and the
Portal restarts the compositor stream to match, underneath the live
PipeWire connection:

1. A consumer that fixates the offered modifier gets zero-copy dmabuf
   slot delivery, as before.
2. A consumer that does not, facing a tiled (non-`DRM_FORMAT_MOD_LINEAR`)
   dmabuf transport, triggers a `StreamOutputStop`/`StreamOutputStart`
   cycle with the `dmabuf` flag cleared. The compositor answers on the SHM
   readback transport, whose frames the Portal copies into the pool. The
   offered PipeWire format is identical on both transports, so the
   consumer never observes the switch.
3. A LINEAR dmabuf transport stays in place for SHM consumers:
   memory-mapping it is exact, so the copy path remains the fast path.
4. As an invariant, the copy path never memory-maps a
   non-`DRM_FORMAT_MOD_LINEAR` descriptor. Frames that would need it are
   dropped — an honest stall, never scrambled output.

This amends ADR-0005's decision points 1 and 2: the mmap fallback serves
only SHM and LINEAR-dmabuf frames, and the slot stream's shared-memory
fallback is the readback transport switch, not a copy of the slot
descriptor.

## Alternatives

- **Offer only the modifier-bearing format on tiled streams.** Rejected:
  consumers that cannot import the modifier fail negotiation entirely and
  show nothing. Correct but unhelpful; the readback transport gives them
  a working capture.
- **Ask the compositor for LINEAR slots and keep the mmap copy.**
  Rejected: it forces the compositor's capture surface off its native
  layout for every SHM consumer, adds wire negotiation, and still pays a
  slow uncached CPU read of GPU memory per frame. The readback transport
  already exists and is byte-exact.
- **Detile in the Portal with a GBM/EGL context.** Rejected in ADR-0005
  and still rejected: no GPU stack in a D-Bus daemon.
- **Do nothing beyond refusing the tiled mmap copy.** Rejected: it turns
  scrambled output into a black picture for the exact consumers the
  fallback exists to serve.

## Consequences

- Flatpak OBS and other modifier-ignorant consumers receive correct
  pixels, at the cost of the compositor's GPU readback per frame — the
  same trade SHM consumers get on wlroots and Mutter.
- The transport switch is a stop/start round trip on the scoped IPC
  connection, triggered once per negotiation; consumers that renegotiate
  mid-stream (OBS removes an unimportable modifier and retries) can
  switch in both directions.
- Frames in flight during a switch are dropped, and frames from a
  superseded stream are filtered by `stream_id`. Teardown and slot
  releases always name the current stream.
- The end-to-end suite now covers a tiled-modifier slot stream: the fake
  compositor observes the stop/restart cycle and the consumer receives
  the readback pixels. The `DRM_FORMAT_MOD_LINEAR` stand-ins keep pinning
  the LINEAR copy path.
- `docs/reference/portal-support.md` continues to describe the negotiated
  transport accurately: zero-copy dmabuf with a shared-memory fallback.
