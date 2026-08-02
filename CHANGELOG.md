# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Changed

- Moved FileChooser UI and filesystem enumeration out of Aegis compositor
  chrome into a one-shot GTK4 `aegis-portal-prompter` child. The backend now
  owns the complete v3 option/result mapping and kills the child on
  `Request.Close`; filesystem paths never cross compositor IPC.
- Added lossless Unix-path transport, typed glob/MIME filters,
  `current_filter`, `choices`, `modal`, Wayland parent handles, and complete
  `current_file`/`SaveFiles` handling to the FileChooser process contract.

## [0.0.1] - 2026-08-02

### Added

- Established the independent `xdg-desktop-portal-aegis` workspace with the
  backend composition crate, shared request runtime, encrypted Secret
  component, optional PAM helper, activation metadata, CI, and supply-chain
  policy.
- Declared compatibility with Aegis `v0.0.9` through exact tagged Cargo
  dependencies.

[Unreleased]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/compare/v0.0.1...HEAD
[0.0.1]: https://github.com/aegis-shell/xdg-desktop-portal-aegis/releases/tag/v0.0.1
