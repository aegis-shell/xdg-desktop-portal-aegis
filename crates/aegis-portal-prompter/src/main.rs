//! One-shot GTK host for a portal file-selection request.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::process::ExitCode;
use std::rc::Rc;

use aegis_portal_prompter::{
    BytePath, FileFilter, FilterRuleKind, PrompterRequest, PrompterResponse, SelectionMode,
    SelectionRequest, SelectionResponse,
};
use gtk::gio;
use gtk::prelude::*;
use gtk4 as gtk;

const MAX_MESSAGE_BYTES: u64 = 8 * 1024 * 1024;

fn main() -> ExitCode {
    aegis_logging::init("info");
    let response = match read_request().and_then(run_dialog) {
        Ok(response) => response,
        Err(message) => {
            log::error!("prompter: {message}");
            SelectionResponse::Failed { message }
        }
    };
    match write_response(&PrompterResponse::new(response)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            log::error!("prompter: could not write response: {error}");
            ExitCode::FAILURE
        }
    }
}

fn read_request() -> Result<SelectionRequest, String> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(MAX_MESSAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read request: {error}"))?;
    if bytes.len() as u64 > MAX_MESSAGE_BYTES {
        return Err("request exceeds the 8 MiB process-contract limit".into());
    }
    let request: PrompterRequest =
        serde_json::from_slice(&bytes).map_err(|error| format!("invalid request JSON: {error}"))?;
    request.into_selection()
}

fn write_response(response: &PrompterResponse) -> Result<(), String> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, response)
        .map_err(|error| format!("could not encode response: {error}"))?;
    stdout
        .write_all(b"\n")
        .and_then(|()| stdout.flush())
        .map_err(|error| error.to_string())
}

fn run_dialog(request: SelectionRequest) -> Result<SelectionResponse, String> {
    gtk::init().map_err(|error| format!("GTK initialization failed: {error}"))?;
    gtk::glib::set_prgname(Some("aegis-portal-prompter"));
    gtk::glib::set_application_name("Aegis Portal Prompter");

    let action = match request.mode {
        SelectionMode::OpenFile => gtk::FileChooserAction::Open,
        SelectionMode::OpenDirectory | SelectionMode::SaveFiles => {
            gtk::FileChooserAction::SelectFolder
        }
        SelectionMode::SaveFile => gtk::FileChooserAction::Save,
    };
    let requested_title = if request.title.is_empty() {
        default_title(request.mode, request.multiple)
    } else {
        request.title.as_str()
    };
    // The frontend supplies a verified app id. Keep that identity visible
    // even when the application controls the requested dialog title.
    let title = if request.app_id.is_empty() {
        requested_title.to_owned()
    } else {
        format!("{requested_title} — {}", request.app_id)
    };
    let accept = request
        .accept_label
        .as_deref()
        .unwrap_or_else(|| default_accept_label(request.mode));
    let dialog = gtk::FileChooserDialog::new(
        Some(&title),
        None::<&gtk::Window>,
        action,
        &[
            ("_Cancel", gtk::ResponseType::Cancel),
            (accept, gtk::ResponseType::Accept),
        ],
    );
    dialog.add_css_class("aegis-prompter");
    dialog.set_modal(request.modal);
    dialog.set_select_multiple(
        matches!(
            request.mode,
            SelectionMode::OpenFile | SelectionMode::OpenDirectory
        ) && request.multiple,
    );
    dialog.set_create_folders(matches!(
        request.mode,
        SelectionMode::SaveFile | SelectionMode::SaveFiles
    ));
    dialog.set_default_response(gtk::ResponseType::Accept);
    if let Some(accept) = dialog.widget_for_response(gtk::ResponseType::Accept) {
        accept.add_css_class("suggested-action");
    }

    apply_start_location(&dialog, &request);
    let rendered_filters = apply_filters(&dialog, &request);
    apply_choices(&dialog, &request);

    // A GdkSurface exists only after realization. Importing the caller's
    // xdg-foreign/X11 handle before presentation gives the compositor the
    // correct transient relationship from the first map.
    gtk::prelude::WidgetExt::realize(&dialog);
    if let Some(parent) = request.parent_window.as_deref()
        && !parent.is_empty()
        && let Err(error) = attach_parent(&dialog, parent)
    {
        log::warn!("prompter: could not attach parent {parent:?}: {error}");
    }

    let result: Rc<RefCell<Option<SelectionResponse>>> = Rc::new(RefCell::new(None));
    let main_loop = gtk::glib::MainLoop::new(None, false);
    let response_slot = Rc::clone(&result);
    let response_loop = main_loop.clone();
    let response_request = request.clone();
    dialog.connect_response(move |chooser, response| {
        let selected = if response == gtk::ResponseType::Accept {
            collect_selection(chooser, &response_request, &rendered_filters)
        } else {
            Ok(SelectionResponse::Cancelled)
        };
        *response_slot.borrow_mut() =
            Some(selected.unwrap_or_else(|message| SelectionResponse::Failed { message }));
        chooser.destroy();
        response_loop.quit();
    });
    dialog.present();
    main_loop.run();

    result
        .borrow_mut()
        .take()
        .ok_or_else(|| "dialog closed without producing a response".into())
}

fn default_title(mode: SelectionMode, multiple: bool) -> &'static str {
    match mode {
        SelectionMode::OpenFile if multiple => "Open Files",
        SelectionMode::OpenFile => "Open File",
        SelectionMode::OpenDirectory | SelectionMode::SaveFiles => "Choose Folder",
        SelectionMode::SaveFile => "Save File",
    }
}

fn default_accept_label(mode: SelectionMode) -> &'static str {
    match mode {
        SelectionMode::OpenFile => "_Open",
        SelectionMode::OpenDirectory | SelectionMode::SaveFiles => "_Select",
        SelectionMode::SaveFile => "_Save",
    }
}

fn apply_start_location(dialog: &gtk::FileChooserDialog, request: &SelectionRequest) {
    if let Some(file) = request.current_file.as_ref() {
        let file = gio::File::for_path(file.to_path_buf());
        if let Err(error) = dialog.set_file(&file) {
            log::warn!("prompter: current_file was not applied: {error}");
        }
        return;
    }
    if let Some(folder) = request.current_folder.as_ref() {
        let folder = gio::File::for_path(folder.to_path_buf());
        if let Err(error) = dialog.set_current_folder(Some(&folder)) {
            log::warn!("prompter: current_folder was not applied: {error}");
        }
    }
    if request.mode == SelectionMode::SaveFile
        && let Some(name) = request.current_name.as_deref()
    {
        dialog.set_current_name(name);
    }
}

fn apply_filters(
    dialog: &gtk::FileChooserDialog,
    request: &SelectionRequest,
) -> Vec<(FileFilter, gtk::FileFilter)> {
    let mut filters = Vec::new();
    for filter in &request.filters {
        let widget = gtk_filter(filter);
        dialog.add_filter(&widget);
        filters.push((filter.clone(), widget));
    }
    if let Some(current) = request.current_filter.as_ref() {
        if let Some((_, widget)) = filters.iter().find(|(filter, _)| filter == current) {
            dialog.set_filter(widget);
        } else if filters.is_empty() {
            let widget = gtk_filter(current);
            dialog.add_filter(&widget);
            dialog.set_filter(&widget);
            filters.push((current.clone(), widget));
        }
    }
    if dialog.filter().is_none()
        && let Some((_, first)) = filters.first()
    {
        dialog.set_filter(first);
    }
    filters
}

fn gtk_filter(filter: &FileFilter) -> gtk::FileFilter {
    let widget = gtk::FileFilter::new();
    widget.set_name(Some(&filter.label));
    for rule in &filter.rules {
        match rule.kind {
            FilterRuleKind::Glob => widget.add_pattern(&rule.value),
            FilterRuleKind::Mime => widget.add_mime_type(&rule.value),
        }
    }
    widget
}

fn apply_choices(dialog: &gtk::FileChooserDialog, request: &SelectionRequest) {
    for choice in &request.choices {
        let options: Vec<(&str, &str)> = choice
            .options
            .iter()
            .map(|(id, label)| (id.as_str(), label.as_str()))
            .collect();
        dialog.add_choice(&choice.id, &choice.label, &options);
        let initial = if choice.selected.is_empty() {
            choice
                .options
                .first()
                .map_or("false", |(id, _)| id.as_str())
        } else {
            choice.selected.as_str()
        };
        dialog.set_choice(&choice.id, initial);
    }
}

fn collect_selection(
    dialog: &gtk::FileChooserDialog,
    request: &SelectionRequest,
    filters: &[(FileFilter, gtk::FileFilter)],
) -> Result<SelectionResponse, String> {
    let mut selected = Vec::new();
    let files = dialog.files();
    for index in 0..files.n_items() {
        let file = files
            .item(index)
            .and_downcast::<gio::File>()
            .ok_or_else(|| "GTK returned a non-file selection object".to_owned())?;
        let path = file
            .path()
            .ok_or_else(|| "FileChooser returned a non-local URI".to_owned())?;
        selected.push(path);
    }
    if selected.is_empty() {
        return Err("FileChooser accepted without a local selection".into());
    }
    let selected = request.finish_paths(selected)?;
    let current_filter = dialog.filter().and_then(|active| {
        filters
            .iter()
            .find(|(_, widget)| widget == &active)
            .map(|(filter, _)| filter.clone())
    });
    let choices = request
        .choices
        .iter()
        .map(|choice| {
            let selected = dialog
                .choice(&choice.id)
                .map(|value| value.to_string())
                .unwrap_or_else(|| {
                    if choice.options.is_empty() {
                        "false".into()
                    } else {
                        String::new()
                    }
                });
            (choice.id.clone(), selected)
        })
        .collect();
    Ok(SelectionResponse::Selected {
        paths: selected.into_iter().map(BytePath::from).collect(),
        current_filter,
        choices,
    })
}

fn attach_parent(dialog: &gtk::FileChooserDialog, identifier: &str) -> Result<(), String> {
    let surface = dialog
        .surface()
        .ok_or_else(|| "dialog has no realized GDK surface".to_owned())?;
    if let Some(handle) = identifier.strip_prefix("wayland:") {
        let toplevel = surface
            .downcast::<gdk4_wayland::WaylandToplevel>()
            .map_err(|_| "Wayland parent supplied on a non-Wayland display".to_owned())?;
        if toplevel.set_transient_for_exported(handle) {
            return Ok(());
        }
        return Err("the compositor rejected the xdg-foreign parent handle".into());
    }
    Err("only Wayland xdg-foreign parent handles are valid in an Aegis session".into())
}
