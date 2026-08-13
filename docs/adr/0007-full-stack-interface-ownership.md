# ADR-0007: Full-stack interface ownership

- Status: Accepted
- Date: 2026-08-13
- Amends: [ADR-0004](0004-portal-ownership-and-runtime-ipc-boundary.md)

## Context

ADR-0003 delegated Inhibit, AppChooser, Notification, DynamicLauncher, and
Wallpaper to `xdg-desktop-portal-gtk`, and the `aegis;gtk` default route let
GTK serve every interface Aegis did not advertise, including Print, Access,
and OpenURI. The delegation was a completeness hedge: the portal frontend
selects one backend per interface, so a partial Aegis implementation would
have shadowed the complete GTK one.

The optics (iris/lens) prompter stack established by ADR-0004 now covers
every interaction those interfaces need, and the remaining compositor-owned
resource — the desktop wallpaper — fits the scoped runtime IPC boundary.
Keeping the GTK backend as a permanent fallback instead couples every Aegis
session to a second portal backend whose UI, toolkit, and release cadence
this project does not control.

## Decision

The Aegis backend natively serves every interface it routes, and the routing
configuration names no other backend: the default route is `aegis` alone.
`xdg-desktop-portal-gtk` is neither a build nor a runtime dependency.

Ownership follows the ADR-0004 boundary. Portal-owned resources are served
in-process or by the prompter, with no compositor IPC:

- Access, AppChooser, and DynamicLauncher are one-shot prompter dialogs.
- OpenURI and AppChooser share a hand-rolled freedesktop desktop-entry,
  mimeapps, and `globs2` resolution inside the backend.
- Background writes login autostart entries under
  `$XDG_CONFIG_HOME/autostart/` itself.
- Notification extends the prompter with a daemon mode speaking a versioned
  newline-delimited JSON stream, because notifications are asynchronous and
  long-lived where prompts are one-shot.
- Inhibit takes logind idle and suspend locks in `block` mode; logout and
  user-switch inhibition have no session-manager equivalent in this stack
  and are tracked no-ops.
- Print spools the document and submits it through the system `lp` client,
  the same system-tool hand-off Email uses for `xdg-email`.

Wallpaper is the one wire extension: the compositor draws outputs, so the
new protocol-26 `SetWallpaper` operation hands the image to the compositor
as a sealed memfd under the portal scope's existing `control` capability.

Interfaces with no backend in this stack (Camera, RemoteDesktop,
GlobalShortcuts, InputCapture, USB, Location, Documents) stay unadvertised;
the portal frontend fails requests for them cleanly.

## Alternatives

- **Keep the GTK fallback for uncovered interfaces.** Rejected because every
  interface the fallback covered is now served natively, and a dormant
  second backend still ships its toolkit and its own UI behavior in every
  session.
- **Extend compositor IPC for notifications and dialogs.** Rejected by
  ADR-0004: these surfaces own no compositor resource, and routing them
  through the compositor would widen the runtime authority boundary while
  making the portal non-functional without it.
- **Present a print dialog.** Rejected because the lens stack has no print
  UI and the settings arrive fully formed; the backend echoes them with a
  fresh token, which the specification permits.
- **Block wallpaper support until a preview-capable UI exists.** Rejected
  because the specification allows direct application, and the preview
  option degrades to a textual confirmation until lens decodes images.

## Consequences

- Production packages no longer install or require `xdg-desktop-portal-gtk`;
  the routing file ships `=aegis` routes only.
- The protocol-26 wire contract is a one-way door: changing the
  `SetWallpaper` shape requires a new protocol version and coordinated
  releases in both repositories, like the ADR-0005 slot protocol.
- Inhibit degrades honestly without logind, Print without `lp`, Email
  without `xdg-email`, and Wallpaper without a protocol-26 compositor: the
  affected calls report ordinary backend errors while the rest of the
  service stays up.
- Notification banners render as a plain toplevel window, not a borderless
  always-on-top overlay, until the optics stack grows such a surface.
- New interfaces added by future portal frontend releases have no automatic
  home; each needs an explicit native implementation before it is
  advertised.
