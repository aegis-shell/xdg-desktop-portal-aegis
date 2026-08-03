# Release Checklist

## Canonical Dependency State

1. Remove the ignored `.cargo/config.toml` local Aegis patch without staging
   it.
2. Resolve the exact tagged Aegis dependencies from the workspace
   `Cargo.toml`.
3. Review and commit the canonical `Cargo.lock` only after the tagged graph
   resolves.
4. Confirm that the release mapping matches the
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
metadata interface list, the GTK fallback routing, and the PAM module's
distribution license before publishing artifacts.

## Release Metadata

1. Move the `CHANGELOG.md` Unreleased entries into the release version and
   date.
2. Update the workspace and Meson project versions together.
3. Update the compatibility table if the Aegis tag changes.
4. Tag only the reviewed canonical-lockfile commit.
