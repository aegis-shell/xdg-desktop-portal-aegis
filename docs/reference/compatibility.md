# Compatibility Reference

Portal and Aegis use independent release sequences. Portal `v0.0.8`
implements the required Aegis IPC wire subset inside the
Portal-owned `aegis-portal-ipc` crate. Compatibility is defined by the wire
protocol and verified Aegis protocol schemas; it is not a Cargo source
dependency.

| Portal line | Aegis runtime | IPC protocol | Aegis build dependency |
|-------------|---------------|--------------|------------------------|
| `v0.0.8` | `v0.0.15`–`v0.0.18` (25); `v0.0.11`–`v0.0.12` (24) | 25, negotiates down to 24 | None |
| `v0.0.7` | `v0.0.15`–`v0.0.17` (25); `v0.0.11`–`v0.0.12` (24) | 25, negotiates down to 24 | None |
| `v0.0.6` | `v0.0.15` (25); `v0.0.11`–`v0.0.12` (24) | 25, negotiates down to 24 | None |
| `v0.0.5` | `v0.0.11`, `v0.0.12` | 24 | None |
| `v0.0.4` | `v0.0.11`, `v0.0.12` | 24 | None |
| `v0.0.3` | `v0.0.11`, `v0.0.12` | 24 | Exact `v0.0.11` tagged Git crates |
| `v0.0.2` | `v0.0.11` | 24 | Exact tagged Git crates |
| `v0.0.1` | `v0.0.9` | Release-specific | Exact tagged Git crates |

Portal `v0.0.8` builds and tests without an Aegis checkout. Its committed
`Cargo.lock` contains no package from the Aegis repository. A production
installation still needs a running Aegis compositor because Settings,
Screenshot, color and target selection, and ScreenCast consume
compositor-owned resources.

Protocol 24 is verified against the `v0.0.11` and `v0.0.12` schemas. Portal `v0.0.6`
offers protocol 25 at the handshake and accepts a
downgrade to 24, so it keeps working with protocol-24 compositors using the
SHM transports while protocol-25 compositors additionally provide the
zero-copy dmabuf slot stream (see
[ADR-0005](../adr/0005-screencast-dmabuf-slot-protocol.md)). A future Aegis
release is compatible when it preserves protocol 24 or implements the
verified protocol-25 schema.

FileChooser, Account, Secret, Email, and Lockdown do not require compositor
IPC. FileChooser, Account confirmation, and Secret password input use the
versioned one-shot Portal prompter contract.
