# xdg-desktop-portal-aegis

`xdg-desktop-portal-aegis` is the private
[xdg-desktop-portal](https://flatpak.github.io/xdg-desktop-portal/)
backend for the Aegis desktop. It translates freedesktop portal D-Bus
requests into Portal-owned services and, only for compositor resources, a
narrow projection of Aegis IPC protocol 24. It publishes ScreenCast streams
through PipeWire and hosts the encrypted, per-application Secret portal.

The repository builds a D-Bus-activated backend plus a disposable FileChooser
UI host from a small Cargo workspace:

- `xdg-desktop-portal-aegis` assembles the backend interfaces, IPC adapters,
  and workers.
- `aegis-portal-ipc` implements the protocol-24 settings, capture, picking,
  and streaming wire contract without depending on Aegis source crates.
- `aegis-portal-prompter` runs one optics (iris/lens) interaction per
  request. It owns file browsing, Account consent, and Secret password input
  and never connects to compositor IPC.
- `aegis-portal-runtime` owns the shared portal Request lifecycle.
- `aegis-portal-secret` owns the encrypted vault and native Secret backend.
- `aegis-pam` optionally forwards a verified login password for vault
  auto-unlock.

## Compatibility

Portal and Aegis releases have independent version sequences. Portal `v0.0.8`
implements Aegis IPC protocol 24; its wire schema is
verified against Aegis `v0.0.11` and `v0.0.12`. This is a runtime
compatibility contract, not a source dependency; see the
[Compatibility Reference](docs/reference/compatibility.md).

## Build

Install Meson, the optics C libraries (flux/lens/iris, from the tagged
`ming2k/optics` release), PipeWire, SPA, and `pkg-config` development
packages, then run:

```bash
cargo build --locked --release --workspace
cargo test --locked --workspace
```

Build and stage the production installation with:

```bash
meson setup build --buildtype=release --prefix=/usr -Dpam=false
meson compile -C build
DESTDIR="$PWD/stage" meson install -C build
```

Meson installs both private executables under `libexecdir`, generates the
D-Bus activation file with that exact path, and installs the portal metadata
and routing configuration. The optional PAM module is enabled with
`-Dpam=true`; it requires PAM development files. A production installation
also requires `xdg-desktop-portal-gtk` for interfaces intentionally delegated
to the GTK backend. See [How to Install for Production](docs/how-to/install-production.md).

The repository's own source is MIT-licensed. A binary package that includes
the optional `pam_aegis.so` module must additionally declare GPL-3.0-only
because that module links the GPL-licensed `pamsm` dependency.

## Protocol Development

Build and test this repository without an Aegis checkout:

```bash
cargo check --locked --workspace
cargo test --locked --workspace
```

When a compositor wire change is required, update the narrow
`aegis-portal-ipc` projection and its literal protocol fixtures, then test the
assembled daemon against the independently implemented test server. Follow
[Cross-Repository Protocol Development](docs/dev/cross-repository-development.md)
for release coordination.

## Documentation

- [Documentation index](docs/index.md)
- [Production installation](docs/how-to/install-production.md)
- [Portal support reference](docs/reference/portal-support.md)
- [Compatibility reference](docs/reference/compatibility.md)
- [Architecture decisions](docs/adr/index.md)
- [Contributor documentation](docs/dev/index.md)
