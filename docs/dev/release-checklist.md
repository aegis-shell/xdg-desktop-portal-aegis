# Release Checklist

## Canonical Dependency State

1. Resolve the workspace with `--locked` from a clean checkout.
2. Confirm that `Cargo.lock` and `cargo tree --workspace` contain no Aegis Git
   source or internal Aegis crate.
3. Run the independent IPC fixtures and daemon-level media tests.
4. Confirm that the runtime protocol mapping matches the
   [Compatibility Reference](../reference/compatibility.md).

## Verification

Run every gate from the repository root:

```bash
cargo fmt --all -- --check
cargo +1.88.0 check --locked --workspace --all-targets
cargo +1.88.0 clippy --locked --workspace --all-targets -- -D warnings
cargo clippy --locked --workspace --all-targets -- -D warnings
AEGIS_PORTAL_REQUIRE_E2E=1 \
AEGIS_PORTAL_REQUIRE_PIPEWIRE_E2E=1 \
  cargo test --locked --workspace
cargo deny check
cargo doc --locked --workspace --no-deps
cargo build --locked --release --workspace
```

The required end-to-end mode fails instead of skipping when `dbus-daemon`,
the real `xdg-desktop-portal` frontend, PipeWire, WirePlumber, or the
GStreamer PipeWire consumer is unavailable.

## Package Staging

Build and inspect both licensing variants:

```bash
meson setup build-package --wipe \
  --buildtype=release --prefix=/usr -Dpam=false
meson compile -C build-package
DESTDIR="$PWD/stage" meson install -C build-package

meson setup build-package --reconfigure -Dpam=true
meson compile -C build-package
DESTDIR="$PWD/stage-pam" meson install -C build-package
```

Confirm executable modes, the configured D-Bus `Exec` path, the portal
metadata interface list, the interface routing, and the PAM module's
distribution license before publishing artifacts.

## Release Metadata

1. Move the `CHANGELOG.md` Unreleased entries into the release version and
   date.
2. Update the workspace and Meson project versions together.
3. Update the compatibility table when the verified Aegis runtime set or IPC
   protocol changes.
4. Tag only the reviewed canonical-lockfile commit.
