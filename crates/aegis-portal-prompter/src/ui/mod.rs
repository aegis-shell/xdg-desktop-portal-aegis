//! Shared infrastructure for the iris-hosted prompt dialogs: the run
//! wrapper, per-frame input helpers, and the small `unsafe` island every
//! raw FFI call is funneled through.

pub mod confirm;
pub mod edit;
pub mod file_chooser;
pub mod secret;
pub mod style;

use iris::{Application, Config, Frame, Input, PaintHost};
use lens::{key, mods};

/// The stable desktop application id reported to the compositor.
pub const APP_ID: &str = "dev.aegis.PortalPrompter";

/// The prompt window title: the request's own title, with the verified
/// application id appended when the backend supplied one
/// (`"Open File — dev.aegis.Test"`), keeping the requesting identity
/// visible even when the application controls the dialog title.
pub fn window_title(title: &str, app_id: Option<&str>) -> String {
    match app_id {
        Some(app_id) if !app_id.is_empty() => format!("{title} — {app_id}"),
        _ => title.to_owned(),
    }
}

/// Run one prompt window until it closes and hand back the final state.
/// `build` is the per-frame chrome builder; the state usually carries a
/// `done: Option<_>` the builder fills before calling [`close_window`].
pub fn run_window<T>(
    title: &str,
    size: (i32, i32),
    mut state: T,
    mut build: impl FnMut(&mut T, &mut Frame, &Input),
) -> Result<T, String> {
    let config = Config::new(title.to_owned())
        .map_err(|error| format!("invalid dialog title: {error}"))?
        .app_id(APP_ID)
        .map_err(|error| format!("invalid application id: {error}"))?
        .size(size.0, size.1);
    Application::run(
        config,
        |frame, input| build(&mut state, frame, input),
        None::<fn(PaintHost)>,
    )
    .map_err(|error| format!("dialog run failed: {error}"))?;
    Ok(state)
}

/// Whether the user pressed Escape this frame (no repeat).
pub fn escape_pressed(input: &Input) -> bool {
    key_pressed(input, key::ESCAPE)
}

/// Whether `key` saw a press edge this frame; auto-repeat does not count.
pub fn key_pressed(input: &Input, key: i32) -> bool {
    any_key(input, key, false)
}

/// Whether `key` is down this frame, including auto-repeat edges.
pub fn key_down(input: &Input, key: i32) -> bool {
    any_key(input, key, true)
}

fn any_key(input: &Input, key: i32, with_repeat: bool) -> bool {
    let raw = input.as_raw();
    raw.keys[..raw.key_count as usize]
        .iter()
        .any(|event| event.key == key && event.pressed && (with_repeat || !event.repeat))
}

/// The active modifier bitmask (`lens::mods::*`).
pub fn modifiers(input: &Input) -> u32 {
    input.as_raw().mods
}

/// Whether Ctrl (or the platform command modifier) is held.
pub fn command_held(input: &Input) -> bool {
    modifiers(input) & (mods::CTRL | mods::SUPER) != 0
}

/// Text committed this frame (typed characters, IME results).
pub fn committed_text(input: &Input) -> String {
    let raw = input.as_raw();
    let bytes = &raw.text_utf8;
    let len = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
    let bytes: Vec<u8> = bytes[..len].iter().map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The window's current logical size.
pub fn display_size(input: &Input) -> (f32, f32) {
    let size = input.as_raw().display_size;
    (size.x, size.y)
}

/// Close the window; the run returns as if the user clicked the frame's
/// close button. Thread-affine to the run loop.
pub fn close_window() {
    // SAFETY: called from inside the per-frame build callback on the run
    // thread; outside a run it is a documented no-op.
    unsafe { iris::sys::iris_window_close() };
}

/// Move keyboard focus to the widget built with `id` as its label (e.g. to
/// focus a text field the frame it appears). The dialogs build no
/// `push_id` scopes, so the flat label resolves to the widget's id hash.
pub fn focus_widget(frame: &mut Frame, id: &str) {
    let Ok(id) = std::ffi::CString::new(id) else {
        return;
    };
    // SAFETY: the frame is live for the build callback; `id` outlives both
    // calls.
    unsafe {
        let widget = lens::sys::lens_current_id(frame.as_raw(), id.as_ptr());
        lens::sys::lens_set_focus(frame.as_raw(), widget);
    }
}

/// Draw an icon from the full C icon set that the safe `lens::Icon` enum
/// does not surface (folders, files, navigation arrows).
pub fn raw_icon(frame: &mut Frame, id: lens::sys::lens_icon_id, size: f32) {
    // SAFETY: the frame is live for the build callback; `id` is a value of
    // the generated bindgen enum.
    unsafe { lens::sys::lens_icon(frame.as_raw(), id, size) };
}

/// The "go to parent folder" glyph.
pub fn parent_icon(frame: &mut Frame, size: f32) {
    raw_icon(frame, lens::sys::lens_icon_id::LENS_ICON_ARROW_UP, size);
}

/// The folder glyph for file-list rows.
pub fn folder_icon(frame: &mut Frame, size: f32) {
    raw_icon(frame, lens::sys::lens_icon_id::LENS_ICON_FOLDER, size);
}

/// The plain file glyph for file-list rows.
pub fn file_icon(frame: &mut Frame, size: f32) {
    raw_icon(frame, lens::sys::lens_icon_id::LENS_ICON_FILE, size);
}

/// The home glyph for the location toolbar.
pub fn home_icon(frame: &mut Frame, size: f32) {
    raw_icon(frame, lens::sys::lens_icon_id::LENS_ICON_HOME, size);
}

/// The "back in history" glyph for the location toolbar.
pub fn back_icon(frame: &mut Frame, size: f32) {
    raw_icon(frame, lens::sys::lens_icon_id::LENS_ICON_ARROW_LEFT, size);
}

/// The "forward in history" glyph for the location toolbar.
pub fn forward_icon(frame: &mut Frame, size: f32) {
    raw_icon(frame, lens::sys::lens_icon_id::LENS_ICON_ARROW_RIGHT, size);
}

/// The "create folder" glyph for the location toolbar.
pub fn new_folder_icon(frame: &mut Frame, size: f32) {
    raw_icon(frame, lens::sys::lens_icon_id::LENS_ICON_FOLDER_PLUS, size);
}

/// The pencil glyph for the "type a path" toggle.
pub fn edit_icon(frame: &mut Frame, size: f32) {
    raw_icon(frame, lens::sys::lens_icon_id::LENS_ICON_EDIT_2, size);
}

/// The drive glyph for the filesystem root (breadcrumb and places).
pub fn computer_icon(frame: &mut Frame, size: f32) {
    raw_icon(frame, lens::sys::lens_icon_id::LENS_ICON_HARD_DRIVE, size);
}
