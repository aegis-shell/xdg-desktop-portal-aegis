# ADR-0003: Production interface and secret boundary

- Status: Superseded by [ADR-0004](0004-portal-ownership-and-runtime-ipc-boundary.md)
- Date: 2026-08-03
- Supersedes: [ADR-0002](0002-resource-authority-and-file-chooser-process-boundary.md)

## Context

The portal frontend selects one backend for an entire interface. Advertising
an interface whose uncommon methods, properties, update signals, or option
semantics are incomplete prevents a complete fallback backend from serving
that interface. The earlier implementation advertised partial Inhibit,
AppChooser, Notification, DynamicLauncher, and Wallpaper backends.

The workspace also exposed `org.freedesktop.secrets`, although it did not
implement the complete Secret Service collection, alias, lock, prompt,
session, and item contract. That API is separate from the native
`org.freedesktop.impl.portal.Secret` backend and cannot be described as a
compatibility layer when clients can observe missing semantics.

Production ScreenCast and Secret paths carry higher security and data
integrity risk than ordinary adapters. They require real consumer tests,
per-application key isolation, fail-closed startup, atomic persistence, and
bounded failure behavior.

## Decision

The Aegis backend advertises exactly these native interfaces:

- `Settings` version 1;
- `Screenshot` version 3, with Area as the only advertised target;
- `ScreenCast` version 6, with monitor sources and hidden cursor mode;
- `Secret` version 1;
- `Lockdown` with all seven read-write properties;
- `FileChooser` with the complete backend method contract;
- `Email`; and
- `Account`.

The routing configuration delegates Inhibit, AppChooser, Notification,
DynamicLauncher, and Wallpaper to `xdg-desktop-portal-gtk`. The default route
is `aegis;gtk`, so GTK also serves interfaces not advertised by Aegis.

The process exposes only `org.freedesktop.impl.portal.Secret`. It never owns
`org.freedesktop.secrets`. Secret output is a stable HKDF-SHA256 derivation of
the vault master key and the frontend-supplied application ID. Vault and key
updates use same-directory atomic replacement, restrictive modes, owner and
symlink checks, file synchronization, and directory synchronization. If the
advertised Secret storage cannot initialize safely, the daemon refuses the
entire startup instead of acquiring its D-Bus name with a missing interface.

ScreenCast republishes scoped compositor frames as a PipeWire output node.
The backend reports the stable PipeWire object serial required by version 6,
does not autoconnect the source through session-manager policy, and accepts
only exact BGRA frame geometry and bounded frame sizes. A real PipeWire,
WirePlumber, and GStreamer producer-consumer test is a release gate.

The FileChooser process boundary and exact Aegis release mapping from
ADR-0002 remain in force.

## Alternatives

- **Advertise partial native interfaces and rely on per-method fallback.**
  Rejected because backend selection is interface-wide.
- **Keep the incomplete Secret Service shim for compatibility.** Rejected
  because a partial credential-storage API creates silent corruption and
  interoperability risk.
- **Return one portal secret for every application.** Rejected because the
  Secret portal requires a unique, stable per-application secret.
- **Start without Secret when its vault is unsafe.** Rejected because the
  static portal metadata would still route Secret calls to the process.

## Consequences

- Production packages require `xdg-desktop-portal-gtk` as the fallback
  backend and must install `aegis-portals.conf`.
- Desktop applications that require Secret Service need a separate complete
  provider such as the distribution keyring service.
- Secrets returned by the pre-production shared-key implementation rotate
  once when upgrading to the per-application derivation. Applications must
  recreate data encrypted with that old value; the encrypted vault itself is
  preserved.
- A corrupt or unsafe Secret vault prevents D-Bus activation and produces an
  explicit startup error rather than a partial service.
- Release validation requires the real public portal frontend for Secret,
  Email, and FileChooser and a real PipeWire consumer for ScreenCast.
