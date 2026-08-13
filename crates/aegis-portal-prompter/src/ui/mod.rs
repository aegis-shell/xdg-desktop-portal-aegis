//! Shared infrastructure for the iris-hosted prompt dialogs: the run
//! wrapper, per-frame input helpers, and the small `unsafe` island every
//! raw FFI call is funneled through.

pub mod choose_app;
pub mod confirm;
pub mod edit;
pub mod file_chooser;
pub mod launcher_edit;
pub mod notify;
pub mod secret;
pub mod secret_buffer;
pub mod style;

use iris::{Application, Config, Frame, Input, PaintHost};
use lens::{key, mods};
use style::metrics;

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
    c_field_text(&input.as_raw().text_utf8)
}

/// The in-progress IME composition (preedit) reported for this frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preedit {
    /// The composition text (e.g. the pinyin being converted).
    pub text: String,
    /// Caret as a byte index into `text`, always on a char boundary.
    pub cursor: usize,
    /// The active clause as a byte range into `text` (the segment the IME
    /// highlights as the conversion target).
    pub sel: (usize, usize),
}

/// The IME's in-progress composition this frame, if any. The app-owned edit
/// surfaces render this inline (underlined) and anchor the IME's candidate
/// window at it, mirroring lens's own textfield.
pub fn preedit(input: &Input) -> Option<Preedit> {
    let raw = input.as_raw();
    let text = c_field_text(&raw.preedit_utf8);
    if text.is_empty() {
        return None;
    }
    let clamp = |offset: u32| -> usize {
        let mut offset = (offset as usize).min(text.len());
        while offset > 0 && !text.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    };
    Some(Preedit {
        cursor: clamp(raw.preedit_cursor),
        sel: (clamp(raw.preedit_sel_lo), clamp(raw.preedit_sel_hi)),
        text,
    })
}

/// The surrounding-text deletion the IME requested this frame, as byte
/// counts before and after the caret (`zwp_text_input_v3`
/// `delete_surrounding_text`); `(0, 0)` means none.
pub fn ime_delete(input: &Input) -> (u32, u32) {
    let raw = input.as_raw();
    (raw.ime_delete_before, raw.ime_delete_after)
}

/// Decode a NUL-terminated UTF-8 C field from the input snapshot.
fn c_field_text(field: &[std::ffi::c_char]) -> String {
    let len = field.iter().position(|&c| c == 0).unwrap_or(field.len());
    let bytes: Vec<u8> = field[..len].iter().map(|&c| c as u8).collect();
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

/// Wake the iris run loop from another thread so the next build callback
/// runs soon. The notification daemon's stdin reader uses this: a command
/// arriving while the window is idle must be observed without waiting for
/// unrelated input. A no-op when no run loop is active (the post is
/// dropped, and the main thread is blocked on the command channel anyway).
pub fn wake_main_thread() {
    unsafe extern "C" fn kick(_user: *mut std::ffi::c_void) {
        // Runs on the iris main thread inside the active run: scheduling
        // one frame is exactly the wake the poster wants.
        iris::request_animation_frame();
    }
    // SAFETY: `kick` is a valid trampoline ignoring its (null) user
    // pointer; posting is thread-safe per the iris contract.
    unsafe {
        iris::sys::iris_post_to_main_thread(Some(kick), std::ptr::null_mut());
    }
}

/// Truncate `text` to fit `max_width` logical pixels at body size, adding
/// an ellipsis when anything is dropped. Measurement goes through the same
/// shaping seam the labels render with.
pub fn truncate_to_width(frame: &Frame, text: &str, max_width: f32) -> String {
    const ELLIPSIS: &str = "…";
    if frame.measure_text(text, metrics::FONT_BODY).width <= max_width {
        return text.to_owned();
    }
    let budget = max_width - frame.measure_text(ELLIPSIS, metrics::FONT_BODY).width;
    let mut kept = String::new();
    for ch in text.chars() {
        if frame
            .measure_text(&format!("{kept}{ch}"), metrics::FONT_BODY)
            .width
            > budget
        {
            break;
        }
        kept.push(ch);
    }
    format!("{kept}{ELLIPSIS}")
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
