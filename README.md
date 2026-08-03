# xdg-desktop-portal-aegis

`xdg-desktop-portal-aegis` is the private
[xdg-desktop-portal](https://flatpak.github.io/xdg-desktop-portal/)
backend for the Aegis desktop. It translates freedesktop portal D-Bus
requests into Aegis's scoped IPC, publishes ScreenCast streams through
PipeWire, and hosts the encrypted, per-application Secret portal.

The repository builds a D-Bus-activated backend plus a disposable FileChooser
UI host from a small Cargo workspace:

- `xdg-desktop-portal-aegis` assembles the backend interfaces, IPC adapters,
  and workers.
- `aegis-portal-prompter` runs one GTK4 file dialog per request. It owns file
  browsing and never connects to compositor IPC.
- `aegis-portal-runtime` owns the shared portal Request lifecycle.
- `aegis-portal-secret` owns the encrypted vault and native Secret backend.
- `aegis-pam` optionally forwards a verified login password for vault
  auto-unlock.

## Compatibility

Portal and Aegis releases have independent version sequences. Each Portal
release pins exactly one supported Aegis Git tag because the scoped IPC
schema and compositor mechanisms evolve together. Portal `v0.0.1` supports
Aegis `v0.0.9`; see the [Compatibility Reference](docs/reference/compatibility.md)
for the authoritative matrix.

## Build

Install Meson, GTK 4.10 or newer, PipeWire, SPA, and `pkg-config` development
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

## Joint Development

Canonical builds resolve Aegis from the tagged Git dependency. To work
against an adjacent Aegis checkout, copy the local patch template and adjust
its paths when the checkout is not named `aegis`:

```bash
cp .cargo/aegis-local.toml .cargo/config.toml
git config core.hooksPath .githooks
cargo test --workspace
```

The generated `.cargo/config.toml` is ignored. While local Aegis mode is
active, the pre-commit hook keeps the path-resolved `Cargo.lock` out of
commits.

## Documentation

- [Documentation index](docs/index.md)
- [Production installation](docs/how-to/install-production.md)
- [Portal support reference](docs/reference/portal-support.md)
- [Compatibility reference](docs/reference/compatibility.md)
- [Architecture decisions](docs/adr/index.md)
- [Contributor documentation](docs/dev/index.md)
