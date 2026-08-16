# ADR-0015: The protocol-29 projection

- Status: Accepted
- Date: 2026-08-16

## Context

The compositor is extending its IPC to protocol 29 in parallel with this
change. The additions the Portal needs are:

- `EnumerateOutputs`, answered by `Outputs { outputs: Vec<OutputInfo> }`:
  the connector, primary flag, and logical rectangle of every output.
- A connector-addressed stream target: `StreamTarget::Output` gains an
  optional `output` connector field. The bare `{"type":"Output"}` shape is
  unchanged and still means the whole desktop, so protocol 28 and older
  peers read and write it identically.
- A `cursor` mode on `StreamOutputStart` (`hidden`, the compositor
  default, or `embedded`).
- A `StreamGeometryChanged { stream_id, width, height }` stream event.
  After it, the compositor produces no further frames for the stream
  until the client restarts it (`StreamOutputStop` + `StreamOutputStart`).
- Output picking: `PickKind::Output`, with `PickResult::Output` gaining an
  optional `connector` (older compositors report none).

Every addition is additive on the wire; none changes a shape protocol 24
peers exchange. The projection's coordination rule still applies (see
[ADR-0011](0011-wallpaper-wire-reconciliation.md)): never project ahead of
the compositor, pin literal fixtures, and cover the additions with the
independent test server rather than any compositor code.

## Decision

1. The `aegis-portal-ipc` projection is re-baselined to
   `PROTOCOL_VERSION = 29`, negotiating down to 24. Version-gated features
   key off the negotiated version, as dmabuf slots do at 25. Upstream
   protocols 26–28 remain deliberately unprojected.
2. The wire additions above are projected exactly, with literal fixtures
   in both directions where meaningful — including the compatibility
   shapes (bare `{"type":"Output"}` deserializes as a connector-less
   target and pick result on either side).
3. The client degrades version-gated stream-start parameters explicitly:
   the cursor mode is sent only to a protocol-29 peer (the peer's default
   is `hidden` anyway), while a connector-named target *fails closed*
   against an older peer — the only thing the peer could stream is the
   whole desktop, which captures more than the caller asked for.
4. `StreamGeometryChanged` surfaces on the client's stream lane as
   `StreamMessage::GeometryChanged`. The cast loop acknowledges it; the
   restart (stop + start, with PipeWire renegotiation) lands together with
   the runtime consumers of output addressing and cursor mode in a
   follow-up change.
5. The independent test server answers `EnumerateOutputs` from the
   handler, pushes `StreamGeometryChanged`, and threads the cursor mode
   into `stream_output_start`, so Portal tests observe the whole v29
   surface.

## Alternatives

- **Project the additions only when their runtime consumers land.**
  Rejected: the wire contract, its fixtures, and the test-server coverage
  are the coordination surface with the compositor; landing them together
  keeps the consumer change reviewable on its own, and ADR-0004 requires
  wire changes to arrive with literal fixtures and independent-server
  coverage.
- **Silently strip the connector against a pre-29 peer.** Rejected:
  degrading a one-output request into a whole-desktop capture records
  more than the user consented to; failing closed is the only honest
  degradation.
- **Handle the geometry restart now.** Deferred, not rejected: the event
  freezes the stream exactly as a compositor-side halt already could, so
  modeling it first changes no runtime behavior; the restart logic needs
  the output-addressing consumer to be testable end to end.

## Consequences

- The Portal offers protocol 29 at the handshake and behaves identically
  to its previous self against protocol 24–28 compositors; the v29 surface
  activates only where negotiated.
- `StreamTarget` and `PickResult` lose their `Copy` impls (the connector
  is an owned `String`); both are message types that move, never copy.
- The cast loop's pending-slot overwrite now also releases the superseded
  slot — found while wiring the v29 stream surface, fixed in the same
  change; the regression test drives two slot frames without an
  intervening PipeWire cycle.
- `docs/reference/ipc-wire-protocol.md` and
  `docs/reference/compatibility.md` record the new baseline.
