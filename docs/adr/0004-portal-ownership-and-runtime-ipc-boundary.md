# ADR-0004: Portal ownership and runtime IPC boundary

- Status: Accepted
- Date: 2026-08-04
- Supersedes: [ADR-0003](0003-production-interface-boundary.md)

## Context

The interface boundary in ADR-0003 remains valid, but the implementation
still compiled the Portal against Aegis's internal Rust crates. A local Cargo
patch redirected those Git dependencies to a sibling checkout. This coupled
Portal builds, lockfiles, tests, and source layout to the compositor even
though a portal backend is a separately activated process.

The same IPC surface also carried Account consent and Secret password input.
Those interactions do not access a compositor-owned resource. Account data
belongs to the Portal contract, and the encrypted vault belongs to this
repository. Sending either interaction through compositor IPC widened the
runtime authority boundary without adding a required compositor capability.

Screenshot pixels, color and region selection, ScreenCast frames, and desktop
preferences are different. Only the compositor can authoritatively provide
those resources, so the backend requires a narrow runtime protocol for them.

## Decision

The Portal source and build graph is independent from the Aegis repository.
It contains no Aegis Git crate, internal model crate, test server, or sibling
path patch. `Cargo.lock` is canonical in every worktree.

The workspace owns `aegis-portal-ipc`, an independent projection of Aegis IPC
protocol version 24. It contains only the wire types and operations required
for compositor-owned portal resources:

- desktop preference snapshots and change notifications;
- screenshot capture through sealed memfd transfer;
- region, pixel, and screen-share target selection;
- capture-related confirmation; and
- ScreenCast output streams.

Unknown fields inside known version-24 responses are ignored so internal
model growth does not break the Portal. Unknown response variants and
protocol-version mismatches fail closed. Literal JSON fixtures and a minimal
independent server test the wire contract without sharing the compositor's
implementation.

The one-shot `aegis-portal-prompter` process owns all Portal UI that does not
require compositor resources:

- FileChooser file and directory selection;
- Account identity-sharing confirmation; and
- masked Secret vault password input.

The backend supervises one process per interactive request. Its versioned
stdin/stdout contract carries typed prompt requests and responses. Account
and FileChooser import the frontend-provided `wayland:` parent handle through
xdg-foreign-v2. Secret values are redacted from debug output and zeroized at
the process-contract boundary.

Email handoff and Lockdown state remain local to the backend. They do not use
Aegis IPC. The daemon may serve Portal-owned interfaces without an available
compositor socket; calls for compositor-owned resources report an ordinary
backend error when IPC is unavailable.

Compatibility is recorded by IPC protocol version and verified Aegis protocol
schemas, not by a Cargo source pin. Distribution packages still depend on an
Aegis runtime that implements the required protocol because the
compositor-owned operations need a live provider.

## Alternatives

- **Keep depending on Aegis's `aegis-ipc` crate.** Rejected because its public
  schema re-exports compositor model and authority types, turning internal
  refactors into Portal build failures.
- **Copy the complete Aegis IPC schema.** Rejected because most operations are
  unrelated to portal interfaces and would recreate the same broad coupling
  inside this repository.
- **Remove all runtime IPC.** Rejected because a passive portal trigger does
  not give the backend direct access to compositor pixels, target selection,
  frame streams, or compositor-owned preferences.
- **Render every prompt in compositor chrome.** Rejected because Account,
  FileChooser, and Secret resources are Portal-owned and need no compositor
  authority.

## Consequences

- The Portal builds and tests without an Aegis checkout or network fetch from
  the Aegis repository.
- Aegis internal crate moves and type refactors do not affect the Portal when
  the version-24 wire contract is unchanged.
- An incompatible wire change requires a protocol-version change, coordinated
  implementations, fixture updates, and compatibility testing in both
  repositories.
- The Portal duplicates a small set of wire structs and must keep their
  serialized representation under explicit conformance tests.
- Account and Secret remain usable independently of compositor IPC, while
  media and settings operations retain the least runtime authority they need.
