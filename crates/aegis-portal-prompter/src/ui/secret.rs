//! The secret password prompt: a centered heading, an optional reason line,
//! and a masked edit field with Cancel / Unlock buttons.
//!
//! lens has no password-masking text widget, so the field is an app-owned
//! editing surface (the pattern lens's caret/paste API is designed for):
//! the dialog owns the real secret and its caret, renders bullet glyphs
//! itself, and consumes text and editing keys straight from the per-frame
//! input snapshot. The real value is zeroized on drop and never reaches the
//! clipboard.

use aegis_portal_prompter::{PromptResult, SecretRequest, SecretResponse};
use lens::{Align, Color, Frame, Input, LayoutOpts, key};
use zeroize::Zeroizing;

use super::style;
use super::{
    close_window, command_held, committed_text, display_size, escape_pressed, key_down,
    key_pressed, run_window, window_title,
};

/// The mask glyph drawn per typed character.
const MASK: &str = "•";

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

    let width = display_size(input).0 - 32.0;
    f.column_ex(
        &LayoutOpts {
            gap: 10.0,
            pad: 16.0,
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

            let palette = style::palette(state.dark);
            let field = LayoutOpts {
                height: 34.0,
                pad: 8.0,
                cross: Align::Center,
                bg: palette.field,
                border: if state.focused {
                    palette.accent
                } else {
                    palette.border
                },
                border_width: 1.0,
                radius: 8.0,
                ..Default::default()
            };
            let (response, ()) = f.pressable_row("secret-field", "", &field, |f, _| {
                let before = state.secret[..state.caret].chars().count();
                let after = state.secret[state.caret..].chars().count();
                if before + after == 0 {
                    f.push_style(style::muted_style(state.dark));
                    f.label("Password");
                    f.pop_style();
                }
                if before > 0 {
                    f.label(&MASK.repeat(before));
                }
                if state.focused {
                    f.row_ex(&caret_bar(palette.text), |_| {});
                }
                if after > 0 {
                    f.label(&MASK.repeat(after));
                }
            });

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
                    gap: 8.0,
                    cross: Align::Center,
                    ..Default::default()
                },
                |f| {
                    f.flex(1.0);
                    f.spacer(0.0);

                    f.size_next(88.0, 30.0);
                    f.push_style(style::secondary_button_style(state.dark));
                    let cancel = f.button("Cancel");
                    f.pop_style();
                    if cancel && state.done.is_none() {
                        finish(state, SecretResponse::Cancelled);
                        return;
                    }

                    f.size_next(96.0, 30.0);
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

/// The 1.5 px caret bar between the bullet runs.
fn caret_bar(color: Color) -> LayoutOpts {
    LayoutOpts {
        width: 1.5,
        height: 18.0,
        bg: color,
        ..Default::default()
    }
}

/// Apply this frame's text and editing keys to the owned secret.
fn edit_secret(state: &mut State, f: &mut Frame, input: &Input) {
    let text = committed_text(input);
    if !text.is_empty() {
        insert(&mut state.secret, &mut state.caret, &text);
    }
    if key_down(input, key::BACKSPACE) {
        delete_backward(&mut state.secret, &mut state.caret);
    }
    if key_down(input, key::DELETE) {
        delete_forward(&mut state.secret, &mut state.caret);
    }
    if key_down(input, key::LEFT) {
        state.caret = prev_boundary(&state.secret, state.caret);
    }
    if key_down(input, key::RIGHT) {
        state.caret = next_boundary(&state.secret, state.caret);
    }
    if key_down(input, key::HOME) {
        state.caret = 0;
    }
    if key_down(input, key::END) {
        state.caret = state.secret.len();
    }
    if command_held(input) && key_pressed(input, 'v' as i32) {
        f.request_paste();
    }
    if let Some(paste) = f.take_paste() {
        insert(&mut state.secret, &mut state.caret, &paste);
    }
    if key_pressed(input, key::RETURN) && state.done.is_none() {
        submit(state);
    }
}

/// Insert text at the caret, dropping control characters (a password is a
/// single line).
fn insert(secret: &mut String, caret: &mut usize, text: &str) {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    if clean.is_empty() {
        return;
    }
    secret.insert_str(*caret, &clean);
    *caret += clean.len();
}

fn delete_backward(secret: &mut String, caret: &mut usize) {
    let start = prev_boundary(secret, *caret);
    if start < *caret {
        secret.replace_range(start..*caret, "");
        *caret = start;
    }
}

fn delete_forward(secret: &mut String, caret: &mut usize) {
    let end = next_boundary(secret, *caret);
    if end > *caret {
        secret.replace_range(*caret..end, "");
    }
}

fn prev_boundary(text: &str, index: usize) -> usize {
    text[..index]
        .char_indices()
        .next_back()
        .map_or(0, |(i, _)| i)
}

fn next_boundary(text: &str, index: usize) -> usize {
    text[index..]
        .chars()
        .next()
        .map_or(text.len(), |c| index + c.len_utf8())
}

fn submit(state: &mut State) {
    let value = state.secret.to_string();
    finish(state, SecretResponse::Secret { value });
}

fn finish(state: &mut State, response: SecretResponse) {
    state.done = Some(response);
    close_window();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_moves_the_caret_with_the_text() {
        let mut secret = String::from("ab");
        let mut caret = 1;
        insert(&mut secret, &mut caret, "xy\n");
        assert_eq!(secret, "axyb");
        assert_eq!(caret, 3);
    }

    #[test]
    fn editing_is_char_boundary_safe() {
        let mut secret = String::from("aé中");
        let mut caret = secret.len();
        delete_backward(&mut secret, &mut caret);
        assert_eq!(secret, "aé");
        delete_backward(&mut secret, &mut caret);
        assert_eq!(secret, "a");
        delete_backward(&mut secret, &mut caret);
        assert_eq!(secret, "");
        delete_backward(&mut secret, &mut caret);
        assert_eq!(secret, "");

        insert(&mut secret, &mut caret, "é中");
        let mut caret = 0;
        delete_forward(&mut secret, &mut caret);
        assert_eq!(secret, "中");
        delete_forward(&mut secret, &mut caret);
        assert_eq!(secret, "");
    }

    #[test]
    fn caret_movement_clamps_to_the_ends() {
        let secret = String::from("aé");
        assert_eq!(prev_boundary(&secret, 0), 0);
        assert_eq!(next_boundary(&secret, secret.len()), secret.len());
        assert_eq!(next_boundary(&secret, 1), 3);
        assert_eq!(prev_boundary(&secret, 3), 1);
    }
}
