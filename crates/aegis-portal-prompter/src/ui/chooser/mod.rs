//! The FileChooser dialog: a lens-native file browser for the portal's
//! open/save selection modes. Directory navigation is double-click based,
//! Ctrl+H toggles dotfiles, Backspace walks up, Enter accepts, and Escape
//! cancels (closing the window cancels too).

mod model;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aegis_portal_prompter::{
    BytePath, FileFilter, PromptResult, SelectionMode, SelectionRequest, SelectionResponse,
};
use lens::{Align, Color, Frame, Input, LayoutOpts, TextBuf, key};
use model::{Entry, breadcrumbs, list_dir};

use super::style;
use super::{
    close_window, command_held, escape_pressed, file_icon, folder_icon, home_icon, key_pressed,
    parent_icon, run_window, window_title,
};

/// Double-click window for "activate" (navigate/open) gestures.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// The dropdown ids whose open popups swallow Escape before the dialog.
const FILTER_DROPDOWN: &str = "chooser-filter";

struct State {
    request: SelectionRequest,
    dark: bool,
    dir: PathBuf,
    entries: Vec<Entry>,
    listing_error: Option<String>,
    selected: BTreeSet<PathBuf>,
    /// The save name (`SaveFile` only).
    name: TextBuf,
    /// The filters offered in the footer dropdown (the request's filters,
    /// or the lone `current_filter` promoted to the only choice).
    filters: Vec<FileFilter>,
    filter_index: i32,
    choices: Vec<ChoiceState>,
    show_hidden: bool,
    last_click: Option<(PathBuf, Instant)>,
    /// Whether Ctrl/Super is held this frame (multi-select modifier),
    /// sampled at the top of every build.
    ctrl_held: bool,
    reload: bool,
    done: Option<SelectionResponse>,
}

enum ChoiceState {
    Bool(bool),
    Options(i32),
}

pub fn run(request: SelectionRequest) -> Result<PromptResult, String> {
    let title = requested_title(&request);
    let title = window_title(&title, Some(&request.app_id));
    let mut state = State::new(request);
    state.reload_entries();
    let state = run_window(&title, (760, 500), state, build)?;
    let response = state.done.unwrap_or(SelectionResponse::Cancelled);
    Ok(PromptResult::Selection(response))
}

/// The dialog title: the request's, or the mode's default when empty.
fn requested_title(request: &SelectionRequest) -> String {
    if !request.title.is_empty() {
        return request.title.clone();
    }
    match request.mode {
        SelectionMode::OpenFile if request.multiple => "Open Files",
        SelectionMode::OpenFile => "Open File",
        SelectionMode::OpenDirectory | SelectionMode::SaveFiles => "Choose Folder",
        SelectionMode::SaveFile => "Save File",
    }
    .to_owned()
}

/// The accept button's default label per mode.
fn default_accept_label(mode: SelectionMode) -> &'static str {
    match mode {
        SelectionMode::OpenFile => "Open",
        SelectionMode::OpenDirectory | SelectionMode::SaveFiles => "Select",
        SelectionMode::SaveFile => "Save",
    }
}

impl State {
    fn new(request: SelectionRequest) -> State {
        let filters = if request.filters.is_empty() {
            request.current_filter.iter().cloned().collect()
        } else {
            request.filters.clone()
        };
        let filter_index = request
            .current_filter
            .as_ref()
            .and_then(|current| filters.iter().position(|filter| filter == current))
            .unwrap_or(0) as i32;

        let start_file = request.current_file.as_ref().map(BytePath::to_path_buf);
        let dir = start_file
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .or_else(|| request.current_folder.as_ref().map(BytePath::to_path_buf))
            .or_else(std::env::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"));
        let selected = start_file.into_iter().collect::<BTreeSet<PathBuf>>();

        let initial_name = match request.mode {
            SelectionMode::SaveFile => request
                .current_name
                .clone()
                .or_else(|| {
                    request.current_file.as_ref().and_then(|file| {
                        file.to_path_buf()
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                    })
                })
                .unwrap_or_default(),
            _ => String::new(),
        };

        let choices = request
            .choices
            .iter()
            .map(|choice| {
                if choice.options.is_empty() {
                    ChoiceState::Bool(choice.selected == "true")
                } else {
                    let index = choice
                        .options
                        .iter()
                        .position(|(id, _)| id == &choice.selected)
                        .unwrap_or(0) as i32;
                    ChoiceState::Options(index)
                }
            })
            .collect();

        State {
            request,
            dark: iris::system_prefers_dark(),
            dir,
            entries: Vec::new(),
            listing_error: None,
            selected,
            name: TextBuf::new(1024, &initial_name),
            filters,
            filter_index,
            choices,
            show_hidden: false,
            last_click: None,
            ctrl_held: false,
            reload: true,
            done: None,
        }
    }

    /// The filter currently narrowing the file list, if any.
    fn active_filter(&self) -> Option<&FileFilter> {
        self.filters.get(self.filter_index.max(0) as usize)
    }

    /// Whether directories (not just files) are valid selections.
    fn dirs_selectable(&self) -> bool {
        matches!(
            self.request.mode,
            SelectionMode::OpenDirectory | SelectionMode::SaveFiles
        )
    }

    /// Whether more than one path may be selected.
    fn multiple_allowed(&self) -> bool {
        matches!(
            self.request.mode,
            SelectionMode::OpenFile | SelectionMode::OpenDirectory
        ) && self.request.multiple
    }

    fn reload_entries(&mut self) {
        match list_dir(&self.dir, self.show_hidden, self.active_filter()) {
            Ok(entries) => {
                self.entries = entries;
                self.listing_error = None;
            }
            Err(error) => {
                self.entries = Vec::new();
                self.listing_error = Some(error);
            }
        }
        self.reload = false;
    }

    fn navigate(&mut self, dir: PathBuf) {
        self.dir = dir;
        self.selected.clear();
        self.last_click = None;
        self.reload = true;
    }
}

/// The paths an accept would return, or `None` when the current state is
/// not acceptable. Pure so it is testable without a window.
fn accept_paths(
    mode: SelectionMode,
    dir: &Path,
    selected: &BTreeSet<PathBuf>,
    save_name: &str,
) -> Option<Vec<PathBuf>> {
    match mode {
        SelectionMode::OpenFile => {
            if selected.is_empty() {
                None
            } else {
                Some(selected.iter().cloned().collect())
            }
        }
        // Choosing a folder with nothing selected targets the folder being
        // browsed, matching GTK's SelectFolder.
        SelectionMode::OpenDirectory => Some(if selected.is_empty() {
            vec![dir.to_path_buf()]
        } else {
            selected.iter().cloned().collect()
        }),
        SelectionMode::SaveFile => {
            let name = save_name.trim();
            if name.is_empty() || name.contains('/') || name.contains('\0') {
                None
            } else {
                Some(vec![dir.join(name)])
            }
        }
        SelectionMode::SaveFiles => Some(vec![dir.to_path_buf()]),
    }
}

fn accept_valid(state: &State) -> bool {
    accept_paths(
        state.request.mode,
        &state.dir,
        &state.selected,
        &state.name.as_str(),
    )
    .is_some()
}

fn accept(state: &mut State) {
    let Some(paths) = accept_paths(
        state.request.mode,
        &state.dir,
        &state.selected,
        &state.name.as_str(),
    ) else {
        return;
    };
    let result = state
        .request
        .finish_paths(paths)
        .map(|paths| SelectionResponse::Selected {
            paths: paths.into_iter().map(BytePath::from).collect(),
            current_filter: state.active_filter().cloned(),
            choices: collect_choices(state),
        });
    finish(
        state,
        result.unwrap_or_else(|message| SelectionResponse::Failed { message }),
    );
}

fn collect_choices(state: &State) -> Vec<(String, String)> {
    state
        .request
        .choices
        .iter()
        .zip(&state.choices)
        .map(|(choice, value)| {
            let selected = match value {
                ChoiceState::Bool(value) => value.to_string(),
                ChoiceState::Options(index) => choice
                    .options
                    .get((*index).max(0) as usize)
                    .map(|(id, _)| id.clone())
                    .unwrap_or_default(),
            };
            (choice.id.clone(), selected)
        })
        .collect()
}

fn finish(state: &mut State, response: SelectionResponse) {
    state.done = Some(response);
    close_window();
}

/// A small square icon button for the location toolbar.
fn icon_tool_button(f: &mut Frame, id: &str, icon: impl Fn(&mut Frame)) -> bool {
    f.size_next(28.0, 28.0);
    let (response, ()) = f.pressable_row(
        id,
        "",
        &LayoutOpts {
            pad: 4.0,
            radius: 6.0,
            ..Default::default()
        },
        |f, _| icon(f),
    );
    response.clicked
}

/// Whether a transient dropdown popup is open (it swallows Escape).
fn popup_open(state: &State, f: &mut Frame) -> bool {
    let filter_open = f.place_is_open(&format!("{FILTER_DROPDOWN}##ov"));
    filter_open
        || state
            .request
            .choices
            .iter()
            .any(|choice| f.place_is_open(&format!("choice-{}##ov", choice.id)))
}

fn build(state: &mut State, f: &mut Frame, input: &Input) {
    f.set_theme(style::theme(state.dark));
    state.ctrl_held = command_held(input);
    if state.reload {
        state.reload_entries();
    }
    if escape_pressed(input) && !popup_open(state, f) {
        finish(state, SelectionResponse::Cancelled);
        return;
    }
    if command_held(input) && key_pressed(input, 'h' as i32) {
        state.show_hidden = !state.show_hidden;
        state.reload = true;
    }

    let palette = style::palette(state.dark);
    let mut name_focused = false;

    f.column_ex(
        &LayoutOpts {
            gap: 8.0,
            pad: 10.0,
            ..Default::default()
        },
        |f| {
            // ---- location toolbar --------------------------------------
            f.row_ex(
                &LayoutOpts {
                    gap: 6.0,
                    cross: Align::Center,
                    ..Default::default()
                },
                |f| {
                    if icon_tool_button(f, "go-parent", |f| parent_icon(f, 16.0))
                        && let Some(parent) = state.dir.parent().map(Path::to_path_buf)
                    {
                        state.navigate(parent);
                    }
                    if icon_tool_button(f, "go-home", |f| home_icon(f, 16.0))
                        && let Some(home) = std::env::home_dir()
                    {
                        state.navigate(home);
                    }
                    breadcrumb(state, f);
                },
            );

            // ---- save name (SaveFile only) ------------------------------
            if state.request.mode == SelectionMode::SaveFile {
                f.row_ex(
                    &LayoutOpts {
                        gap: 8.0,
                        cross: Align::Center,
                        ..Default::default()
                    },
                    |f| {
                        f.label("Name:");
                        f.textfield_placeholder("save-name", &mut state.name, "File name");
                        let response = f.response();
                        name_focused = response.focused;
                        if response.clicked && accept_valid(state) {
                            accept(state);
                        }
                    },
                );
            }

            // ---- directory listing --------------------------------------
            f.flex(1.0);
            f.scroll("chooser-list", |f| {
                f.column_ex(
                    &LayoutOpts {
                        gap: 2.0,
                        ..Default::default()
                    },
                    |f| {
                        if let Some(error) = state.listing_error.clone() {
                            f.push_style(style::muted_style(state.dark));
                            f.label(&error);
                            f.pop_style();
                        }
                        if state.entries.is_empty() && state.listing_error.is_none() {
                            f.push_style(style::muted_style(state.dark));
                            f.label("This folder is empty");
                            f.pop_style();
                        }
                        for index in 0..state.entries.len() {
                            entry_row(state, f, index, palette.active);
                        }
                    },
                );
            });

            if state.request.mode == SelectionMode::SaveFiles {
                f.push_style(style::muted_style(state.dark));
                f.label(&format!(
                    "New files will be created in {}",
                    state.dir.display()
                ));
                f.pop_style();
            }

            // ---- embedded choices ----------------------------------------
            for index in 0..state.request.choices.len() {
                choice_row(state, f, index);
            }

            // ---- footer: filter + buttons --------------------------------
            f.row_ex(
                &LayoutOpts {
                    gap: 8.0,
                    cross: Align::Center,
                    ..Default::default()
                },
                |f| {
                    if !state.filters.is_empty() {
                        let mut labels: Vec<&str> = state
                            .filters
                            .iter()
                            .map(|filter| filter.label.as_str())
                            .collect();
                        labels.truncate(16);
                        if f.dropdown(FILTER_DROPDOWN, &mut state.filter_index, &labels) {
                            state.reload = true;
                        }
                    }

                    f.flex(1.0);
                    f.spacer(0.0);

                    f.size_next(88.0, 30.0);
                    f.push_style(style::secondary_button_style(state.dark));
                    let cancel = f.button("Cancel");
                    f.pop_style();
                    if cancel {
                        finish(state, SelectionResponse::Cancelled);
                        return;
                    }

                    f.size_next(96.0, 30.0);
                    let label = state
                        .request
                        .accept_label
                        .as_deref()
                        .map(style::plain_label)
                        .unwrap_or_else(|| default_accept_label(state.request.mode));
                    let valid = accept_valid(state);
                    if valid && state.done.is_none() && f.button(label) {
                        accept(state);
                    } else if !valid {
                        // Build the disabled-looking button anyway so the
                        // layout does not jump when it becomes valid.
                        f.push_style(style::muted_style(state.dark));
                        f.button(label);
                        f.pop_style();
                    }
                },
            );
        },
    );

    // Backspace walks up one folder when no text field owns the key.
    if key_pressed(input, key::BACKSPACE)
        && !name_focused
        && !popup_open(state, f)
        && let Some(parent) = state.dir.parent().map(Path::to_path_buf)
    {
        state.navigate(parent);
    }
}

/// The ancestor chain as clickable links, oldest first, truncated to the
/// last four components.
fn breadcrumb(state: &mut State, f: &mut Frame) {
    let chain = breadcrumbs(&state.dir);
    let hidden = chain.len().saturating_sub(4);
    if hidden > 0 {
        f.push_style(style::muted_style(state.dark));
        f.label("…");
        f.pop_style();
    }
    for (position, component) in chain.into_iter().skip(hidden).enumerate() {
        if position > 0 || hidden > 0 {
            f.push_style(style::muted_style(state.dark));
            f.label("/");
            f.pop_style();
        }
        let name = if component.parent().is_none() {
            "/".to_owned()
        } else {
            let name = component
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name.chars().count() > 24 {
                let truncated: String = name.chars().take(21).collect();
                format!("{truncated}…")
            } else {
                name
            }
        };
        if f.link(&format!("{name}##crumb-{position}")) {
            state.navigate(component);
            return;
        }
    }
}

/// One directory-listing row: icon, name, and the selection/activation
/// gestures for the mode.
fn entry_row(state: &mut State, f: &mut Frame, index: usize, selected_bg: Color) {
    let entry = state.entries[index].clone();
    let selected = state.selected.contains(&entry.path);
    let opts = LayoutOpts {
        gap: 8.0,
        pad: 4.0,
        cross: Align::Center,
        bg: if selected {
            selected_bg
        } else {
            Color::TRANSPARENT
        },
        radius: 6.0,
        ..Default::default()
    };
    let (response, ()) = f.pressable_row(&format!("entry-{index}"), "", &opts, |f, _| {
        if entry.is_dir {
            folder_icon(f, 16.0);
        } else {
            file_icon(f, 16.0);
        }
        f.label(&entry.name);
    });
    if response.clicked {
        handle_click(state, &entry);
    }
}

/// Single click selects (Ctrl toggles in multiple mode); double click
/// enters a folder or opens a file directly.
fn handle_click(state: &mut State, entry: &Entry) {
    let now = Instant::now();
    let double = state.last_click.as_ref().is_some_and(|(path, when)| {
        *path == entry.path && now.duration_since(*when) < DOUBLE_CLICK
    });
    state.last_click = Some((entry.path.clone(), now));

    if entry.is_dir {
        if double {
            state.navigate(entry.path.clone());
        } else if state.dirs_selectable() {
            select(state, &entry.path);
        }
        return;
    }

    match state.request.mode {
        SelectionMode::OpenFile => {
            if double {
                state.selected.clear();
                state.selected.insert(entry.path.clone());
                accept(state);
            } else {
                select(state, &entry.path);
            }
        }
        // Clicking a file while saving offers its name, like GTK.
        SelectionMode::SaveFile => {
            state.name.set(&entry.name);
        }
        SelectionMode::OpenDirectory | SelectionMode::SaveFiles => {}
    }
}

/// Apply a click to the selection set: Ctrl toggles in multiple mode,
/// otherwise the clicked path becomes the whole selection.
fn select(state: &mut State, path: &Path) {
    if state.multiple_allowed() {
        if command_held_click(state) {
            if !state.selected.remove(path) {
                state.selected.insert(path.to_path_buf());
            }
        } else if !(state.selected.len() == 1 && state.selected.contains(path)) {
            state.selected.clear();
            state.selected.insert(path.to_path_buf());
        }
    } else {
        state.selected.clear();
        state.selected.insert(path.to_path_buf());
    }
}

/// Whether the current click event carries the multi-select modifier.
/// Read from the pressable row's own frame input, stashed on the state by
/// the build closure (a plain bool beats threading `Input` through).
fn command_held_click(state: &State) -> bool {
    state.ctrl_held
}

/// One embedded FileChooser choice: a boolean checkbox, or a labeled
/// dropdown of option labels.
fn choice_row(state: &mut State, f: &mut Frame, index: usize) {
    let choice = state.request.choices[index].clone();
    match &mut state.choices[index] {
        ChoiceState::Bool(value) => {
            f.checkbox(&choice.label, value);
        }
        ChoiceState::Options(selected) => {
            f.row_ex(
                &LayoutOpts {
                    gap: 8.0,
                    cross: Align::Center,
                    ..Default::default()
                },
                |f| {
                    f.label(&choice.label);
                    let labels: Vec<&str> = choice
                        .options
                        .iter()
                        .map(|(_, label)| label.as_str())
                        .collect();
                    f.dropdown(&format!("choice-{}", choice.id), selected, &labels);
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selected_of(paths: &[&str]) -> BTreeSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn open_file_requires_a_selection() {
        let dir = Path::new("/tmp");
        assert!(accept_paths(SelectionMode::OpenFile, dir, &BTreeSet::new(), "").is_none());
        let selected = selected_of(&["/tmp/a.txt"]);
        assert_eq!(
            accept_paths(SelectionMode::OpenFile, dir, &selected, ""),
            Some(vec![PathBuf::from("/tmp/a.txt")])
        );
    }

    #[test]
    fn open_directory_falls_back_to_the_browsed_folder() {
        let dir = Path::new("/tmp");
        assert_eq!(
            accept_paths(SelectionMode::OpenDirectory, dir, &BTreeSet::new(), ""),
            Some(vec![PathBuf::from("/tmp")])
        );
    }

    #[test]
    fn save_file_rejects_empty_and_escaped_names() {
        let dir = Path::new("/tmp");
        assert!(accept_paths(SelectionMode::SaveFile, dir, &BTreeSet::new(), "  ").is_none());
        assert!(accept_paths(SelectionMode::SaveFile, dir, &BTreeSet::new(), "../x").is_none());
        assert_eq!(
            accept_paths(SelectionMode::SaveFile, dir, &BTreeSet::new(), "out.txt"),
            Some(vec![PathBuf::from("/tmp/out.txt")])
        );
    }

    #[test]
    fn save_files_targets_the_browsed_folder() {
        let dir = Path::new("/tmp");
        assert_eq!(
            accept_paths(SelectionMode::SaveFiles, dir, &BTreeSet::new(), ""),
            Some(vec![PathBuf::from("/tmp")])
        );
    }
}
