//! The secret password prompt: a centered heading, an optional reason line,
//! and a masked edit field with Cancel / Unlock buttons.
//!
//! lens has no password-masking text widget, so the field is an app-owned
//! editing surface (the pattern lens's caret/paste API is designed for):
//! the dialog owns the real secret and its caret, renders bullet glyphs
//! itself, and consumes text and editing keys straight from the per-frame
//! input snapshot. The real value is zeroized on drop and never reaches the
//! clipboard. IME compositions are masked too, so a preedit never echoes
//! the secret's content.

use aegis_portal_prompter::{PromptResult, SecretRequest, SecretResponse};
use lens::{Frame, Input, LayoutOpts, key};
use zeroize::Zeroizing;

use super::edit;
use super::style::{self, metrics};
use super::{
    close_window, display_size, escape_pressed, key_pressed, preedit, run_window, window_title,
};

struct State {
    request: SecretRequest,
    dark: bool,
    secret: Zeroizing<String>,
    /// Caret as a byte index into the secret, always on a char boundary.
    caret: usize,
    /// Whether the password surface owns typing. Starts focused, matching
    /// the former GTK dialog's `grab_focus`.
    focused: bool,
    done: Option<SecretResponse>,
}

pub fn run(request: SecretRequest) -> Result<PromptResult, String> {
    let title = window_title(&request.title, None);
    let state = State {
        dark: iris::system_prefers_dark(),
        request,
        secret: Zeroizing::new(String::new()),
        caret: 0,
        focused: true,
        done: None,
    };
    let state = run_window(&title, (440, 240), state, build)?;
    let response = state.done.unwrap_or(SecretResponse::Cancelled);
    Ok(PromptResult::Secret(response))
}

fn build(state: &mut State, f: &mut Frame, input: &Input) {
    f.set_theme(style::theme(state.dark));
    if escape_pressed(input) {
        finish(state, SecretResponse::Cancelled);
        return;
    }

    let width = display_size(input).0 - 2.0 * metrics::SPACE_L;
    f.column_ex(
        &LayoutOpts {
            gap: metrics::SPACE_M,
            pad: metrics::SPACE_L,
            ..Default::default()
        },
        |f| {
            f.push_style(style::title_style());
            f.label(&state.request.title);
            f.pop_style();

            if let Some(reason) = state
                .request
                .reason
                .as_deref()
                .filter(|reason| !reason.is_empty())
            {
                f.push_style(style::muted_style(state.dark));
                f.label_wrapped(reason, width.max(120.0));
                f.pop_style();
            }

            let composition = if state.focused { preedit(input) } else { None };
            let response = edit::edit_surface(
                f,
                state.dark,
                edit::EditSurface {
                    id: "secret-field",
                    text: &state.secret,
                    caret: state.caret,
                    placeholder: "Password",
                    focused: state.focused,
                    preedit: composition.as_ref(),
                    masked: true,
                },
            );

            // Focus follows the pointer: clicking the field focuses it,
            // clicking anywhere else unfocuses it.
            let left = lens::sys::lens_mouse_button::LENS_MOUSE_LEFT as usize;
            if response.clicked {
                state.focused = true;
            } else if input.as_raw().mouse_pressed[left] && !response.hovered {
                state.focused = false;
            }

            f.flex(1.0);
            f.spacer(0.0);

            f.row_ex(
                &LayoutOpts {
                    gap: metrics::SPACE_S,
                    cross: lens::Align::Center,
                    ..Default::default()
                },
                |f| {
                    f.flex(1.0);
                    f.spacer(0.0);

                    f.size_next(metrics::BUTTON_WIDTH, metrics::CONTROL_HEIGHT);
                    f.push_style(style::secondary_button_style(state.dark));
                    let cancel = f.button("Cancel");
                    f.pop_style();
                    if cancel && state.done.is_none() {
                        finish(state, SecretResponse::Cancelled);
                        return;
                    }

                    f.size_next(metrics::ACCEPT_WIDTH, metrics::CONTROL_HEIGHT);
                    if f.button("Unlock") && state.done.is_none() {
                        submit(state);
                    }
                },
            );
        },
    );

    if state.focused {
        // The app-owned field consumes all text input; keep lens's own
        // focus empty so a Tab-focused button cannot also fire on Return.
        f.clear_focus();
        edit_secret(state, f, input);
    }
}

/// Apply this frame's input to the owned secret.
fn edit_secret(state: &mut State, f: &mut Frame, input: &Input) {
    edit::edit_keys(&mut state.secret, &mut state.caret, f, input);
    if key_pressed(input, key::RETURN) && state.done.is_none() {
        submit(state);
    }
}

fn submit(state: &mut State) {
    let value = state.secret.to_string();
    finish(state, SecretResponse::Secret { value });
}

fn finish(state: &mut State, response: SecretResponse) {
    state.done = Some(response);
    close_window();
}
