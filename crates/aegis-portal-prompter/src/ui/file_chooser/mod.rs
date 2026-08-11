//! The FileChooser dialog: a lens-native file browser for the portal's
//! open/save modes, modeled on GTK's file chooser. A places
//! sidebar jumps to well-known folders, the breadcrumb bar navigates
//! ancestors, and Ctrl+L (or the pencil button, or typing `/`/`~`) opens
//! a type-a-path location field with Tab completion. The toolbar carries
//! back/forward history, parent, home, and a create-folder action.
//! Directory navigation is double-click based, arrow keys move a cursor
//! (selection follows, Ctrl+Space toggles in multiple mode), typing
//! selects by name, Enter activates, Backspace/Alt+Up walks up, Ctrl+H
//! toggles dotfiles, Enter accepts, saving over an existing file asks for
//! confirmation, and Escape cancels (closing the window cancels too).

mod model;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aegis_portal_prompter::{
    BytePath, FileChooserMode, FileChooserRequest, FileChooserResponse, FileFilter, PromptResult,
};
use lens::{Align, Color, Frame, Input, LayoutOpts, ModalOpts, TextBuf, key, mods};
use model::{
    Entry, History, Place, PlaceIcon, breadcrumbs, common_prefix, expand_tilde, list_dir,
    normalize_lexical, split_dir_tail, typeahead_index, valid_filename,
};

use super::edit;
use super::style;
use super::{
    back_icon, close_window, command_held, committed_text, computer_icon, edit_icon,
    escape_pressed, file_icon, focus_widget, folder_icon, forward_icon, home_icon, key_down,
    key_pressed, modifiers, new_folder_icon, parent_icon, raw_icon, run_window, window_title,
};

/// Double-click window for "activate" (navigate/open) gestures.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// How long typeahead characters accumulate before the buffer resets.
const TYPEAHEAD_WINDOW: Duration = Duration::from_millis(1000);
/// The dropdown ids whose open popups swallow Escape before the dialog.
const FILTER_DROPDOWN: &str = "chooser-filter";
/// The overwrite-confirmation modal id.
const OVERWRITE_MODAL: &str = "chooser-overwrite";

struct State {
    request: FileChooserRequest,
    dark: bool,
    dir: PathBuf,
    entries: Vec<Entry>,
    listing_error: Option<String>,
    selected: BTreeSet<PathBuf>,
    /// The save name (`SaveFile` only): an app-owned editing surface (see
    /// `ui::edit`), because lens's textfield cannot move its caret when
    /// the host rewrites the buffer.
    name: String,
    /// Caret as a byte index into `name`, always on a char boundary.
    name_caret: usize,
    /// Whether the save-name surface owns typing (starts focused in save
    /// mode, matching GTK's name entry).
    name_focused: bool,
    /// The filters offered in the footer dropdown (the request's filters,
    /// or the lone `current_filter` promoted to the only choice).
    filters: Vec<FileFilter>,
    filter_index: i32,
    choices: Vec<ChoiceState>,
    show_hidden: bool,
    last_click: Option<(PathBuf, Instant)>,
    /// The sidebar shortcuts to well-known folders, resolved once at start.
    places: Vec<Place>,
    /// The type-a-path field content (while `location_editing`), an
    /// app-owned editing surface like `name`.
    location: String,
    location_caret: usize,
    /// Whether the location bar is a text field instead of breadcrumbs.
    location_editing: bool,
    /// Why the last typed path was rejected, shown under the toolbar.
    location_error: Option<String>,
    /// The row the keyboard acts on (the arrow-key cursor), if any.
    focus_index: Option<usize>,
    /// Bring this row into view after the list builds.
    scroll_to_index: Option<usize>,
    /// Reset the listing scroll to the top (set on directory changes).
    scroll_top: bool,
    /// Measured entry-row stride (height + gap) for scroll-into-view math.
    row_stride: f32,
    /// Back/forward navigation stacks.
    history: History,
    /// Typeahead buffer and when the last character arrived.
    typeahead: (String, Option<Instant>),
    /// The save target awaiting overwrite confirmation, if any.
    confirm_overwrite: Option<PathBuf>,
    /// Open the overwrite modal on this frame.
    confirm_open: bool,
    /// The new-folder row is open.
    creating_folder: bool,
    folder_name: TextBuf,
    folder_error: Option<String>,
    /// Focus the new-folder field on this frame.
    folder_focus: bool,
    /// The new-folder lens text field owned keyboard input last frame.
    field_focused: bool,
    /// Whether Ctrl/Super is held this frame (multi-select modifier),
    /// sampled at the top of every build.
    ctrl_held: bool,
    reload: bool,
    done: Option<FileChooserResponse>,
}

enum ChoiceState {
    Bool(bool),
    Options(i32),
}

pub fn run(request: FileChooserRequest) -> Result<PromptResult, String> {
    let title = requested_title(&request);
    let title = window_title(&title, Some(&request.app_id));
    let mut state = State::new(request);
    state.reload_entries();
    let state = run_window(&title, (920, 540), state, build)?;
    let response = state.done.unwrap_or(FileChooserResponse::Cancelled);
    Ok(PromptResult::FileChooser(response))
}

/// The dialog title: the request's, or the mode's default when empty.
fn requested_title(request: &FileChooserRequest) -> String {
    if !request.title.is_empty() {
        return request.title.clone();
    }
    match request.mode {
        FileChooserMode::OpenFile if request.multiple => "Open Files",
        FileChooserMode::OpenFile => "Open File",
        FileChooserMode::OpenDirectory | FileChooserMode::SaveFiles => "Choose Folder",
        FileChooserMode::SaveFile => "Save File",
    }
    .to_owned()
}

/// The accept button's default label per mode.
fn default_accept_label(mode: FileChooserMode) -> &'static str {
    match mode {
        FileChooserMode::OpenFile => "Open",
        FileChooserMode::OpenDirectory | FileChooserMode::SaveFiles => "Select",
        FileChooserMode::SaveFile => "Save",
    }
}

impl State {
    fn new(request: FileChooserRequest) -> State {
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
            FileChooserMode::SaveFile => request
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
            name_caret: initial_name.len(),
            name_focused: matches!(request.mode, FileChooserMode::SaveFile),
            request,
            dark: iris::system_prefers_dark(),
            dir,
            entries: Vec::new(),
            listing_error: None,
            selected,
            name: initial_name,
            filters,
            filter_index,
            choices,
            show_hidden: false,
            last_click: None,
            places: model::places(),
            location: String::new(),
            location_caret: 0,
            location_editing: false,
            location_error: None,
            focus_index: None,
            scroll_to_index: None,
            scroll_top: true,
            row_stride: 0.0,
            history: History::default(),
            typeahead: (String::new(), None),
            confirm_overwrite: None,
            confirm_open: false,
            creating_folder: false,
            folder_name: TextBuf::new(1024, ""),
            folder_error: None,
            folder_focus: false,
            field_focused: false,
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
            FileChooserMode::OpenDirectory | FileChooserMode::SaveFiles
        )
    }

    /// Whether more than one path may be selected.
    fn multiple_allowed(&self) -> bool {
        matches!(
            self.request.mode,
            FileChooserMode::OpenFile | FileChooserMode::OpenDirectory
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
        if dir != self.dir {
            let from = std::mem::replace(&mut self.dir, dir.clone());
            self.history.push(&from, &dir);
        }
        self.after_navigate();
    }

    /// Move within the back/forward stacks (no new history entry).
    fn navigate_history(&mut self, target: PathBuf) {
        self.dir = target;
        self.after_navigate();
    }

    /// The shared aftermath of every directory change.
    fn after_navigate(&mut self) {
        self.selected.clear();
        self.last_click = None;
        self.location_editing = false;
        self.location_error = None;
        self.focus_index = None;
        self.scroll_to_index = None;
        self.scroll_top = true;
        self.typeahead.0.clear();
        self.reload = true;
    }
}

/// The paths an accept would return, or `None` when the current state is
/// not acceptable. Pure so it is testable without a window.
fn accept_paths(
    mode: FileChooserMode,
    dir: &Path,
    selected: &BTreeSet<PathBuf>,
    save_name: &str,
) -> Option<Vec<PathBuf>> {
    match mode {
        FileChooserMode::OpenFile => {
            if selected.is_empty() {
                None
            } else {
                Some(selected.iter().cloned().collect())
            }
        }
        // Choosing a folder with nothing selected targets the folder being
        // browsed, matching GTK's SelectFolder.
        FileChooserMode::OpenDirectory => Some(if selected.is_empty() {
            vec![dir.to_path_buf()]
        } else {
            selected.iter().cloned().collect()
        }),
        FileChooserMode::SaveFile => {
            let name = save_name.trim();
            if valid_filename(name) {
                Some(vec![dir.join(name)])
            } else {
                None
            }
        }
        FileChooserMode::SaveFiles => Some(vec![dir.to_path_buf()]),
    }
}

fn accept_valid(state: &State) -> bool {
    accept_paths(
        state.request.mode,
        &state.dir,
        &state.selected,
        state.name.as_str(),
    )
    .is_some()
}

fn accept(state: &mut State) {
    accept_checked(state, false);
}

/// Accept the current selection. In save mode an existing target first
/// asks for overwrite confirmation; `force` skips that check (the
/// confirmation modal's Replace button).
fn accept_checked(state: &mut State, force: bool) {
    let Some(paths) = accept_paths(
        state.request.mode,
        &state.dir,
        &state.selected,
        state.name.as_str(),
    ) else {
        return;
    };
    if !force
        && state.request.mode == FileChooserMode::SaveFile
        && paths.first().is_some_and(|path| path.exists())
    {
        state.confirm_overwrite = Some(paths[0].clone());
        state.confirm_open = true;
        return;
    }
    let result = state
        .request
        .finish_paths(paths)
        .map(|paths| FileChooserResponse::Selected {
            paths: paths.into_iter().map(BytePath::from).collect(),
            current_filter: state.active_filter().cloned(),
            choices: collect_choices(state),
        });
    finish(
        state,
        result.unwrap_or_else(|message| FileChooserResponse::Failed { message }),
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

fn finish(state: &mut State, response: FileChooserResponse) {
    state.done = Some(response);
    close_window();
}

/// Open the location field seeded with the current folder's path.
fn start_location_edit(state: &mut State) {
    let seed = state.dir.to_string_lossy().into_owned();
    open_location(state, seed);
}

/// Open the location field with `seed` as content, caret at the end.
fn open_location(state: &mut State, seed: String) {
    state.location_editing = true;
    state.location_error = None;
    state.location_caret = seed.len();
    state.location = seed;
}

/// Set the save name programmatically, caret to the end.
fn set_name(state: &mut State, name: &str) {
    state.name = name.to_owned();
    state.name_caret = state.name.len();
}

/// Resolve the typed location: a directory navigates, an existing file
/// selects (or seeds the save name), anything else reports inline why the
/// path cannot be used.
fn go_location(state: &mut State) {
    let typed = state.location.trim().to_owned();
    if typed.is_empty() {
        return;
    }
    let expanded = expand_tilde(&typed, std::env::home_dir().as_deref());
    let full = if expanded.is_absolute() {
        expanded
    } else {
        state.dir.join(&expanded)
    };
    let full = normalize_lexical(&full);
    if full.is_dir() {
        state.navigate(full);
        return;
    }
    let Some(parent) = full.parent().filter(|parent| parent.is_dir()) else {
        state.location_error = Some(format!("No such location: {}", full.display()));
        return;
    };
    if state.request.mode == FileChooserMode::SaveFile {
        // Existing file or new name alike: offer the tail as the save name.
        if let Some(name) = full.file_name() {
            let name = name.to_string_lossy().into_owned();
            state.navigate(parent.to_path_buf());
            set_name(state, &name);
        }
    } else if full.is_file() {
        state.navigate(parent.to_path_buf());
        if state.request.mode == FileChooserMode::OpenFile {
            state.selected.clear();
            state.selected.insert(full);
        }
    } else {
        state.location_error = Some(format!("No such file: {}", full.display()));
    }
}

/// Tab-complete the location field against the typed directory's entries:
/// a single match completes in full (with a trailing `/` for folders),
/// several matches complete the longest common prefix.
fn complete_location(state: &mut State) {
    let typed = state.location.clone();
    if typed.is_empty() {
        return;
    }
    let (dir_part, tail) = split_dir_tail(&typed);
    let expanded = expand_tilde(dir_part, std::env::home_dir().as_deref());
    let base = if expanded.as_os_str().is_empty() {
        state.dir.clone()
    } else if expanded.is_absolute() {
        expanded
    } else {
        state.dir.join(expanded)
    };
    let base = normalize_lexical(&base);
    let Ok(entries) = list_dir(&base, tail.starts_with('.'), None) else {
        return;
    };
    let matches: Vec<&Entry> = entries
        .iter()
        .filter(|entry| entry.name.starts_with(tail))
        .collect();
    match matches.as_slice() {
        [] => {}
        [only] => {
            let mut completed = format!("{dir_part}{}", only.name);
            if only.is_dir {
                completed.push('/');
            }
            state.location_caret = completed.len();
            state.location = completed;
        }
        many => {
            let prefix = common_prefix(many.iter().map(|entry| entry.name.as_str()));
            if prefix.len() > tail.len() {
                let completed = format!("{dir_part}{prefix}");
                state.location_caret = completed.len();
                state.location = completed;
            }
        }
    }
}

/// Create the typed folder inside the current directory and enter it.
fn create_folder(state: &mut State) {
    let name = state.folder_name.as_str().trim().to_owned();
    if !valid_filename(&name) {
        state.folder_error = Some("Enter a name without /".to_owned());
        return;
    }
    let target = state.dir.join(&name);
    match std::fs::create_dir(&target) {
        Ok(()) => {
            state.creating_folder = false;
            state.folder_error = None;
            state.navigate(target);
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            state.folder_error = Some(format!("{name} already exists"));
        }
        Err(error) => {
            state.folder_error = Some(format!("Could not create {name}: {error}"));
        }
    }
}

/// Move the keyboard cursor by `delta` rows, clamped to the listing.
fn focus_move(state: &mut State, delta: i64) {
    let len = state.entries.len() as i64;
    if len == 0 {
        return;
    }
    let next = match state.focus_index {
        None if delta < 0 => len - 1,
        None => 0,
        Some(index) => (index as i64 + delta).clamp(0, len - 1),
    };
    focus_to(state, next as usize);
}

/// Set the keyboard cursor on a row; selection follows unless Ctrl is
/// held (then only the cursor moves and Ctrl+Space toggles, like GTK).
fn focus_to(state: &mut State, index: usize) {
    state.focus_index = Some(index);
    state.scroll_to_index = Some(index);
    let Some(entry) = state.entries.get(index).cloned() else {
        return;
    };
    if state.ctrl_held {
        return;
    }
    match state.request.mode {
        FileChooserMode::OpenFile if !entry.is_dir => {
            state.selected.clear();
            state.selected.insert(entry.path.clone());
        }
        FileChooserMode::SaveFile if !entry.is_dir => {
            set_name(state, &entry.name);
        }
        FileChooserMode::OpenDirectory | FileChooserMode::SaveFiles if entry.is_dir => {
            state.selected.clear();
            state.selected.insert(entry.path.clone());
        }
        _ => {}
    }
}

/// Toggle the focused row's selection (Ctrl+Space in multiple mode).
fn toggle_focused(state: &mut State) {
    let Some(entry) = state
        .focus_index
        .and_then(|index| state.entries.get(index))
        .cloned()
    else {
        return;
    };
    if entry.is_dir && !state.dirs_selectable() {
        return;
    }
    if !entry.is_dir && state.request.mode != FileChooserMode::OpenFile {
        return;
    }
    if !state.selected.remove(&entry.path) {
        state.selected.insert(entry.path);
    }
}

/// Enter activates the keyboard cursor (navigate/open); with no cursor it
/// accepts the dialog when the current state is valid.
fn handle_enter(state: &mut State) {
    if let Some(entry) = state
        .focus_index
        .and_then(|index| state.entries.get(index))
        .cloned()
    {
        activate_entry(state, &entry);
        return;
    }
    if accept_valid(state) {
        accept(state);
    }
}

/// The double-click semantics, shared with Enter: folders open, files
/// open directly (or seed the save name).
fn activate_entry(state: &mut State, entry: &Entry) {
    if entry.is_dir {
        state.navigate(entry.path.clone());
        return;
    }
    match state.request.mode {
        FileChooserMode::OpenFile => {
            state.selected.clear();
            state.selected.insert(entry.path.clone());
            accept(state);
        }
        FileChooserMode::SaveFile => {
            set_name(state, &entry.name);
        }
        FileChooserMode::OpenDirectory | FileChooserMode::SaveFiles => {}
    }
}

/// GTK's search-as-you-type: accumulate characters for a moment and move
/// the cursor to the first entry whose name starts with the buffer.
fn typeahead(state: &mut State, text: &str) {
    let now = Instant::now();
    let fresh = state
        .typeahead
        .1
        .is_some_and(|when| now.duration_since(when) < TYPEAHEAD_WINDOW);
    if !fresh {
        state.typeahead.0.clear();
    }
    state.typeahead.0.push_str(text);
    state.typeahead.1 = Some(now);
    let prefix = state.typeahead.0.clone();
    if let Some(index) = typeahead_index(&state.entries, &prefix) {
        focus_to(state, index);
    }
}

/// A small square icon button for the location toolbar; `enabled=false`
/// draws it muted and inert.
fn icon_tool_button(
    f: &mut Frame,
    id: &str,
    dark: bool,
    enabled: bool,
    icon: impl Fn(&mut Frame),
) -> bool {
    f.size_next(28.0, 28.0);
    if !enabled {
        f.push_style(style::muted_style(dark));
    }
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
    if !enabled {
        f.pop_style();
    }
    enabled && response.clicked
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

/// All keyboard processing for the dialog, run before rendering so the
/// frame reflects the keys pressed this frame. Text fields own their keys
/// while focused; dropdowns and the overwrite modal own Escape while open.
fn handle_keys(state: &mut State, f: &mut Frame, input: &Input) {
    let popup = popup_open(state, f);
    if escape_pressed(input) {
        if state.confirm_overwrite.is_some() {
            // The overwrite modal owns the first Escape.
            f.modal_close(OVERWRITE_MODAL);
            state.confirm_overwrite = None;
        } else if !popup {
            if state.location_editing {
                state.location_editing = false;
                state.location_error = None;
            } else if state.creating_folder {
                state.creating_folder = false;
                state.folder_error = None;
            } else {
                finish(state, FileChooserResponse::Cancelled);
            }
        }
        return;
    }
    if command_held(input) && key_pressed(input, 'h' as i32) {
        state.show_hidden = !state.show_hidden;
        state.reload = true;
    }
    if command_held(input) && key_pressed(input, 'l' as i32) {
        start_location_edit(state);
    }
    if state.confirm_overwrite.is_some() {
        // The overwrite modal owns the remaining keys; Return confirms the
        // replacement (its default action), mirroring GTK.
        if key_pressed(input, key::RETURN) {
            f.modal_close(OVERWRITE_MODAL);
            state.confirm_overwrite = None;
            accept_checked(state, true);
        }
        return;
    }
    if popup {
        // Dropdowns own the remaining keys.
        return;
    }
    if state.location_editing {
        // The location surface owns the remaining keys; Tab completes and
        // Return resolves the typed path.
        edit_location(state, f, input);
        return;
    }
    if state.name_focused {
        // The save-name surface owns the remaining keys while focused.
        edit_name(state, f, input);
        return;
    }
    if state.field_focused {
        return;
    }
    if modifiers(input) & mods::ALT != 0 && key_pressed(input, key::UP) {
        // Alt+Up walks up, mirroring GTK.
        if let Some(parent) = state.dir.parent().map(Path::to_path_buf) {
            state.navigate(parent);
        }
        return;
    }
    if key_down(input, key::UP) {
        focus_move(state, -1);
    } else if key_down(input, key::DOWN) {
        focus_move(state, 1);
    } else if key_pressed(input, key::HOME) {
        focus_to(state, 0);
    } else if key_pressed(input, key::END) && !state.entries.is_empty() {
        focus_to(state, state.entries.len() - 1);
    }
    if key_pressed(input, key::RETURN) {
        handle_enter(state);
        return;
    }
    if command_held(input) && key_pressed(input, ' ' as i32) && state.multiple_allowed() {
        toggle_focused(state);
        return;
    }
    let text = committed_text(input);
    if !text.is_empty() && !command_held(input) {
        // Typing `/` or `~` opens the location field, like GTK; anything
        // else searches the listing by name.
        if text.starts_with('/') || text.starts_with('~') {
            open_location(state, text);
        } else {
            typeahead(state, &text);
        }
    }
}

/// Apply this frame's text and editing keys to the owned location path;
/// Tab completes and Return resolves the typed path.
fn edit_location(state: &mut State, f: &mut Frame, input: &Input) {
    let text = committed_text(input);
    if !text.is_empty() {
        edit::insert(&mut state.location, &mut state.location_caret, &text);
    }
    edit_keys(&mut state.location, &mut state.location_caret, f, input);
    if key_pressed(input, key::TAB) {
        complete_location(state);
    }
    if key_pressed(input, key::RETURN) {
        go_location(state);
    }
}

/// Apply this frame's text and editing keys to the owned save name;
/// Return accepts the dialog when the name is valid.
fn edit_name(state: &mut State, f: &mut Frame, input: &Input) {
    let text = committed_text(input);
    if !text.is_empty() {
        edit::insert(&mut state.name, &mut state.name_caret, &text);
    }
    edit_keys(&mut state.name, &mut state.name_caret, f, input);
    if key_pressed(input, key::RETURN) && accept_valid(state) {
        accept(state);
    }
}

/// The editing keys every app-owned surface in this dialog shares:
/// Backspace/Delete, caret arrows/Home/End, and Ctrl+V paste.
fn edit_keys(text: &mut String, caret: &mut usize, f: &mut Frame, input: &Input) {
    if key_down(input, key::BACKSPACE) {
        edit::delete_backward(text, caret);
    }
    if key_down(input, key::DELETE) {
        edit::delete_forward(text, caret);
    }
    if key_down(input, key::LEFT) {
        *caret = edit::prev_boundary(text, *caret);
    }
    if key_down(input, key::RIGHT) {
        *caret = edit::next_boundary(text, *caret);
    }
    if key_down(input, key::HOME) {
        *caret = 0;
    }
    if key_down(input, key::END) {
        *caret = text.len();
    }
    if command_held(input) && key_pressed(input, 'v' as i32) {
        f.request_paste();
    }
    if let Some(paste) = f.take_paste() {
        edit::insert(text, caret, &paste);
    }
}

/// An app-owned single-line field row: text before the caret, the caret
/// bar, text after (the secret prompt's pattern, see `ui::edit`).
fn edit_surface(
    state: &State,
    f: &mut Frame,
    id: &str,
    text: &str,
    caret: usize,
    placeholder: &str,
    focused: bool,
) -> lens::Response {
    let palette = style::palette(state.dark);
    let opts = LayoutOpts {
        height: 34.0,
        pad: 8.0,
        cross: Align::Center,
        bg: palette.field,
        border: if focused {
            palette.accent
        } else {
            palette.border
        },
        border_width: 1.0,
        radius: 8.0,
        ..Default::default()
    };
    let (response, ()) = f.pressable_row(id, "", &opts, |f, _| {
        let (before, after) = text.split_at(caret);
        if text.is_empty() {
            f.push_style(style::muted_style(state.dark));
            f.label(placeholder);
            f.pop_style();
        }
        if !before.is_empty() {
            f.label(before);
        }
        if focused {
            f.row_ex(&edit::caret_bar(palette.text), |_| {});
        }
        if !after.is_empty() {
            f.label(after);
        }
    });
    response
}

fn build(state: &mut State, f: &mut Frame, input: &Input) {
    f.set_theme(style::theme(state.dark));
    state.ctrl_held = command_held(input);
    if state.reload {
        state.reload_entries();
    }
    handle_keys(state, f, input);
    if state.done.is_some() {
        return;
    }

    let palette = style::palette(state.dark);
    let mut folder_focused = false;

    f.column_ex(
        &LayoutOpts {
            gap: 8.0,
            pad: 10.0,
            // Fill the window so the flexible listing row absorbs both the
            // slack and the deficit; an intrinsic-sized root column would
            // push the footer below the window's bottom edge on short
            // windows or long places/list content.
            flex: 1.0,
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
                    let current_dir = state.dir.clone();
                    if icon_tool_button(
                        f,
                        "go-back",
                        state.dark,
                        state.history.back().is_some(),
                        |f| back_icon(f, 16.0),
                    ) && let Some(target) = state.history.go_back(&current_dir)
                    {
                        state.navigate_history(target);
                    }
                    if icon_tool_button(
                        f,
                        "go-forward",
                        state.dark,
                        state.history.forward().is_some(),
                        |f| forward_icon(f, 16.0),
                    ) && let Some(target) = state.history.go_forward(&current_dir)
                    {
                        state.navigate_history(target);
                    }
                    if icon_tool_button(
                        f,
                        "go-parent",
                        state.dark,
                        state.dir.parent().is_some(),
                        |f| parent_icon(f, 16.0),
                    ) && let Some(parent) = state.dir.parent().map(Path::to_path_buf)
                    {
                        state.navigate(parent);
                    }
                    if icon_tool_button(f, "go-home", state.dark, true, |f| home_icon(f, 16.0))
                        && let Some(home) = std::env::home_dir()
                    {
                        state.navigate(home);
                    }
                    if icon_tool_button(f, "new-folder", state.dark, true, |f| {
                        new_folder_icon(f, 16.0)
                    }) {
                        state.creating_folder = true;
                        state.folder_focus = true;
                        state.folder_error = None;
                        state.folder_name.set("");
                        state.name_focused = false;
                    }
                    if state.location_editing {
                        f.flex(1.0);
                        let text = state.location.clone();
                        edit_surface(
                            state,
                            f,
                            "location-path",
                            &text,
                            state.location_caret,
                            "Type a path",
                            true,
                        );
                    } else {
                        breadcrumb(state, f);
                        f.flex(1.0);
                        f.spacer(0.0);
                        if icon_tool_button(f, "edit-location", state.dark, true, |f| {
                            edit_icon(f, 16.0)
                        }) {
                            start_location_edit(state);
                        }
                    }
                },
            );
            if state.location_editing
                && let Some(error) = state.location_error.clone()
            {
                f.push_style(style::muted_style(state.dark));
                f.label(&error);
                f.pop_style();
            }

            // ---- new folder ---------------------------------------------
            if state.creating_folder {
                f.row_ex(
                    &LayoutOpts {
                        gap: 8.0,
                        cross: Align::Center,
                        ..Default::default()
                    },
                    |f| {
                        f.label("Folder name:");
                        f.textfield_placeholder(
                            "folder-name",
                            &mut state.folder_name,
                            "New folder",
                        );
                        let response = f.response();
                        folder_focused = response.focused;
                        if state.folder_focus {
                            focus_widget(f, "folder-name");
                            state.folder_focus = false;
                        }
                        if response.clicked {
                            create_folder(state);
                        }
                        f.size_next(80.0, 30.0);
                        if f.button("Create") {
                            create_folder(state);
                        }
                    },
                );
                if let Some(error) = state.folder_error.clone() {
                    f.push_style(style::muted_style(state.dark));
                    f.label(&error);
                    f.pop_style();
                }
            }

            // ---- save name (SaveFile only) ------------------------------
            if state.request.mode == FileChooserMode::SaveFile {
                f.row_ex(
                    &LayoutOpts {
                        gap: 8.0,
                        cross: Align::Center,
                        ..Default::default()
                    },
                    |f| {
                        f.label("Name:");
                        f.flex(1.0);
                        let name = state.name.clone();
                        let response = edit_surface(
                            state,
                            f,
                            "save-name",
                            &name,
                            state.name_caret,
                            "File name",
                            state.name_focused,
                        );
                        // Focus follows the pointer: clicking the field
                        // focuses it, clicking anywhere else unfocuses it.
                        let left = lens::sys::lens_mouse_button::LENS_MOUSE_LEFT as usize;
                        if response.clicked {
                            state.name_focused = true;
                        } else if input.as_raw().mouse_pressed[left] && !response.hovered {
                            state.name_focused = false;
                        }
                    },
                );
            }

            // ---- places sidebar + directory listing ---------------------
            f.row_ex(
                &LayoutOpts {
                    gap: 8.0,
                    flex: 1.0,
                    ..Default::default()
                },
                |f| {
                    f.scroll("chooser-places", |f| {
                        f.column_ex(
                            &LayoutOpts {
                                width: 148.0,
                                gap: 2.0,
                                ..Default::default()
                            },
                            |f| {
                                for index in 0..state.places.len() {
                                    let place = state.places[index].clone();
                                    place_row(state, f, index, &place);
                                }
                            },
                        );
                    });
                    f.separator();
                    f.column_ex(
                        &LayoutOpts {
                            flex: 1.0,
                            ..Default::default()
                        },
                        |f| {
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
                                        if state.entries.is_empty() && state.listing_error.is_none()
                                        {
                                            f.push_style(style::muted_style(state.dark));
                                            f.label("This folder is empty");
                                            f.pop_style();
                                        }
                                        for index in 0..state.entries.len() {
                                            entry_row(state, f, index, palette);
                                        }
                                    },
                                );
                            });
                            if !state.typeahead.0.is_empty() {
                                f.push_style(style::muted_style(state.dark));
                                f.label(&format!("Search: {}", state.typeahead.0));
                                f.pop_style();
                            }
                            // Measure the row stride for scroll-into-view,
                            // then apply pending scroll requests.
                            if let Some(bounds) = f.node_bounds("entry-0") {
                                state.row_stride = bounds.h + 2.0;
                            }
                            if state.scroll_top {
                                state.scroll_top = false;
                                f.scroll_to("chooser-list", 0.0, 0.0);
                            }
                            if let Some(index) = state.scroll_to_index.take()
                                && state.row_stride > 0.0
                            {
                                let y = index as f32 * state.row_stride - 120.0;
                                f.scroll_to("chooser-list", 0.0, y.max(0.0));
                            }
                        },
                    );
                },
            );

            if state.request.mode == FileChooserMode::SaveFiles {
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
                            state.focus_index = None;
                            state.scroll_top = true;
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
                        finish(state, FileChooserResponse::Cancelled);
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
                        f.push_style(style::disabled_button_style(state.dark));
                        f.button(label);
                        f.pop_style();
                    }
                },
            );
        },
    );

    // ---- overwrite confirmation ----------------------------------------
    let was_open = f.modal_is_open(OVERWRITE_MODAL);
    if state.confirm_overwrite.is_some() && !was_open && !state.confirm_open {
        // Dismissed without an answer (click outside).
        state.confirm_overwrite = None;
    }
    if state.confirm_open {
        f.modal_open(OVERWRITE_MODAL);
        state.confirm_open = false;
    }
    if state.confirm_overwrite.is_some() && f.modal_is_open(OVERWRITE_MODAL) {
        let target_name = state
            .confirm_overwrite
            .as_ref()
            .and_then(|target| target.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        f.modal(
            OVERWRITE_MODAL,
            &ModalOpts {
                title: Some("Replace File?"),
                min_width: 340.0,
                ..Default::default()
            },
            |f| {
                f.column_ex(
                    &LayoutOpts {
                        gap: 12.0,
                        pad: 6.0,
                        ..Default::default()
                    },
                    |f| {
                        f.label(&format!(
                            "A file named \"{target_name}\" already exists. \
                             Do you want to replace it?"
                        ));
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
                                    f.modal_close(OVERWRITE_MODAL);
                                    state.confirm_overwrite = None;
                                }
                                f.size_next(96.0, 30.0);
                                if f.button("Replace") {
                                    f.modal_close(OVERWRITE_MODAL);
                                    state.confirm_overwrite = None;
                                    accept_checked(state, true);
                                }
                            },
                        );
                    },
                );
            },
        );
    }

    state.field_focused = folder_focused;
    // App-owned surfaces consume Return and caret keys; keep lens's own
    // focus empty so no lens-focused widget also reacts.
    if state.location_editing || state.name_focused {
        f.clear_focus();
    }

    // Backspace walks up one folder when no text field owns the key.
    if key_pressed(input, key::BACKSPACE)
        && !state.location_editing
        && !state.name_focused
        && !folder_focused
        && !popup_open(state, f)
        && let Some(parent) = state.dir.parent().map(Path::to_path_buf)
    {
        state.navigate(parent);
    }
}

/// The ancestor chain as clickable chips, oldest first, truncated to the
/// last four components. The current folder is filled and inert; clicking
/// an ancestor navigates to it.
fn breadcrumb(state: &mut State, f: &mut Frame) {
    let chain = breadcrumbs(&state.dir);
    let hidden = chain.len().saturating_sub(4);
    if hidden > 0 {
        f.push_style(style::muted_style(state.dark));
        f.label("…");
        f.pop_style();
    }
    let last = chain.len().saturating_sub(1);
    for (position, component) in chain.into_iter().skip(hidden).enumerate() {
        if position > 0 || hidden > 0 {
            f.push_style(style::muted_style(state.dark));
            f.label("›");
            f.pop_style();
        }
        let current = hidden + position == last;
        crumb_button(
            state,
            f,
            &component,
            position,
            current,
            palette_active(state),
        );
    }
}

/// The current accent wash, resolved from the state's theme preference.
fn palette_active(state: &State) -> Color {
    style::palette(state.dark).active
}

/// One breadcrumb segment: the root shows a drive glyph, folders show
/// their (truncated) name.
fn crumb_button(
    state: &mut State,
    f: &mut Frame,
    component: &Path,
    position: usize,
    current: bool,
    active_bg: Color,
) {
    let is_root = component.parent().is_none();
    let opts = LayoutOpts {
        gap: 4.0,
        pad: 4.0,
        cross: Align::Center,
        bg: if current {
            active_bg
        } else {
            Color::TRANSPARENT
        },
        radius: 6.0,
        ..Default::default()
    };
    let name = crumb_name(component);
    let (response, ()) = f.pressable_row(&format!("crumb-{position}"), "", &opts, |f, _| {
        if is_root {
            computer_icon(f, 14.0);
        } else {
            f.label(&name);
        }
    });
    if response.clicked && !current {
        state.navigate(component.to_path_buf());
    }
}

/// A crumb's display text: the folder's name capped at 24 chars (the root
/// crumb, drawn as an icon, would read "/").
fn crumb_name(component: &Path) -> String {
    let name = component
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".to_owned());
    if name.chars().count() > 24 {
        let truncated: String = name.chars().take(21).collect();
        format!("{truncated}…")
    } else {
        name
    }
}

/// One sidebar shortcut row, highlighted when it is the browsed folder.
fn place_row(state: &mut State, f: &mut Frame, index: usize, place: &Place) {
    let active = state.dir == place.path;
    let opts = LayoutOpts {
        gap: 8.0,
        pad: 6.0,
        cross: Align::Center,
        bg: if active {
            palette_active(state)
        } else {
            Color::TRANSPARENT
        },
        radius: 6.0,
        ..Default::default()
    };
    let (response, ()) = f.pressable_row(&format!("place-{index}"), "", &opts, |f, _| {
        place_icon(f, place.icon);
        f.label(&place.name);
    });
    if response.clicked && !active {
        state.navigate(place.path.clone());
    }
}

/// The glyph for a sidebar place.
fn place_icon(f: &mut Frame, icon: PlaceIcon) {
    use lens::sys::lens_icon_id as id;
    let icon = match icon {
        PlaceIcon::Home => return home_icon(f, 16.0),
        PlaceIcon::Computer => return computer_icon(f, 16.0),
        PlaceIcon::Desktop => id::LENS_ICON_MONITOR,
        PlaceIcon::Documents => id::LENS_ICON_FILE_TEXT,
        PlaceIcon::Downloads => id::LENS_ICON_DOWNLOAD,
        PlaceIcon::Music => id::LENS_ICON_MUSIC,
        PlaceIcon::Pictures => id::LENS_ICON_IMAGE,
        PlaceIcon::Videos => id::LENS_ICON_FILM,
    };
    raw_icon(f, icon, 16.0);
}

/// One directory-listing row: icon, name, the selection/activation
/// gestures for the mode, and the keyboard-cursor border when focused.
fn entry_row(state: &mut State, f: &mut Frame, index: usize, palette: style::Palette) {
    let entry = state.entries[index].clone();
    let selected = state.selected.contains(&entry.path);
    let focused = state.focus_index == Some(index);
    let opts = LayoutOpts {
        gap: 8.0,
        pad: 4.0,
        cross: Align::Center,
        bg: if selected {
            palette.active
        } else {
            Color::TRANSPARENT
        },
        border: if focused {
            palette.accent
        } else {
            Color::TRANSPARENT
        },
        border_width: if focused { 1.0 } else { 0.0 },
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
        handle_click(state, index, &entry);
    }
}

/// Single click moves the keyboard cursor and selects (Ctrl toggles in
/// multiple mode); double click enters a folder or opens a file directly.
fn handle_click(state: &mut State, index: usize, entry: &Entry) {
    state.focus_index = Some(index);
    let now = Instant::now();
    let double = state.last_click.as_ref().is_some_and(|(path, when)| {
        *path == entry.path && now.duration_since(*when) < DOUBLE_CLICK
    });
    state.last_click = Some((entry.path.clone(), now));

    if double {
        activate_entry(state, entry);
        return;
    }
    if entry.is_dir {
        if state.dirs_selectable() {
            select(state, &entry.path);
        }
        return;
    }

    match state.request.mode {
        FileChooserMode::OpenFile => select(state, &entry.path),
        // Clicking a file while saving offers its name, like GTK.
        FileChooserMode::SaveFile => set_name(state, &entry.name),
        FileChooserMode::OpenDirectory | FileChooserMode::SaveFiles => {}
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
        assert!(accept_paths(FileChooserMode::OpenFile, dir, &BTreeSet::new(), "").is_none());
        let selected = selected_of(&["/tmp/a.txt"]);
        assert_eq!(
            accept_paths(FileChooserMode::OpenFile, dir, &selected, ""),
            Some(vec![PathBuf::from("/tmp/a.txt")])
        );
    }

    #[test]
    fn open_directory_falls_back_to_the_browsed_folder() {
        let dir = Path::new("/tmp");
        assert_eq!(
            accept_paths(FileChooserMode::OpenDirectory, dir, &BTreeSet::new(), ""),
            Some(vec![PathBuf::from("/tmp")])
        );
    }

    #[test]
    fn save_file_rejects_empty_and_escaped_names() {
        let dir = Path::new("/tmp");
        assert!(accept_paths(FileChooserMode::SaveFile, dir, &BTreeSet::new(), "  ").is_none());
        assert!(accept_paths(FileChooserMode::SaveFile, dir, &BTreeSet::new(), "../x").is_none());
        assert_eq!(
            accept_paths(FileChooserMode::SaveFile, dir, &BTreeSet::new(), "out.txt"),
            Some(vec![PathBuf::from("/tmp/out.txt")])
        );
    }

    #[test]
    fn save_files_targets_the_browsed_folder() {
        let dir = Path::new("/tmp");
        assert_eq!(
            accept_paths(FileChooserMode::SaveFiles, dir, &BTreeSet::new(), ""),
            Some(vec![PathBuf::from("/tmp")])
        );
    }

    #[test]
    fn crumb_names_cap_long_components() {
        assert_eq!(crumb_name(Path::new("/")), "/");
        assert_eq!(crumb_name(Path::new("/home/ming")), "ming");
        let long = "a-very-long-folder-name-that-overflows";
        let shown = crumb_name(Path::new("/tmp").join(long).as_path());
        assert_eq!(shown.chars().count(), 22);
        assert!(shown.ends_with('…'));
    }
}
