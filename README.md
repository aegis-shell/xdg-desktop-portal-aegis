# xdg-desktop-portal-aegis

`xdg-desktop-portal-aegis` is the private
[xdg-desktop-portal](https://flatpak.github.io/xdg-desktop-portal/)
backend for the Aegis desktop. It translates freedesktop portal D-Bus
requests into Aegis's scoped IPC, publishes ScreenCast streams through
PipeWire, and hosts the encrypted Secret portal and transitional Secret
Service compatibility API.

The repository builds a D-Bus-activated backend plus a disposable FileChooser
UI host from a small Cargo workspace:

- `xdg-desktop-portal-aegis` assembles the backend interfaces, IPC adapters,
  and workers.
- `aegis-portal-prompter` runs one GTK4 file dialog per request. It owns file
  browsing and never connects to compositor IPC.
- `aegis-portal-runtime` owns the shared portal Request lifecycle.
- `aegis-portal-secret` owns the encrypted vault and both Secret APIs.
- `aegis-pam` optionally forwards a verified login password for vault
  auto-unlock.

## Compatibility

Portal and Aegis releases have independent version sequences. Each Portal
release pins exactly one supported Aegis Git tag because the scoped IPC
schema and compositor mechanisms evolve together. Portal `v0.0.1` supports
Aegis `v0.0.9`; see the [Compatibility Reference](docs/reference/compatibility.md)
for the authoritative matrix.

## Build

Install GTK4, PipeWire, SPA, PAM, and `pkg-config` development packages, then
run:

```bash
cargo build --locked --release --workspace
cargo test --locked --workspace
```

The backend binary is private and is normally activated by D-Bus. Packaging
installs it as `/usr/lib/xdg-desktop-portal-aegis`, installs
`aegis-portal-prompter` as `/usr/lib/aegis-portal-prompter`, and installs the
files under `contrib/`.

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
- [Compatibility reference](docs/reference/compatibility.md)
- [Architecture decisions](docs/adr/index.md)
- [Contributor documentation](docs/dev/index.md)
