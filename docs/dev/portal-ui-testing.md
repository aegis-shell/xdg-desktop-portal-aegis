# Portal UI Testing

Portal interactions surface in two places: **Portal-owned prompter
windows** (iris/lens dialogs hosted by this repository) and
**compositor-owned chrome pickers** (rendered by the running Aegis session
for requests that need compositor resources). Interfaces deliberately
delegated to the GTK backend (`AppChooser`, `Notification`, and the rest
of the [Portal Support Reference](../reference/portal-support.md)) render
outside this repository and are not covered here.

## UI Surfaces

| Portal call | UI shown | UI owner |
|-------------|----------|----------|
| `FileChooser.OpenFile` / `SaveFile` | File browser with filters, choices, and save-name entry | Prompter |
| `Account.GetUserInformation` | Confirmation dialog | Prompter |
| Secret vault unlock | Masked password prompt | Prompter |
| `Screenshot.Screenshot` with `interactive=true` | Region picker | Compositor chrome |
| `Screenshot.PickColor` | Crosshair pixel picker | Compositor chrome |
| `ScreenCast` `SelectSources` | Source picker and capture consent | Compositor chrome |

## Prompter Tests

The prompter runs in three setups, from the fastest iteration loop to the
full request path.

### Prerequisites

Build the prompter once per change:

```bash
cargo build -p aegis-portal-prompter
```

The binary resolves the optics shared libraries from the sibling meson
build tree when local optics mode is active (`.cargo/config.toml`, see the
repository `AGENTS.md`), or from the system installation otherwise.

### Direct Contract Smoke Tests

The prompter is a stdin/stdout contract process: write one versioned JSON
request to standard input and it shows the real lens window; the response
JSON appears on standard output when you answer, press Escape, or close
the window. No bus, daemon, or display server setup is required.

A confirmation dialog:

```bash
printf '%s' '{"version":2,"prompt":{"kind":"confirm","request":{"title":"Smoke Test","body":"Lens UI works.","accept_label":"_Continue","modal":false,"parent_window":null}}}' \
  | ./target/debug/aegis-portal-prompter; echo
```

A secret prompt (masked editing: typing, Backspace, caret keys, Ctrl+V,
Enter to submit):

```bash
printf '%s' '{"version":2,"prompt":{"kind":"secret","request":{"title":"Unlock Keyring","reason":"dev.aegis.Test wants access."}}}' \
  | ./target/debug/aegis-portal-prompter; echo
```

A file chooser with a filter and multi-selection:

```bash
printf '%s' '{"version":2,"prompt":{"kind":"selection","request":{"mode":"open_file","app_id":"dev.aegis.Test","title":"Open File","accept_label":null,"modal":false,"parent_window":null,"multiple":true,"current_folder":null,"current_name":null,"current_file":null,"filters":[{"label":"Images","rules":[{"kind":"glob","value":"*.png"}]}],"current_filter":null,"choices":[],"files":[]}}}' \
  | ./target/debug/aegis-portal-prompter; echo
```

Selection requests also accept `open_directory` and `save_files` (the
latter requires a non-empty `files` list of suggested basenames), plus a
`save_file` mode with `current_name` and embedded `choices`. Set
`RUST_LOG=debug` to trace failures; the dialog reports a `failed`
response instead of crashing when no Wayland display is available.

### Headless End-to-End Tests

The integration suite under `crates/xdg-desktop-portal-aegis/tests/`
spawns the real daemon on a private D-Bus session and swaps the prompter
for a pipe-compatible fake, so no display participates:

```bash
AEGIS_PORTAL_REQUIRE_E2E=1 cargo test -p xdg-desktop-portal-aegis
```

The fake prompter records every request the daemon issues as
`request-N.json` in its fixture directory. Capture one of those files to
replay a realistic, backend-generated request through the real UI in the
direct setup above.

## Compositor Chrome Tests

Chrome pickers render inside the compositor, so they need a live Aegis
session; there is no headless substitute in this repository. The Aegis
repository covers the picker rendering itself with offscreen-canvas
tests — these setups validate that the *request path* reaches the chrome
and that the answer flows back.

### Full-Stack Manual Tests

Run the daemon on a private bus to keep the session clean, with
`AEGIS_PORTAL_PROMPTER` pointing at a debug prompter (the daemon locates
the prompter through that variable, see `prompter.rs`):

```bash
dbus-daemon --session --nofork --print-address=1 > /tmp/bus.addr &
export DBUS_SESSION_BUS_ADDRESS=$(cat /tmp/bus.addr)
AEGIS_PORTAL_PROMPTER=$PWD/target/debug/aegis-portal-prompter \
  RUST_LOG=info ./target/debug/xdg-desktop-portal-aegis &
```

The daemon still connects to the running compositor's IPC socket for
compositor-owned requests, so run this inside the session under test.
Issue real portal calls and interact with the window:

```bash
# Prompter: file browser and confirmation dialog
gdbus call --session -d org.freedesktop.impl.portal.desktop.aegis \
  -o /org/freedesktop/portal/desktop \
  -m org.freedesktop.impl.portal.FileChooser.OpenFile \
  "" "Open File" "{'handle_token': <'t1'>}"

gdbus call --session -d org.freedesktop.impl.portal.desktop.aegis \
  -o /org/freedesktop/portal/desktop \
  -m org.freedesktop.impl.portal.Account.GetUserInformation \
  "" "{'handle_token': <'t2'>, 'reason': <'smoke'>}"
```

```bash
# Compositor chrome: region picker and crosshair pixel picker
gdbus call --session -d org.freedesktop.impl.portal.desktop.aegis \
  -o /org/freedesktop/portal/desktop \
  -m org.freedesktop.impl.portal.Screenshot.Screenshot \
  "" "{'handle_token': <'t3'>, 'interactive': <true>}"

gdbus call --session -d org.freedesktop.impl.portal.desktop.aegis \
  -o /org/freedesktop/portal/desktop \
  -m org.freedesktop.impl.portal.Screenshot.PickColor \
  "" "{'handle_token': <'t4'>}"
```

The ScreenCast picker requires the session dance before the compositor
chrome appears at `SelectSources`:

```bash
gdbus call --session -d org.freedesktop.impl.portal.desktop.aegis \
  -o /org/freedesktop/portal/desktop \
  -m org.freedesktop.impl.portal.ScreenCast.CreateSession \
  "{'handle_token': <'s1'>, 'session_handle_token': <'s1'>}"

gdbus call --session -d org.freedesktop.impl.portal.desktop.aegis \
  -o /org/freedesktop/portal/desktop/request/... \
  -m org.freedesktop.impl.portal.ScreenCast.SelectSources \
  /org/freedesktop/portal/desktop/session/... \
  "{'handle_token': <'s2'>}"
```

Portal results arrive asynchronously as `Request.Response` signals;
observe them with
`dbus-monitor "interface='org.freedesktop.impl.portal.Request'"` while
answering the dialog. Read the session and request object paths from the
signal payloads or the daemon log.

## What Each Setup Covers

| Setup | Renders real UI | Exercises backend | Needs a display | Runs in CI |
|-------|-----------------|-------------------|-----------------|------------|
| Direct contract (prompter) | Yes | No | Yes | No |
| Headless e2e | No | Yes | No | Yes |
| Full-stack manual | Yes | Yes | Yes (live session) | No |
