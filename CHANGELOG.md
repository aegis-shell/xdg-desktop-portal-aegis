# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

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

[Unreleased]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/compare/v0.0.5...HEAD
[0.0.5]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.5
[0.0.4]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.4
[0.0.3]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.3
[0.0.2]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.2
[0.0.1]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.1
