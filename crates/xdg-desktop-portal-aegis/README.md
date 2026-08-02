# xdg-desktop-portal-aegis

The `xdg-desktop-portal-aegis` crate builds the private D-Bus backend
process. It owns interface registration, scoped Aegis IPC adapters, request
workers, the FileChooser prompter lifecycle, and the PipeWire ScreenCast
bridge. FileChooser requests run in a fresh `aegis-portal-prompter` child;
neither the backend nor Aegis compositor implements a file browser.

Secret storage is provided by `aegis-portal-secret`. Shared portal Request
objects and cancellation tracking come from `aegis-portal-runtime`. Both are
linked into this one process; neither is deployed separately.
