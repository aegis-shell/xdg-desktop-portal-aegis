# ADR-0005: ScreenCast dmabuf transport and the slot protocol

- Status: Accepted
- Date: 2026-08-09

## Context

The ScreenCast bridge established in earlier ADRs delivered every frame as
CPU pixels: the compositor reads the output back, converts it, seals it
into a memfd, and the Portal copies it once more into a PipeWire pool
buffer. At 4K this moves about 1 GB/s through several `memcpy`s, drops the
delivered rate far below the fixed 30 fps the stream advertised, and stalls
the compositor's render loop with readbacks.

The protocol-24 wire shape already carries a
`StreamPixelFormat::Dmabuf { drm_format, modifier }` variant, so the natural
fix is forwarding each frame's dmabuf descriptor into PipeWire. An
implementation attempt and a reading of the PipeWire 1.6 stream internals
show why that cannot work:

- `pw_stream_queue_buffer` only accepts buffers previously returned by
  `pw_stream_dequeue_buffer`; a producer-owned `pw_buffer` wrapping the
  frame's descriptor is silently rejected.
- A pool buffer's `spa_data` descriptor (type and fd) is transferred to the
  consumer once, when the buffer is allocated. Per frame, only the buffer
  id and its `spa_chunk` cross. Patching a pool buffer's fd per frame — the
  pattern xdg-desktop-portal-wlr appears to use — does not propagate; the
  consumer keeps the allocation-time descriptor. This repository's
  integration test `screencast_cannot_forward_per_frame_descriptors` pins
  the behavior.

Every production portal that delivers dmabufs (wlroots, Mutter, KWin) uses
a fixed set of buffer slots registered at allocation time and a GPU blit
into a free slot per frame. The Portal has no GPU context, and giving it
one would duplicate the compositor's renderer for a copy that the
compositor can avoid entirely.

## Decision

1. **Protocol 24 (shipped): the mmap fallback.** The Portal accepts
   dmabuf-announced streams, validates each frame against the announced
   DRM format/modifier, and memory-maps the descriptor for a single copy
   into the PipeWire pool. The same path serves sealed-memfd frames, so the
   previous read-into-`Vec` copy is gone. The stream requests a 60 fps
   ceiling from the compositor and offers consumers a 1–360 fps range
   (default 60) instead of a fixed 30/1. The Portal does not offer a
   modifier-bearing format: a consumer that fixates one expects
   `SPA_DATA_DmaBuf` buffers, which per-frame descriptors cannot populate.

2. **Protocol 25 (planned): the slot protocol.** The compositor exports a
   fixed set of dmabuf slots once per stream; their descriptors cross at
   setup, and each frame then references a slot by index. The Portal
   patches PipeWire pool buffers onto the slot descriptors at allocation
   time (in the `add_buffer` event, where the descriptor transfer happens)
   and queues the matching slot per frame. When the consumer returns a
   buffer, the Portal reports the slot as released, and only then may the
   compositor reuse it. This is the only true zero-copy path that satisfies
   PipeWire's allocation-time descriptor transfer and the compositor's
   buffer-lifetime rules, and it requires a protocol-version bump because
   version 24 peers cannot decode the new messages safely.

## Alternatives

- **Forward each frame's descriptor through a self-allocated `pw_buffer`.**
  Rejected: `pw_stream_queue_buffer` rejects buffers its pool did not
  produce, and consumer-visible descriptors are fixed at allocation.
  Verified in code and by experiment.
- **Give the Portal a GBM/EGL context and blit into fixed dmabuf slots.**
  Rejected: it adds GPU dependencies to a D-Bus daemon, still pays a GPU
  copy per frame, and duplicates rendering the compositor already does.
- **Stay on the 30 fps SHM path.** Rejected: it is the reported defect.

## Consequences

- The protocol-24 fallback serves every consumer correctly but keeps one
  CPU copy per frame; 4K capture remains CPU-bound until protocol 25.
- Slot lifetimes couple the consumer's pace to the compositor's renderer:
  a stalled consumer exhausts the slot set, and the compositor drops
  frames instead of applying backpressure to the desktop.
- The wire additions (slot advertisement, per-frame slot references, slot
  release) get literal fixtures and independent test-server coverage in
  this repository before either side ships, per
  [ADR-0004](0004-portal-ownership-and-runtime-ipc-boundary.md).
- `docs/reference/portal-support.md` records which transport a running
  system negotiates.
