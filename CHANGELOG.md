# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Added

- Nine new native portal interfaces, completing full-stack ownership of
  the routing table (see
  [ADR-0007](docs/adr/0007-full-stack-interface-ownership.md)):
  - `org.freedesktop.impl.portal.Access` v1, the generic consent dialog,
    rendered by the one-shot prompter with the frontend's labels.
  - `org.freedesktop.impl.portal.AppChooser` v4, a Portal-owned chooser
    over in-process freedesktop desktop-entry, `mimeapps.list`, and
    `globs2` resolution, with a "Remember this choice" checkbox that
    records the default application. Live `UpdateChoices` is acknowledged
    but not rendered by the one-shot dialog.
  - `org.freedesktop.impl.portal.OpenURI` v3, launching the resolved
    default application directly or through the chooser when asked;
    `file://` targets take their content type from the shared-mime-info
    glob databases, other schemes resolve as `x-scheme-handler/*`.
  - `org.freedesktop.impl.portal.Background` v1, consent-prompted on every
    request, writing login autostart entries under
    `$XDG_CONFIG_HOME/autostart/`.
  - `org.freedesktop.impl.portal.DynamicLauncher` v1, a Portal-owned
    install-confirmation dialog with name editing; install tokens are
    never issued.
  - `org.freedesktop.impl.portal.Inhibit` v3, taking logind idle and
    suspend locks in `block` mode; logout and user-switch inhibition are
    tracked no-ops, and monitor sessions report the Running state.
  - `org.freedesktop.impl.portal.Notification` v2, rendered by the
    prompter's new daemon mode: a versioned newline-delimited JSON stream
    drives a single window stacking notification cards, with priority-based
    auto-dismiss and action buttons.
  - `org.freedesktop.impl.portal.Wallpaper` v1, handing local images to
    the compositor over the new protocol-26 `SetWallpaper` IPC operation
    (sealed-memfd transport), with a textual confirmation for preview
    requests.
  - `org.freedesktop.impl.portal.Print`, echoing settings from
    `PreparePrint` and submitting documents to the default printer through
    the system `lp` client.

### Changed

- The prompter process contract is version 4, adding the confirmation
  dialog's deny label, the application chooser, and the launcher editor
  prompt kinds.
- The `aegis-portal-ipc` projection speaks protocol 26 (negotiating down
  to 24); protocol 26 adds the wallpaper operation under the portal
  scope's existing `control` capability.
- The routing configuration names no other backend: every interface routes
  to `aegis` and the default is `aegis` alone. Interfaces without a
  backend in this stack (Camera, RemoteDesktop, GlobalShortcuts,
  InputCapture, USB, Location, Documents) stay unadvertised and fail
  cleanly at the frontend.

### Removed

- The `xdg-desktop-portal-gtk` fallback dependency. Production
  installations no longer install or require another portal backend.


## [0.0.9] - 2026-08-12

### Added

- Full IME support in the prompter's app-owned text fields (the location
  path, save-name, and secret surfaces). They now render the in-progress
  composition (preedit) inline — accent text underlined, caret inside at
  the composition's own cursor — apply the IME's
  `delete_surrounding_text` requests, and report the caret rectangle every
  focused frame through `lens_set_caret_rect`, so the input method's
  candidate window anchors at the caret instead of falling back to a
  default screen position. The secret field masks the composition too, so
  a preedit never echoes the password.

### Changed

- Rebuild the FileChooser's text fields and directory listing on new
  optics host-control APIs instead of app-owned implementations. The
  location and save-name fields are plain lens text fields; after a
  programmatic rewrite (Tab completion, a pre-filled name) the caret is
  moved through `lens_textfield_set_caret` (optics ADR-0064), with the
  setter applied in the field's own id scope at build time. The listing
  is now a virtualized `lens_table` (optics ADR-0066) with a keyboard
  cursor, per-cell folder/file icons, host-owned selection, and a
  per-directory scroll position (back/forward restores it); IME preedit
  and candidate-window anchoring on those fields now come from the
  toolkit itself. The dialog's headless interaction tests drive the real
  build path on a `Ui::headless` with synthetic input. These APIs ship
  in the tagged optics v0.0.14 release.
- Put the prompter dialogs on one design-token grid (`ui::style::metrics`):
  a 4 px spacing scale, paired control heights (text fields 36, buttons
  and toolbar buttons 32, listing and sidebar rows 32 minimum), a single
  corner radius, and type roles (body 14, dialog title 17, small 12.5 for
  hints, typeahead, and inline errors — the latter now in a danger color
  instead of muted gray). The location toolbar is pinned to the field
  height so swapping breadcrumbs for the path field no longer shifts the
  dialog; breadcrumb names truncate to a measured pixel budget rather
  than a character count; and keyboard navigation keeps the focused row
  inside the viewport with ensure-visible scrolling instead of a fixed
  pixel lead.

### Fixed

- ScreenCast no longer delivers a scrambled picture to consumers that
  cannot import the compositor's dmabuf modifier (reported with Flatpak
  OBS). The shared-memory fallback memory-mapped the slot descriptors and
  copied them linearly, which returns tile-swizzled bytes for the
  device-native tiled modifiers the compositor exports. Fixating the
  modifier-less format now restarts the compositor stream on the SHM
  readback transport underneath the live PipeWire connection, and the
  copy path never memory-maps a non-`DRM_FORMAT_MOD_LINEAR` descriptor;
  see [ADR-0006](docs/adr/0006-shm-consumers-switch-to-readback-transport.md).
- The `SPA_PARAM_BUFFERS` offer now advertises the layout delivery
  actually uses: the slot's stride and size for zero-copy dmabuf,
  tightly packed dimensions for the shared-memory copy path.

## [0.0.8] - 2026-08-11

### Added

- Rework the FileChooser dialog after GTK's file chooser. A places
  sidebar (Home, the configured XDG user dirs, and the filesystem root)
  offers one-click jumps; the breadcrumb bar renders as clickable chips
  with the current folder highlighted; back/forward buttons walk the
  navigation history; a create-folder action makes a directory and enters
  it. Ctrl+L, the pencil button, or typing `/`/`~` opens a type-a-path
  location field with Tab completion that accepts `~`, relative, and
  absolute paths — directories navigate, an existing file selects it, and
  in save mode the tail seeds the name field. The listing is fully
  keyboard-driven: arrows/Home/End move a cursor with selection
  following, Ctrl+Space toggles in multiple mode, typing selects by name,
  Enter activates (or accepts), Backspace and Alt+Up walk up, and saving
  over an existing file asks for overwrite confirmation first. The
  location and save-name fields are app-owned editing surfaces (the
  secret prompt's pattern) so the caret stays at the end across
  programmatic edits, with Left/Right/Home/End, Delete, and Ctrl+V
  editing.

### Changed

- Rename the prompter process contract's `selection` prompt kind to
  `file_chooser`, aligning the private wire name with the public
  `FileChooser` portal interface: `SelectionRequest`, `SelectionResponse`,
  and `SelectionMode` are now `FileChooserRequest`, `FileChooserResponse`,
  and `FileChooserMode`. The contract version rises from 2 to 3; a
  mismatched backend/prompter pair keeps refusing to interpret each
  other's fields.

### Fixed

- Keep the FileChooser footer visible: the root layout column now fills
  the window so the flexible listing absorbs any deficit — previously a
  long places list or directory listing pushed the Cancel/accept buttons
  below the window's bottom edge.
- Accept the compositor's dmabuf stream descriptors. They are anonymous
  inodes — never regular files — and their allocation may exceed the
  announced stride*height plane bytes, so the dmabuf receive path now
  validates only the size floor instead of demanding a regular file of
  exactly the announced length. The sealed-memfd path keeps its
  exact-length regular-file contract. This was the visible
  `capture descriptor length/type mismatch` ScreenCast failure.
- Buffer stream frames that race ahead of `StreamOutputStarted`. The
  compositor publishes the stream lane before it queues the reply, so an
  already-produced frame can legitimately precede it; the client now
  demultiplexes events from responses during stream start/stop and drains
  buffered frames from `next_stream_message` in arrival order instead of
  failing the start with an `unknown variant` parse error.

## [0.0.7] - 2026-08-10

### Changed

- Re-implement the prompter dialogs on the optics (iris/lens) stack
  instead of GTK4, styled after the aegis design language and following
  the system light/dark preference. FileChooser, Account confirmation,
  and Secret password requests keep the same versioned stdin/stdout
  process contract; prompts now map as independent windows because iris
  cannot yet import an exported `wayland:` parent handle. The build no
  longer requires GTK 4 development files; it requires the flux, lens,
  and iris C libraries from the tagged `ming2k/optics` release.

## [0.0.6] - 2026-08-10

### Added

- Zero-copy ScreenCast over the protocol-25 dmabuf slot transport. The
  compositor transfers a fixed set of dmabuf slot descriptors once at
  stream start; frames reference slots by index; the Portal binds each
  PipeWire pool buffer to a slot descriptor at registration and releases
  slots back to the compositor when the consumer returns them. Consumers
  that cannot import the stream's DRM modifier keep the shared-memory
  copy path. The handshake negotiates down to protocol 24 against older
  compositors, which keeps the previous transports working.

### Changed

- Raise the ScreenCast frame-rate ceiling from 30 to 60 fps and offer
  PipeWire consumers a 1–360 fps range (default 60) instead of a fixed
  30/1, so capture matches the compositor's actual cadence on 60 Hz
  outputs and each consumer paces against its own clock.
- Accept dmabuf-announced compositor streams instead of failing `Start`,
  and deliver their frames through a single mmap-and-copy into the PipeWire
  pool. Sealed-memfd frames take the same path, which removes the previous
  per-frame `Vec` allocation and copy. Per-frame dmabuf descriptors cannot
  be forwarded through PipeWire's fixed buffer pools; true zero-copy
  delivery is specified as the protocol-25 slot protocol in
  [ADR-0005](docs/adr/0005-screencast-dmabuf-slot-protocol.md).
- Log compositor-reported stream frame drops (`dropped` counter deltas)
  and Portal-side delivery drops for capture diagnostics.

## [0.0.5] - 2026-08-07

### Fixed

- Accept ScreenCast `SelectSources` source-type masks that offer window
  alongside monitor and serve the monitor subset, instead of rejecting the
  mixed offer. OBS's unified "Screen Capture (PipeWire)" source always sends
  `types = monitor|window` and aborted with a backend error, which made
  screen recording impossible.
- Fix ScreenCast frame pacing and stutter for PipeWire consumers such as
  Flatpak OBS. The stream now advertises the fixed `30/1` framerate it
  produces, pushes each compositor frame exactly once via
  `pw_stream_trigger_process`, and avoids re-copying stale frames into later
  process cycles.

## [0.0.4] - 2026-08-04

### Changed

- Remove all Aegis Git crates and sibling-checkout Cargo patches from the
  source and build graph. The Portal now owns a narrow, independent Aegis IPC
  protocol-24 client for compositor settings, capture, picking, and streams.
- Move Account consent and Secret vault password input from compositor IPC to
  the supervised, one-shot GTK4 Portal prompter used by FileChooser.
- Remove dormant native implementations for interfaces that are routed to the
  complete GTK backend.

### Added

- Add literal protocol fixtures and an independent minimal IPC server for
  media integration tests, so client and server tests do not share the
  implementation under test.

## [0.0.3] - 2026-08-04

### Fixed

- Resolve the exact Aegis `v0.0.11` IPC crates from the canonical
  `aegis-shell/aegis` repository so clean distribution builds do not depend
  on the retired `ming2k/aegis` remote.

## [0.0.2] - 2026-08-03

### Changed

- Moved FileChooser UI and filesystem enumeration out of Aegis compositor
  chrome into a one-shot GTK4 `aegis-portal-prompter` child. The backend now
  owns the complete v3 option/result mapping and kills the child on
  `Request.Close`; filesystem paths never cross compositor IPC.
- Added lossless Unix-path transport, typed glob/MIME filters,
  `current_filter`, `choices`, `modal`, Wayland parent handles, and complete
  `current_file`/`SaveFiles` handling to the FileChooser process contract.
- Advertise only the eight complete native backend interfaces and delegate
  Inhibit, AppChooser, Notification, DynamicLauncher, and Wallpaper to the
  GTK backend at the interface-routing boundary.
- Align Screenshot with version 3, ScreenCast with version 6 stable PipeWire
  serials, and Lockdown with all seven read-write properties.
- Derive stable Secret values per application instead of returning one
  shared value. This rotates values returned by the pre-production `v0.0.1`
  implementation; the encrypted vault remains intact.

### Security

- Remove the incomplete `org.freedesktop.secrets` compatibility service.
  A complete distribution keyring service must provide that separate API.
- Make vault/key persistence atomic and durable, reject symlinks, unsafe
  owners or modes, oversized files, corrupt vaults, and orphan ciphertext,
  and refuse partial daemon startup when Secret initialization fails.
- Harden PAM token delivery against environment-controlled runtime paths,
  symlink replacement, partial writes, unsafe directory modes, and
  thread-unsafe passwd lookup; zeroize credential buffers.
- Reject symlinked screenshot cache directories without changing the link
  target's permissions.
- Require explicit compositor consent for screen sharing, Account data, and
  legacy screenshots whose frontend permission was not already checked.

### Fixed

- Correct PipeWire buffer data-type masks and producer routing so a real
  WirePlumber/GStreamer consumer can negotiate and receive compositor frames.
- Prevent head-of-line blocking across Screenshot, ScreenCast, Account, and
  FileChooser requests; bound worker queues, UI/mailer/unlock tasks, total
  sessions, live casts, and user-controlled request payloads, and make
  session close cleanup race-safe.
- Parse Email attachment URIs without lossy Unix-path conversion and reap
  every spawned mailer child.
- Make real frontend tests use the backend-discovery override supported by
  both `xdg-desktop-portal` 1.18 and current releases.

### Added

- Add real public-frontend tests for Secret, Email attachment FD translation,
  and FileChooser, plus sealed-memfd screenshot and real PipeWire frame
  delivery tests.
- Add a Meson production installer with configurable `libexecdir`, portal
  metadata/routing installation, staged packaging, and optional PAM output.
- Gate the declared Rust 1.88 MSRV and both PAM-disabled and PAM-enabled
  package variants in CI.
- Add production installation, interface support, migration, and release
  documentation.

## [0.0.1] - 2026-08-02

### Added

- Established the independent `xdg-desktop-portal-aegis` workspace with the
  backend composition crate, shared request runtime, encrypted Secret
  component, optional PAM helper, activation metadata, CI, and supply-chain
  policy.
- Declared compatibility with Aegis `v0.0.9` through exact tagged Cargo
  dependencies.

[Unreleased]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/compare/v0.0.9...HEAD
[0.0.9]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.9
[0.0.8]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.8
[0.0.7]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.7
[0.0.6]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.6
[0.0.5]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.5
[0.0.4]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.4
[0.0.3]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.3
[0.0.2]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.2
[0.0.1]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.1
