# ADR-0002: Resource authority and FileChooser process boundary

- Status: Accepted
- Date: 2026-08-02
- Supersedes: [ADR-0001](0001-repository-and-compatibility-boundary.md)

## Context

The independent repository and exact-release mapping from ADR-0001 remain
necessary, but its statement that Aegis owns all compositor-hosted consent
chrome put the FileChooser on the wrong side of the boundary. The compositor
implementation synchronously enumerated host directories, carried paths over
private IPC, and reproduced a partial file browser. Meanwhile the portal
adapter discarded `modal`, `parent_window`, `choices`, and `current_filter`,
and inferred filter types from their text.

File selection authorizes host filesystem resources. The portal backend owns
that contract and the request lifetime; the compositor owns scene resources
and window relationships. Rendering a filesystem browser inside the
compositor combines unrelated failure domains and makes the private IPC schema
duplicate the public portal ABI.

## Decision

Keep `xdg-desktop-portal-aegis` in this independent repository with its own
versions, lockfile, CI, artifacts, and exact Aegis compatibility mapping.
Divide interactive work by resource authority:

- the backend process owns the FileChooser D-Bus adapter, option validation,
  result encoding, cancellation, and child-process supervision;
- a new one-shot `aegis-portal-prompter` process owns GTK4 file browsing and
  the complete local FileChooser interaction;
- Aegis owns only the caller/prompter transient relationship, implemented by
  the standard `xdg-foreign-unstable-v2` Wayland protocol; and
- compositor IPC owns no file-selection operation and carries no filesystem
  path, filter, or filename.

The backend sends one explicitly versioned JSON request over anonymous stdin
and receives one versioned JSON response over stdout. Unix paths are byte
arrays, not UTF-8 strings. One independently supervised child exists for one
request, so concurrent clients cannot block each other's chooser lifecycle;
`Request.Close` terminates only its child, and a crash maps to portal response
code 2 without affecting the backend or compositor. The child sets
`GTK_USE_PORTAL=0` so its chooser cannot recurse into the portal it serves.

The process contract preserves FileChooser v3 semantics: `modal`, Wayland
`parent_window`, `multiple`, directory selection, `accept_label`,
`current_folder`, `current_name`, `current_file`, typed glob/MIME filters,
`current_filter`, `choices`, and ordered `SaveFiles` names. `SaveFiles`
basename validation and collision avoidance run beside the chooser, before
final paths are returned.

The remaining workspace boundaries are:

- `xdg-desktop-portal-aegis` composes D-Bus interfaces, Aegis adapters, and
  workers;
- `aegis-portal-prompter` owns disposable host-resource UI;
- `aegis-portal-runtime` owns shared Request lifecycle primitives;
- `aegis-portal-secret` owns encrypted state and both Secret APIs; and
- `aegis-pam` produces the optional login token.

Protocol-facing identifiers remain `aegis`. Portal and Aegis keep independent
release sequences and exact compatibility mappings.

## Alternatives

- **Keep FileChooser in compositor chrome.** Rejected because directory I/O,
  path data, and a general file-browser model are not compositor resources.
- **Delegate FileChooser to `xdg-desktop-portal-gtk`.** Rejected because it
  gives another backend the request lifecycle and makes Aegis behavior depend
  on fallback routing. Reusing GTK as a UI toolkit inside the owned prompter
  retains one authoritative backend.
- **Link GTK into the resident backend.** Rejected because toolkit state and
  slow filesystem providers would share the long-lived D-Bus process's fault
  domain.
- **Create a new private compositor parenting IPC.** Rejected because
  xdg-foreign already expresses exactly the cross-client window capability
  and is understood by GTK.

## Consequences

- The compositor never reads directories for a portal request and cannot
  observe the selected paths.
- A chooser crash, blocked mount, or toolkit defect is contained to one child
  process; cancellation has an operating-system-enforced cleanup boundary.
- The portal package gains a GTK4 runtime dependency and must install
  `/usr/lib/aegis-portal-prompter` beside the backend.
- Aegis must provide xdg-foreign-v2 and releases paired with this design must
  use the IPC version that removed `PickFile`.
- Aegis is a Wayland session, so `wayland:` parent handles are authoritative.
  Supporting `x11:` handles remains contingent on future XWayland support,
  not on FileChooser ownership.
