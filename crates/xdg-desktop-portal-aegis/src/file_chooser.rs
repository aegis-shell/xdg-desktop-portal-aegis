//! `org.freedesktop.impl.portal.FileChooser` v3.
//!
//! The backend owns D-Bus translation and request lifetime. Each request is
//! rendered by a fresh `aegis-portal-prompter` child, communicating over a
//! private JSON pipe contract. No file path or directory entry crosses Aegis
//! compositor IPC, and closing the portal request terminates the child.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use aegis_portal_prompter::{
    BytePath, Choice, FileFilter, FilterRule, FilterRuleKind, PrompterRequest, PrompterResponse,
    SelectionMode, SelectionRequest, SelectionResponse,
};
use aegis_portal_runtime::{PortalResponse, RequestTracker, ResponseSender};
use zbus::zvariant::{ObjectPath, Value};

use crate::files;

const MAX_MESSAGE_BYTES: u64 = 8 * 1024 * 1024;
const PROMPTER_ENV: &str = "AEGIS_PORTAL_PROMPTER";

/// The served interface version: 3 adds the `current_file` option to
/// `SaveFile` (2 added `SaveFiles`).
pub(crate) const FILE_CHOOSER_VERSION: u32 = 3;

/// One file-chooser request handed from the bus methods to the dispatcher.
/// The request contains the complete portal semantics; no UI policy is
/// reconstructed by the worker task.
pub(crate) enum FileChooserJob {
    Choose {
        request_path: String,
        request: SelectionRequest,
        reply: ResponseSender,
    },
}

pub(crate) struct FileChooserIface {
    pub(crate) conn: zbus::Connection,
    pub(crate) tracker: Arc<Mutex<RequestTracker>>,
    pub(crate) jobs: mpsc::Sender<FileChooserJob>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.FileChooser")]
impl FileChooserIface {
    async fn open_file(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        title: &str,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        let parsed = parse_options(&options);
        let request = SelectionRequest {
            mode: if parsed.directory {
                SelectionMode::OpenDirectory
            } else {
                SelectionMode::OpenFile
            },
            app_id: app_id.to_owned(),
            title: title.to_owned(),
            accept_label: parsed.accept_label,
            modal: parsed.modal,
            parent_window: nonempty(parent_window),
            multiple: parsed.multiple,
            current_folder: parsed.current_folder,
            current_name: None,
            current_file: None,
            filters: parsed.filters,
            current_filter: parsed.current_filter,
            choices: parsed.choices,
            files: Vec::new(),
        };
        self.choose(handle, request).await
    }

    async fn save_file(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        title: &str,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        let parsed = parse_options(&options);
        let request = SelectionRequest {
            mode: SelectionMode::SaveFile,
            app_id: app_id.to_owned(),
            title: title.to_owned(),
            accept_label: parsed.accept_label,
            modal: parsed.modal,
            parent_window: nonempty(parent_window),
            multiple: false,
            current_folder: parsed.current_folder,
            current_name: parsed.current_name,
            current_file: parsed.current_file,
            filters: parsed.filters,
            current_filter: parsed.current_filter,
            choices: parsed.choices,
            files: Vec::new(),
        };
        self.choose(handle, request).await
    }

    async fn save_files(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        title: &str,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        let parsed = parse_options(&options);
        let request = SelectionRequest {
            mode: SelectionMode::SaveFiles,
            app_id: app_id.to_owned(),
            title: title.to_owned(),
            accept_label: parsed.accept_label,
            modal: parsed.modal,
            parent_window: nonempty(parent_window),
            multiple: false,
            current_folder: parsed.current_folder,
            current_name: None,
            current_file: None,
            filters: Vec::new(),
            current_filter: None,
            choices: parsed.choices,
            files: parsed.save_files,
        };
        self.choose(handle, request).await
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        FILE_CHOOSER_VERSION
    }
}

impl FileChooserIface {
    async fn choose(
        &self,
        handle: ObjectPath<'_>,
        request: SelectionRequest,
    ) -> zbus::fdo::Result<PortalResponse> {
        let path = handle.as_str().to_owned();
        log::info!(
            "portal: FileChooser for '{}' ({:?}) at {path}",
            request.app_id,
            request.mode
        );

        aegis_portal_runtime::register(&self.conn, &self.tracker, &path).await?;
        let (reply, response) = async_channel::bounded(1);
        if self
            .jobs
            .send(FileChooserJob::Choose {
                request_path: path.clone(),
                request,
                reply,
            })
            .is_err()
        {
            aegis_portal_runtime::finish(&self.conn, &self.tracker, &path).await;
            return Err(zbus::fdo::Error::Failed(
                "file chooser worker is gone".to_owned(),
            ));
        }
        let result = response.recv().await.map_err(|_| {
            zbus::fdo::Error::Failed("file chooser worker dropped its response".to_owned())
        });
        aegis_portal_runtime::finish(&self.conn, &self.tracker, &path).await;
        result
    }
}

/// Dispatch each request to its own supervised task. A modal chooser belongs
/// to one calling application and must not block unrelated portal clients or
/// delay cancellation of a queued request.
pub(crate) fn file_chooser_worker(
    rx: mpsc::Receiver<FileChooserJob>,
    tracker: Arc<Mutex<RequestTracker>>,
) {
    while let Ok(FileChooserJob::Choose {
        request_path,
        request,
        reply,
    }) = rx.recv()
    {
        let tracker = Arc::clone(&tracker);
        if let Err(error) = std::thread::Builder::new()
            .name("aegis-file-chooser".to_owned())
            .spawn(move || {
                let result = run_pick(&tracker, &request_path, request);
                let _ = reply.send_blocking(result);
            })
        {
            log::error!("portal: could not spawn FileChooser task: {error}");
        }
    }
}

fn run_pick(
    tracker: &Arc<Mutex<RequestTracker>>,
    request_path: &str,
    request: SelectionRequest,
) -> (u32, HashMap<String, Value<'static>>) {
    if tracker.lock().unwrap().was_closed(request_path) {
        return cancelled();
    }
    if let Err(error) = request.validate() {
        log::warn!("portal: invalid FileChooser request: {error}");
        return failed();
    }

    let app_id = request.app_id.clone();
    match invoke_prompter(tracker, request_path, &request) {
        Ok(response @ SelectionResponse::Selected { .. }) => {
            // Request.Close wins a race with a completed child response.
            if tracker.lock().unwrap().was_closed(request_path) {
                return cancelled();
            }
            if let Err(error) = response.validate_for(&request) {
                log::warn!("portal: invalid FileChooser response for '{app_id}': {error}");
                return failed();
            }
            let SelectionResponse::Selected {
                paths,
                current_filter,
                choices,
            } = response
            else {
                unreachable!()
            };
            let (count, results) = build_results(paths, current_filter, choices);
            log::info!("portal: FileChooser for '{app_id}' -> {count} uri(s)");
            (0, results)
        }
        Ok(SelectionResponse::Cancelled) => cancelled(),
        Ok(SelectionResponse::Failed { message }) | Err(message) => {
            log::warn!("portal: FileChooser for '{app_id}' failed: {message}");
            failed()
        }
    }
}

fn invoke_prompter(
    tracker: &Arc<Mutex<RequestTracker>>,
    request_path: &str,
    request: &SelectionRequest,
) -> Result<SelectionResponse, String> {
    let executable = prompter_executable()?;
    let mut child = Command::new(&executable)
        // A file chooser implementing the portal must never recursively call
        // the portal it is serving.
        .env("GTK_USE_PORTAL", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("could not start {}: {error}", executable.display()))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "prompter stdin was not piped".to_owned())?;
    let send_result = serde_json::to_writer(&mut stdin, &PrompterRequest::new(request.clone()))
        .map_err(|error| error.to_string())
        .and_then(|()| stdin.write_all(b"\n").map_err(|error| error.to_string()));
    if let Err(error) = send_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("could not send prompter request: {error}"));
    }
    drop(stdin);

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "prompter stdout was not piped".to_owned())?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(MAX_MESSAGE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });

    let status = loop {
        if tracker.lock().unwrap().was_closed(request_path) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Ok(SelectionResponse::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(format!("could not wait for prompter: {error}"));
            }
        }
    };
    let bytes = reader
        .join()
        .map_err(|_| "prompter response reader panicked".to_owned())?
        .map_err(|error| format!("could not read prompter response: {error}"))?;
    if bytes.len() as u64 > MAX_MESSAGE_BYTES {
        return Err("prompter response exceeds the 8 MiB process-contract limit".into());
    }
    if bytes.is_empty() {
        return Err(format!("prompter exited with {status} and no response"));
    }
    let response: PrompterResponse = serde_json::from_slice(&bytes).map_err(|error| {
        format!("prompter exited with {status} and returned invalid JSON: {error}")
    })?;
    response.into_selection()
}

fn prompter_executable() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os(PROMPTER_ENV).filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    if let Ok(current) = std::env::current_exe()
        && let Some(directory) = current.parent()
    {
        let sibling = directory.join("aegis-portal-prompter");
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    let installed = PathBuf::from("/usr/lib/aegis-portal-prompter");
    if installed.is_file() {
        return Ok(installed);
    }
    Err(format!(
        "aegis-portal-prompter was not found; set {PROMPTER_ENV} or install /usr/lib/aegis-portal-prompter"
    ))
}

fn build_results(
    paths: Vec<BytePath>,
    current_filter: Option<FileFilter>,
    choices: Vec<(String, String)>,
) -> (usize, HashMap<String, Value<'static>>) {
    let uris: Vec<String> = paths
        .into_iter()
        .filter(|path| !path.is_empty())
        .map(|path| files::file_uri(&path.to_path_buf()))
        .collect();
    let count = uris.len();
    let mut results = HashMap::from([("uris".to_owned(), Value::from(uris))]);
    if let Some(filter) = current_filter {
        results.insert(
            "current_filter".to_owned(),
            Value::Structure(zbus::zvariant::Structure::from(filter_to_wire(filter))),
        );
    }
    if !choices.is_empty() {
        results.insert("choices".to_owned(), Value::from(choices));
    }
    (count, results)
}

fn cancelled() -> (u32, HashMap<String, Value<'static>>) {
    (1, HashMap::new())
}

fn failed() -> (u32, HashMap<String, Value<'static>>) {
    (2, HashMap::new())
}

fn nonempty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

struct ParsedOptions {
    modal: bool,
    multiple: bool,
    directory: bool,
    accept_label: Option<String>,
    current_folder: Option<BytePath>,
    current_name: Option<String>,
    current_file: Option<BytePath>,
    filters: Vec<FileFilter>,
    current_filter: Option<FileFilter>,
    choices: Vec<Choice>,
    save_files: Vec<BytePath>,
}

fn parse_options(options: &HashMap<String, Value<'_>>) -> ParsedOptions {
    let get_bool = |key: &str, default| {
        options
            .get(key)
            .and_then(|value| bool::try_from(value).ok())
            .unwrap_or(default)
    };
    let get_string = |key: &str| {
        options
            .get(key)
            .and_then(|value| String::try_from(value).ok())
    };
    let get_path = |key: &str| {
        options
            .get(key)
            .and_then(|value| Vec::<u8>::try_from(value.clone()).ok())
            .and_then(byte_path)
    };
    let filters = options
        .get("filters")
        .and_then(|value| {
            Vec::<(String, Vec<(u32, String)>)>::try_from(value.try_clone().ok()?).ok()
        })
        .unwrap_or_default()
        .into_iter()
        .map(filter_from_wire)
        .collect();
    let current_filter = options
        .get("current_filter")
        .and_then(filter_from_value)
        .map(filter_from_wire);
    let choices = options
        .get("choices")
        .and_then(|value| {
            Vec::<(String, String, Vec<(String, String)>, String)>::try_from(
                value.try_clone().ok()?,
            )
            .ok()
        })
        .unwrap_or_default()
        .into_iter()
        .map(|(id, label, options, selected)| Choice {
            id,
            label,
            options,
            selected,
        })
        .collect();
    let save_files = options
        .get("files")
        .and_then(|value| Vec::<Vec<u8>>::try_from(value.try_clone().ok()?).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(byte_path)
        .collect();

    ParsedOptions {
        modal: get_bool("modal", true),
        multiple: get_bool("multiple", false),
        directory: get_bool("directory", false),
        accept_label: get_string("accept_label").filter(|label| !label.is_empty()),
        current_folder: get_path("current_folder").filter(|path| path.to_path_buf().is_absolute()),
        current_name: get_string("current_name"),
        current_file: get_path("current_file").filter(|path| path.to_path_buf().is_absolute()),
        filters,
        current_filter,
        choices,
        save_files,
    }
}

fn byte_path(mut bytes: Vec<u8>) -> Option<BytePath> {
    if bytes.last() != Some(&0) {
        return None;
    }
    bytes.pop();
    if bytes.is_empty() || bytes.contains(&0) {
        return None;
    }
    Some(BytePath(bytes))
}

fn filter_from_wire((label, rules): (String, Vec<(u32, String)>)) -> FileFilter {
    FileFilter {
        label,
        rules: rules
            .into_iter()
            .filter_map(|(kind, value)| {
                let kind = match kind {
                    0 => FilterRuleKind::Glob,
                    1 => FilterRuleKind::Mime,
                    _ => return None,
                };
                Some(FilterRule { kind, value })
            })
            .collect(),
    }
}

fn filter_from_value(value: &Value<'_>) -> Option<(String, Vec<(u32, String)>)> {
    let Value::Structure(structure) = value else {
        return None;
    };
    let [label, rules] = structure.fields() else {
        return None;
    };
    Some((
        String::try_from(label).ok()?,
        Vec::<(u32, String)>::try_from(rules.try_clone().ok()?).ok()?,
    ))
}

fn filter_to_wire(filter: FileFilter) -> (String, Vec<(u32, String)>) {
    let rules = filter
        .rules
        .into_iter()
        .map(|rule| {
            let kind = match rule.kind {
                FilterRuleKind::Glob => 0,
                FilterRuleKind::Mime => 1,
            };
            (kind, rule.value)
        })
        .collect();
    (filter.label, rules)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(pairs: &[(&str, Value<'static>)]) -> HashMap<String, Value<'static>> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect()
    }

    #[test]
    fn version_is_3() {
        assert_eq!(FILE_CHOOSER_VERSION, 3);
    }

    #[test]
    fn defaults_match_the_portal_contract() {
        let parsed = parse_options(&HashMap::new());
        assert!(parsed.modal);
        assert!(!parsed.multiple && !parsed.directory);
        assert!(parsed.filters.is_empty() && parsed.save_files.is_empty());
        assert!(parsed.current_folder.is_none() && parsed.current_file.is_none());
    }

    #[test]
    fn filters_keep_their_explicit_rule_types() {
        let rules = vec![
            (0u32, "image/*".to_owned()),
            (1u32, "not-a-slashless-mime".to_owned()),
        ];
        let parsed = parse_options(&options(&[(
            "filters",
            Value::from(vec![("Images".to_owned(), rules)]),
        )]));
        assert_eq!(parsed.filters.len(), 1);
        assert_eq!(parsed.filters[0].rules[0].kind, FilterRuleKind::Glob);
        assert_eq!(parsed.filters[0].rules[1].kind, FilterRuleKind::Mime);
    }

    #[test]
    fn path_options_require_one_trailing_nul_and_reject_interior_nuls() {
        let parsed = parse_options(&options(&[(
            "current_folder",
            Value::from(b"/tmp/docs\0".to_vec()),
        )]));
        assert_eq!(
            parsed.current_folder.unwrap().to_path_buf(),
            PathBuf::from("/tmp/docs")
        );
        let parsed = parse_options(&options(&[(
            "current_folder",
            Value::from(b"/tmp\0/docs\0".to_vec()),
        )]));
        assert!(parsed.current_folder.is_none());
        let parsed = parse_options(&options(&[(
            "current_folder",
            Value::from(b"/tmp/docs".to_vec()),
        )]));
        assert!(parsed.current_folder.is_none());
        let parsed = parse_options(&options(&[(
            "current_folder",
            Value::from(b"relative\0".to_vec()),
        )]));
        assert!(parsed.current_folder.is_none());
    }

    #[test]
    fn results_preserve_filter_types_and_real_choices() {
        let filter = FileFilter {
            label: "Images".into(),
            rules: vec![FilterRule {
                kind: FilterRuleKind::Glob,
                value: "image/*".into(),
            }],
        };
        let (_, results) = build_results(
            vec![BytePath::from_path("/tmp/a file.png")],
            Some(filter),
            vec![("encoding".into(), "utf8".into())],
        );
        let Value::Array(uris) = &results["uris"] else {
            panic!("uris must be an array");
        };
        let uri = String::try_from(uris.iter().next().unwrap()).unwrap();
        assert_eq!(uri, "file:///tmp/a%20file.png");
        assert!(results.contains_key("current_filter"));
        let Value::Array(choices) = &results["choices"] else {
            panic!("choices must be an array");
        };
        assert_eq!(choices.len(), 1);
    }
}
