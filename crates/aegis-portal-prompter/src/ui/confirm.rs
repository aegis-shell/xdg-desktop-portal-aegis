//! The yes/no confirmation dialog (portal Account/Access consent flows):
//! a centered heading, wrapped body text, and Cancel plus one affirmative
//! button. Escape or the window's close button cancels.

use aegis_portal_prompter::{ConfirmRequest, ConfirmResponse, PromptResult};
use lens::{Align, Frame, Input, LayoutOpts};

use super::style;
use super::{close_window, display_size, escape_pressed, run_window, window_title};

struct State {
    request: ConfirmRequest,
    dark: bool,
    done: Option<ConfirmResponse>,
}

pub fn run(request: ConfirmRequest) -> Result<PromptResult, String> {
    let title = window_title(&request.title, None);
    let state = State {
        dark: iris::system_prefers_dark(),
        request,
        done: None,
    };
    let state = run_window(&title, (460, 220), state, build)?;
    // Closing the window without answering is a cancellation, matching the
    // former GTK dialog's delete-event semantics.
    let response = state.done.unwrap_or(ConfirmResponse::Cancelled);
    Ok(PromptResult::Confirm(response))
}

fn build(state: &mut State, f: &mut Frame, input: &Input) {
    f.set_theme(style::theme(state.dark));
    if escape_pressed(input) {
        finish(state, ConfirmResponse::Cancelled);
        return;
    }

    let width = display_size(input).0 - 32.0;
    f.column_ex(
        &LayoutOpts {
            gap: 12.0,
            pad: 16.0,
            ..Default::default()
        },
        |f| {
            f.push_style(style::title_style());
            f.label(&state.request.title);
            f.pop_style();

            f.push_style(style::muted_style(state.dark));
            f.label_wrapped(&state.request.body, width.max(120.0));
            f.pop_style();

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
                    if cancel {
                        finish(state, ConfirmResponse::Cancelled);
                        return;
                    }

                    f.size_next(96.0, 30.0);
                    let accept = state
                        .request
                        .accept_label
                        .as_deref()
                        .map(style::plain_label)
                        .unwrap_or("Continue");
                    if f.button(accept) {
                        finish(state, ConfirmResponse::Confirmed);
                    }
                },
            );
        },
    );
}

fn finish(state: &mut State, response: ConfirmResponse) {
    state.done = Some(response);
    close_window();
}
