# Portal Support Reference

## Native Interfaces

| Backend interface | Contract level | Aegis behavior |
|-------------------|----------------|----------------|
| `org.freedesktop.impl.portal.Settings` | Version 1 | Compositor-owned appearance and input settings |
| `org.freedesktop.impl.portal.Screenshot` | Version 3 | Area target, color picking, and consent-checked legacy output capture |
| `org.freedesktop.impl.portal.ScreenCast` | Version 6 | One monitor stream, hidden cursor, stable `pipewire-serial` |
| `org.freedesktop.impl.portal.Secret` | Version 1 | Stable per-application secret from the encrypted vault; Portal-owned masked unlock prompt |
| `org.freedesktop.impl.portal.Lockdown` | Current seven-property ABI | All properties are read-write and process-resident |
| `org.freedesktop.impl.portal.FileChooser` | Current backend ABI | Open, save, directory, and multiple-file flows through a one-shot GTK4 process |
| `org.freedesktop.impl.portal.Email` | Current backend ABI | `xdg-email` handoff, attachment URI validation, activation token forwarding |
| `org.freedesktop.impl.portal.Account` | Current backend ABI | Name and optional avatar after explicit Portal-owned confirmation |

`FileChooser`, `Email`, and `Account` do not define a backend `version`
property. The backend does not add one.

## Delegated Interfaces

| Interface | Route | Reason |
|-----------|-------|--------|
| `Inhibit` | `gtk` | Aegis has no complete logout, switch-user, suspend, idle, and monitor contract |
| `AppChooser` | `gtk` | Aegis has no complete live choice-update and activation-token contract |
| `Notification` | `gtk` | Aegis notifications do not represent all portal actions and metadata |
| `DynamicLauncher` | `gtk` | Aegis does not implement the complete editable launcher contract |
| `Wallpaper` | `gtk` | Aegis does not implement every preview and destination option |

All other unadvertised interfaces follow `default=aegis;gtk`; the portal
frontend skips Aegis when `aegis.portal` does not advertise the requested
interface.

## Runtime Dependencies

| Component | Purpose |
|-----------|---------|
| Aegis IPC protocol 24 | Compositor settings, screenshot capture and selection, capture consent, and ScreenCast frames |
| `xdg-desktop-portal` | Public portal frontend |
| `xdg-desktop-portal-gtk` | Complete fallback interfaces |
| GTK 4.10 or newer | One-shot FileChooser, Account, and Secret prompter process |
| PipeWire and WirePlumber | ScreenCast transport and routing |
| `xdg-email` | Email handoff |
| PAM | Optional login-time vault unlock only |

The release gates exercise two production integration baselines:

| Baseline | Frontend | GTK | PipeWire | WirePlumber |
|----------|----------|-----|----------|-------------|
| Ubuntu 24.04 | 1.18.4 | 4.14.5 | 1.0.5 | 0.4.17 |
| Current development | 1.20.4 | 4.22.4 | 1.6.4 | 0.5.14 |

Meson enforces GTK 4.10 or newer, `libpipewire-0.3` 0.3 or newer, and the
SPA 0.2 development ABI. The Ubuntu baseline is tested with Rust 1.88, the
minimum supported Rust version. Compatible newer releases remain supported
through their stable ABIs.

See the [Compatibility Reference](compatibility.md) for the Aegis releases
whose protocol-24 wire schemas are verified by the current Portal line.

## Persistent State

The default vault directory is
`$XDG_DATA_HOME/aegis/secrets`, or `$HOME/.local/share/aegis/secrets` when
`XDG_DATA_HOME` is unset.

| Path | Mode | Purpose |
|------|------|---------|
| `vault.key` | `0600` | Random master key for key-file mode |
| `vault.salt` | Not group/other writable | Argon2id salt for password mode |
| `vault.enc` | `0600` | XChaCha20-Poly1305 encrypted vault |

The directory is private to the user. Symlinks, unexpected owners, unsafe
modes, oversized input, orphan ciphertext, and malformed encryption are
startup errors. Back up all files together while the daemon is stopped.

The production per-application derivation differs from the shared secret
returned by the pre-production `v0.0.1` implementation. The first production
upgrade preserves the vault but rotates the value returned to applications.
Data encrypted directly with the old portal value must be recreated.
