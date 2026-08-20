//! Shared infrastructure for the iris-hosted prompt dialogs: the run
//! wrappers (plain and lifecycle), the texture seam for device-backed
//! previews, per-frame input helpers, and the small `unsafe` islands
//! every raw FFI call is funneled through.

pub mod choose_app;
pub mod choose_source;
pub mod confirm;
pub mod edit;
pub mod file_chooser;
pub mod launcher_edit;
pub mod notify;
pub mod secret;
pub mod secret_buffer;
pub mod style;

use std::cell::RefCell;
use std::rc::Rc;

use iris::{Application, Config, Frame, Input, PaintHost, StartHost};
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
    state: T,
    build: impl FnMut(&mut T, &mut Frame, &Input),
) -> Result<T, String> {
    run_window_with_lifecycle(title, size, state, |_, _| {}, build, |_| {})
}

/// [`run_window`] with the iris lifecycle hooks wrapped around it:
/// `on_start` runs once after iris created its device, canvas, and lens
/// context — the only point a dialog may capture the device for texture
/// uploads — and `on_stop` runs after the frame loop, before iris destroys
/// the device, so device-backed resources can be released (ADR-0045). The
/// paint callback stays unset, keeping the backend's idle frame-skip.
pub fn run_window_with_lifecycle<T>(
    title: &str,
    size: (i32, i32),
    state: T,
    on_start: impl FnOnce(&mut T, DevicePtr),
    mut build: impl FnMut(&mut T, &mut Frame, &Input),
    on_stop: impl FnOnce(&mut T),
) -> Result<T, String> {
    let config = Config::new(title.to_owned())
        .map_err(|error| format!("invalid dialog title: {error}"))?
        .app_id(APP_ID)
        .map_err(|error| format!("invalid application id: {error}"))?
        .size(size.0, size.1);

    // Iris drives start/build/stop sequentially on one thread; a RefCell
    // is enough to arbitrate the three borrows of the dialog state. The
    // lifecycle hooks are FnMut by signature but fire at most once, so
    // each one-time payload rides in an Option the call site takes.
    let state = Rc::new(RefCell::new(state));
    let start_state = state.clone();
    let stop_state = state.clone();
    let build_state = state.clone();
    let mut on_start = Some(on_start);
    let mut on_stop = Some(on_stop);
    let mut start = move |host: StartHost| -> bool {
        let device = DevicePtr::from_host(&host);
        if let Some(on_start) = on_start.take() {
            on_start(&mut start_state.borrow_mut(), device);
        }
        true
    };
    let mut stop = move |_host: StartHost| {
        if let Some(on_stop) = on_stop.take() {
            on_stop(&mut stop_state.borrow_mut());
        }
    };
    let mut frame_builder = move |frame: &mut Frame, input: &Input| {
        build(&mut build_state.borrow_mut(), frame, input);
    };

    Application::run_with_lifecycle(
        config,
        Some(&mut start),
        Some(&mut stop),
        &mut frame_builder,
        None::<fn(PaintHost)>,
    )
    .map_err(|error| format!("dialog run failed: {error}"))?;
    // The run has returned, so every callback borrow is over: exactly this
    // wrapper's clone of the state Rc remains (iris drops its callbacks
    // before returning), which `try_unwrap` hands back. A panic here would
    // mean iris kept a callback alive past the run — a binding bug, not a
    // dialog state problem — so asserting is right.
    let state = match Rc::try_unwrap(state) {
        Ok(state) => state,
        Err(_) => unreachable!("iris released its callbacks before run returned"),
    };
    Ok(state.into_inner())
}

/// A non-owning handle to the `flux_device` iris owns for this run.
///
/// Texture-backed previews need a device, and opening a second one in the
/// same process is unsupported and crashes, so the dialog borrows iris's
/// through the start lifecycle callback instead. The borrow is valid from
/// `start` (after iris created the device) to `stop` (after the frame
/// loop, before iris destroys it); never store one beyond the run.
#[derive(Clone, Copy)]
pub struct DevicePtr(*mut flux_sys::flux_device);

impl DevicePtr {
    /// Wrap the device pointer an iris [`StartHost`] handed over.
    fn from_host(host: &StartHost) -> DevicePtr {
        // The iris and flux bindings declare ABI-identical opaque
        // `flux_device` pointers, so the c_void seam is a plain cast.
        DevicePtr(host.device() as *mut flux_sys::flux_device)
    }

    /// The raw pointer, for the FFI island's texture creation.
    fn as_flux(&self) -> *mut flux_sys::flux_device {
        self.0
    }
}

/// A GPU texture created from decoded preview pixels; refcounted in C.
///
/// Lens borrows the texture for the frame it is drawn in
/// ([`draw_texture_centered`]); this handle keeps it alive between frames.
/// Dropping it after the run's `stop` callback is safe:
/// `flux_image_release` parks the Vulkan objects on the device's retire
/// queue rather than assuming the device still exists.
pub struct TextureHandle {
    raw: *mut flux_sys::flux_image,
}

impl TextureHandle {
    /// Upload `w × h` 8-bit RGBA pixels as a preview texture.
    ///
    /// The canvas image pipeline samples gamma-encoded 8-bit RGBA as sRGB
    /// with **premultiplied** alpha (`canvas_image.frag`), so the pixels
    /// must already be premultiplied; the preview pipeline does that
    /// while downsampling. RGBA8_UNORM (not *_SRGB) keeps the decode on
    /// the shader's explicit premultiplied-sRGB path.
    pub fn from_premultiplied_rgba(
        device: &DevicePtr,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Result<TextureHandle, String> {
        texture_from_premultiplied_rgba(device.as_flux(), width, height, pixels)
    }

    /// The texture's pixel dimensions.
    pub fn size(&self) -> (u32, u32) {
        // SAFETY: self.raw is a live flux_image (this handle owns one
        // reference) and both accessors are pure queries on it.
        unsafe {
            (
                flux_sys::flux_image_width(self.raw),
                flux_sys::flux_image_height(self.raw),
            )
        }
    }

    /// Duplicate the texture handle: the C side is refcounted, so a retain
    /// keeps both handles valid. Used to hand a cached texture to a frame
    /// without giving up cache ownership.
    pub(crate) fn from_retained(other: &TextureHandle) -> TextureHandle {
        // SAFETY: other.raw is a live flux_image; retain returns the same
        // pointer with its refcount bumped, transferring that reference.
        let raw = unsafe { flux_sys::flux_image_retain(other.raw) };
        TextureHandle { raw }
    }
}

impl Drop for TextureHandle {
    fn drop(&mut self) {
        // SAFETY: self.raw holds this handle's own reference, released
        // exactly once.
        unsafe { flux_sys::flux_image_release(self.raw) };
    }
}

/// Create a preview texture. The FFI island for `flux_image_create`: the
/// descriptor is fully initialized from checked arguments, and the pixel
/// bytes are copied by the callee before it returns.
fn texture_from_premultiplied_rgba(
    device: *mut flux_sys::flux_device,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> Result<TextureHandle, String> {
    if width == 0 || height == 0 {
        return Err("preview texture has a zero dimension".to_owned());
    }
    let expected = width as usize * height as usize * 4;
    if pixels.len() != expected {
        return Err(format!(
            "preview needs {expected} pixel bytes, got {}",
            pixels.len()
        ));
    }
    let desc = flux_sys::flux_image_desc {
        type_: flux_sys::flux_struct_type::FLUX_TYPE_IMAGE_DESC,
        next: std::ptr::null(),
        width,
        height,
        format: flux_sys::flux_format::FLUX_FORMAT_RGBA8_UNORM,
        initial_data: pixels.as_ptr() as *const std::ffi::c_void,
    };
    let mut out: *mut flux_sys::flux_image = std::ptr::null_mut();
    // SAFETY: device is iris's live device (the DevicePtr contract); desc
    // is fully initialized; out is a valid slot. flux_image_create copies
    // the pixel bytes before returning, so pixels may be freed after.
    let result = unsafe { flux_sys::flux_image_create(device, &desc, &mut out) };
    if result != flux_sys::flux_result::FLUX_OK {
        return Err(format!(
            "preview texture upload failed: {}",
            flux_result_string(result)
        ));
    }
    Ok(TextureHandle { raw: out })
}

/// The flux error string for a result code (static in C; best effort).
fn flux_result_string(result: flux_sys::flux_result) -> String {
    // SAFETY: flux_result_string returns a static C string for any code.
    unsafe {
        let text = flux_sys::flux_result_string(result);
        if text.is_null() {
            return "unknown flux error".to_owned();
        }
        std::ffi::CStr::from_ptr(text)
            .to_string_lossy()
            .into_owned()
    }
}

/// Draw a preview texture centered inside a `width × height` box,
/// preserving its aspect ratio. Lens containers pack the main axis from
/// the start, so [`Frame::centered`] supplies the missing centring around
/// the image widget, which itself scales the texture to the fitted size.
pub fn draw_texture_centered(frame: &mut Frame, texture: &TextureHandle, width: f32, height: f32) {
    let (tex_w, tex_h) = texture.size();
    let (w, h) = fit_within(tex_w as f32, tex_h as f32, width, height);
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    frame.centered(width, height, |f| {
        // SAFETY: texture is owned by the dialog state and stays alive
        // past this frame's render; w/h are positive finite dimensions.
        // The lens and flux bindings declare ABI-identical flux_image
        // pointers, so the cast is a plain re-type of the opaque handle.
        unsafe {
            lens::sys::lens_image(f.as_raw(), texture.raw as *mut lens::sys::flux_image, w, h);
        }
    });
}

/// The largest `(w, h)` with the source's aspect that fits inside the box.
pub fn fit_within(source_w: f32, source_h: f32, width: f32, height: f32) -> (f32, f32) {
    if source_w <= 0.0 || source_h <= 0.0 || width <= 0.0 || height <= 0.0 {
        return (0.0, 0.0);
    }
    let scale = (width / source_w).min(height / source_h);
    (source_w * scale, source_h * scale)
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
