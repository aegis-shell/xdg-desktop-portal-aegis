//! Stable process contract for the portal's one-shot prompter.
//!
//! The contract uses JSON over anonymous pipes. Paths are byte arrays rather
//! than UTF-8 strings so every Unix filename accepted by the FileChooser
//! portal round-trips without loss.

#![forbid(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

/// Version of the private stdin/stdout contract. The backend and prompter
/// reject mismatches instead of interpreting fields using different schemas.
pub const PROCESS_CONTRACT_VERSION: u32 = 3;

/// Versioned wire envelope sent to a prompter process.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrompterRequest {
    pub version: u32,
    pub prompt: PromptRequest,
}

impl PrompterRequest {
    #[must_use]
    pub fn new(request: FileChooserRequest) -> Self {
        Self::file_chooser(request)
    }

    #[must_use]
    pub fn file_chooser(request: FileChooserRequest) -> Self {
        Self {
            version: PROCESS_CONTRACT_VERSION,
            prompt: PromptRequest::FileChooser(request),
        }
    }

    #[must_use]
    pub fn confirm(confirm: ConfirmRequest) -> Self {
        Self {
            version: PROCESS_CONTRACT_VERSION,
            prompt: PromptRequest::Confirm(confirm),
        }
    }

    #[must_use]
    pub fn secret(secret: SecretRequest) -> Self {
        Self {
            version: PROCESS_CONTRACT_VERSION,
            prompt: PromptRequest::Secret(secret),
        }
    }

    pub fn into_prompt(self) -> Result<PromptRequest, String> {
        if self.version != PROCESS_CONTRACT_VERSION {
            return Err(format!(
                "unsupported prompter request version {}; expected {}",
                self.version, PROCESS_CONTRACT_VERSION
            ));
        }
        self.prompt.validate()?;
        Ok(self.prompt)
    }

    pub fn into_file_chooser(self) -> Result<FileChooserRequest, String> {
        match self.into_prompt()? {
            PromptRequest::FileChooser(request) => Ok(request),
            _ => Err("prompter request is not a file chooser request".into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "request", rename_all = "snake_case")]
pub enum PromptRequest {
    FileChooser(FileChooserRequest),
    Confirm(ConfirmRequest),
    Secret(SecretRequest),
}

impl PromptRequest {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::FileChooser(request) => request.validate(),
            Self::Confirm(confirm) => confirm.validate(),
            Self::Secret(secret) => secret.validate(),
        }
    }
}

/// Versioned wire envelope returned by a prompter process.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrompterResponse {
    pub version: u32,
    pub result: PromptResult,
}

impl PrompterResponse {
    #[must_use]
    pub fn new(request: FileChooserResponse) -> Self {
        Self {
            version: PROCESS_CONTRACT_VERSION,
            result: PromptResult::FileChooser(request),
        }
    }

    #[must_use]
    pub fn confirm(confirm: ConfirmResponse) -> Self {
        Self {
            version: PROCESS_CONTRACT_VERSION,
            result: PromptResult::Confirm(confirm),
        }
    }

    #[must_use]
    pub fn secret(secret: SecretResponse) -> Self {
        Self {
            version: PROCESS_CONTRACT_VERSION,
            result: PromptResult::Secret(secret),
        }
    }

    #[must_use]
    pub fn failed(message: String) -> Self {
        Self {
            version: PROCESS_CONTRACT_VERSION,
            result: PromptResult::Failed { message },
        }
    }

    pub fn into_result(self) -> Result<PromptResult, String> {
        if self.version != PROCESS_CONTRACT_VERSION {
            return Err(format!(
                "unsupported prompter response version {}; expected {}",
                self.version, PROCESS_CONTRACT_VERSION
            ));
        }
        Ok(self.result)
    }

    pub fn into_file_chooser(self) -> Result<FileChooserResponse, String> {
        match self.into_result()? {
            PromptResult::FileChooser(request) => Ok(request),
            PromptResult::Failed { message } => Err(message),
            _ => Err("prompter response is not a file chooser response".into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "response", rename_all = "snake_case")]
pub enum PromptResult {
    FileChooser(FileChooserResponse),
    Confirm(ConfirmResponse),
    Secret(SecretResponse),
    Failed { message: String },
}

const MAX_PROMPT_TEXT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmRequest {
    pub title: String,
    pub body: String,
    pub accept_label: Option<String>,
    pub modal: bool,
    pub parent_window: Option<String>,
}

impl ConfirmRequest {
    fn validate(&self) -> Result<(), String> {
        validate_prompt_text("confirmation title", &self.title, false)?;
        validate_prompt_text("confirmation body", &self.body, false)?;
        if let Some(label) = self.accept_label.as_deref() {
            validate_prompt_text("confirmation accept label", label, false)?;
        }
        if let Some(parent) = self.parent_window.as_deref() {
            validate_prompt_text("confirmation parent window", parent, true)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ConfirmResponse {
    Confirmed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRequest {
    pub title: String,
    pub reason: Option<String>,
}

impl SecretRequest {
    fn validate(&self) -> Result<(), String> {
        validate_prompt_text("secret title", &self.title, false)?;
        if let Some(reason) = self.reason.as_deref() {
            validate_prompt_text("secret reason", reason, true)?;
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SecretResponse {
    Secret { value: String },
    Cancelled,
}

impl SecretResponse {
    #[must_use]
    pub fn take_value(&mut self) -> Option<String> {
        match self {
            Self::Secret { value } => Some(std::mem::take(value)),
            Self::Cancelled => None,
        }
    }
}

impl std::fmt::Debug for SecretResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Secret { .. } => formatter.write_str("SecretResponse::Secret([REDACTED])"),
            Self::Cancelled => formatter.write_str("SecretResponse::Cancelled"),
        }
    }
}

impl Drop for SecretResponse {
    fn drop(&mut self) {
        use zeroize::Zeroize as _;
        if let Self::Secret { value } = self {
            value.zeroize();
        }
    }
}

fn validate_prompt_text(name: &str, value: &str, allow_empty: bool) -> Result<(), String> {
    if (!allow_empty && value.trim().is_empty())
        || value.len() > MAX_PROMPT_TEXT_BYTES
        || value.chars().any(|character| character == '\0')
    {
        return Err(format!("{name} is empty, oversized, or contains NUL"));
    }
    Ok(())
}

/// One filesystem path encoded as its native Unix bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct BytePath(pub Vec<u8>);

impl BytePath {
    #[must_use]
    pub fn from_path(path: impl AsRef<Path>) -> Self {
        Self(path.as_ref().as_os_str().as_bytes().to_vec())
    }

    #[must_use]
    pub fn to_path_buf(&self) -> PathBuf {
        PathBuf::from(OsString::from_vec(self.0.clone()))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<PathBuf> for BytePath {
    fn from(path: PathBuf) -> Self {
        Self::from_path(path)
    }
}

/// The FileChooser operation represented by one prompter process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChooserMode {
    OpenFile,
    OpenDirectory,
    SaveFile,
    SaveFiles,
}

/// The two rule kinds in the portal's `(sa(us))` filter structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterRuleKind {
    Glob,
    Mime,
}

/// One typed file-filter rule. The rule kind is never inferred from text.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FilterRule {
    pub kind: FilterRuleKind,
    pub value: String,
}

/// One user-visible file filter.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileFilter {
    pub label: String,
    pub rules: Vec<FilterRule>,
}

/// One optional control embedded in a FileChooser request.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Choice {
    pub id: String,
    pub label: String,
    /// Empty means a boolean check button whose values are `true`/`false`.
    pub options: Vec<(String, String)>,
    pub selected: String,
}

/// One complete request sent from the D-Bus backend to the prompter.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileChooserRequest {
    pub mode: FileChooserMode,
    pub app_id: String,
    pub title: String,
    pub accept_label: Option<String>,
    pub modal: bool,
    pub parent_window: Option<String>,
    pub multiple: bool,
    pub current_folder: Option<BytePath>,
    pub current_name: Option<String>,
    pub current_file: Option<BytePath>,
    pub filters: Vec<FileFilter>,
    pub current_filter: Option<FileFilter>,
    pub choices: Vec<Choice>,
    /// Suggested basenames for `SaveFiles`, in request order.
    pub files: Vec<BytePath>,
}

impl FileChooserRequest {
    /// Reject malformed values before any dialog or filesystem access.
    pub fn validate(&self) -> Result<(), String> {
        for (name, path) in [
            ("current_folder", self.current_folder.as_ref()),
            ("current_file", self.current_file.as_ref()),
        ] {
            if let Some(path) = path {
                validate_absolute_path(name, &path.to_path_buf())?;
            }
        }
        if self.mode != FileChooserMode::SaveFiles && !self.files.is_empty() {
            return Err("suggested files are valid only for SaveFiles".into());
        }
        if self.mode == FileChooserMode::SaveFiles && self.files.is_empty() {
            return Err("SaveFiles requires at least one suggested basename".into());
        }
        for name in &self.files {
            validate_basename(&name.to_path_buf())?;
        }
        for filter in self.filters.iter().chain(self.current_filter.as_ref()) {
            if filter.label.is_empty() || filter.rules.iter().any(|rule| rule.value.is_empty()) {
                return Err("filter labels and rules must not be empty".into());
            }
        }
        let mut ids = std::collections::BTreeSet::new();
        for choice in &self.choices {
            if choice.id.is_empty() || choice.label.is_empty() {
                return Err("choice ids and labels must not be empty".into());
            }
            if !ids.insert(choice.id.as_str()) {
                return Err(format!("duplicate choice id {:?}", choice.id));
            }
            if choice
                .options
                .iter()
                .any(|(id, label)| id.is_empty() || label.is_empty())
            {
                return Err(format!("choice {:?} contains an empty option", choice.id));
            }
            let mut option_ids = std::collections::BTreeSet::new();
            if choice
                .options
                .iter()
                .any(|(id, _)| !option_ids.insert(id.as_str()))
            {
                return Err(format!("choice {:?} has duplicate option ids", choice.id));
            }
            if choice.options.is_empty() {
                if !matches!(choice.selected.as_str(), "" | "true" | "false") {
                    return Err(format!(
                        "boolean choice {:?} has invalid value {:?}",
                        choice.id, choice.selected
                    ));
                }
            } else if !choice.selected.is_empty()
                && !choice.options.iter().any(|(id, _)| id == &choice.selected)
            {
                return Err(format!(
                    "choice {:?} selects unknown option {:?}",
                    choice.id, choice.selected
                ));
            }
        }
        Ok(())
    }

    /// Apply `SaveFiles` basename and collision semantics to the selected
    /// folder. Other modes return the selected paths unchanged.
    pub fn finish_paths(&self, selected: Vec<PathBuf>) -> Result<Vec<PathBuf>, String> {
        if self.mode != FileChooserMode::SaveFiles {
            return Ok(selected);
        }
        let folder = selected
            .into_iter()
            .next()
            .ok_or_else(|| "SaveFiles returned no selected folder".to_owned())?;
        let mut reserved = std::collections::HashSet::new();
        let mut paths = Vec::with_capacity(self.files.len());
        for name in &self.files {
            let path = unique_child(&folder, &name.to_path_buf(), &reserved)?;
            reserved.insert(path.clone());
            paths.push(path);
        }
        Ok(paths)
    }
}

/// The one response emitted by a prompter process.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FileChooserResponse {
    Selected {
        paths: Vec<BytePath>,
        current_filter: Option<FileFilter>,
        choices: Vec<(String, String)>,
    },
    Cancelled,
    Failed {
        message: String,
    },
}

impl FileChooserResponse {
    /// Validate the child result against the exact request before exposing it
    /// as a portal response. The prompter is a fault boundary, not a trusted
    /// source of arbitrarily shaped paths or choice values.
    pub fn validate_for(&self, request: &FileChooserRequest) -> Result<(), String> {
        let Self::Selected {
            paths,
            current_filter,
            choices,
        } = self
        else {
            return Ok(());
        };

        let expected_paths = match request.mode {
            FileChooserMode::SaveFile => Some(1),
            FileChooserMode::SaveFiles => Some(request.files.len()),
            FileChooserMode::OpenFile | FileChooserMode::OpenDirectory if !request.multiple => {
                Some(1)
            }
            FileChooserMode::OpenFile | FileChooserMode::OpenDirectory => None,
        };
        if paths.is_empty() || expected_paths.is_some_and(|expected| paths.len() != expected) {
            return Err(format!(
                "prompter returned {} path(s), incompatible with {:?}",
                paths.len(),
                request.mode
            ));
        }
        for path in paths {
            let path = path.to_path_buf();
            if !path.is_absolute() || path.as_os_str().as_bytes().contains(&0) {
                return Err(format!("prompter returned an invalid local path {path:?}"));
            }
        }

        if let Some(filter) = current_filter {
            let offered = request.filters.iter().any(|candidate| candidate == filter)
                || (request.filters.is_empty() && request.current_filter.as_ref() == Some(filter));
            if !offered {
                return Err("prompter returned a filter that was not offered".into());
            }
        }

        if choices.len() != request.choices.len() {
            return Err("prompter returned the wrong number of choices".into());
        }
        for ((id, selected), requested) in choices.iter().zip(&request.choices) {
            if id != &requested.id {
                return Err(format!("prompter returned unexpected choice id {id:?}"));
            }
            let valid = if requested.options.is_empty() {
                matches!(selected.as_str(), "true" | "false")
            } else {
                requested.options.iter().any(|(value, _)| value == selected)
            };
            if !valid {
                return Err(format!(
                    "prompter returned invalid value {selected:?} for choice {id:?}"
                ));
            }
        }
        Ok(())
    }
}

fn validate_basename(path: &Path) -> Result<(), String> {
    if path.as_os_str().as_bytes().contains(&0) {
        return Err("SaveFiles basenames must not contain NUL".into());
    }
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) if !name.is_empty() => Ok(()),
        _ => Err(format!(
            "SaveFiles name {path:?} is not a single non-empty basename"
        )),
    }
}

fn validate_absolute_path(name: &str, path: &Path) -> Result<(), String> {
    if !path.is_absolute() || path.as_os_str().as_bytes().contains(&0) {
        return Err(format!("{name} is not a valid absolute Unix path"));
    }
    Ok(())
}

fn unique_child(
    folder: &Path,
    name: &Path,
    reserved: &std::collections::HashSet<PathBuf>,
) -> Result<PathBuf, String> {
    validate_basename(name)?;
    let candidate = folder.join(name);
    if !reserved.contains(&candidate) && !path_occupied(&candidate) {
        return Ok(candidate);
    }

    let raw = name.as_os_str().as_bytes();
    let (stem, extension) = split_extension(raw);
    for suffix in 1..=u32::MAX {
        let mut bytes = stem.to_vec();
        bytes.extend_from_slice(format!("({suffix})").as_bytes());
        if let Some(extension) = extension {
            bytes.push(b'.');
            bytes.extend_from_slice(extension);
        }
        let candidate = folder.join(OsStr::from_bytes(&bytes));
        if !reserved.contains(&candidate) && !path_occupied(&candidate) {
            return Ok(candidate);
        }
    }
    Err(format!(
        "could not construct a unique filename for {name:?}"
    ))
}

fn path_occupied(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        // Permission and I/O failures must not be interpreted as a free name.
        Err(_) => true,
    }
}

fn split_extension(name: &[u8]) -> (&[u8], Option<&[u8]>) {
    let Some(dot) = name.iter().rposition(|byte| *byte == b'.') else {
        return (name, None);
    };
    if dot == 0 || dot + 1 == name.len() {
        return (name, None);
    }
    (&name[..dot], Some(&name[dot + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(mode: FileChooserMode) -> FileChooserRequest {
        FileChooserRequest {
            mode,
            app_id: "dev.aegis.Test".into(),
            title: "Choose".into(),
            accept_label: None,
            modal: true,
            parent_window: None,
            multiple: false,
            current_folder: None,
            current_name: None,
            current_file: None,
            filters: Vec::new(),
            current_filter: None,
            choices: Vec::new(),
            files: Vec::new(),
        }
    }

    #[test]
    fn byte_paths_round_trip_non_utf8() {
        let path = PathBuf::from(OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xff]));
        let json = serde_json::to_string(&BytePath::from(path.clone())).unwrap();
        let decoded: BytePath = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.to_path_buf(), path);
    }

    #[test]
    fn process_contract_rejects_version_mismatches() {
        let envelope = PrompterRequest {
            version: PROCESS_CONTRACT_VERSION + 1,
            prompt: PromptRequest::FileChooser(request(FileChooserMode::OpenFile)),
        };
        assert!(envelope.into_file_chooser().is_err());
        let envelope = PrompterResponse {
            version: PROCESS_CONTRACT_VERSION + 1,
            result: PromptResult::FileChooser(FileChooserResponse::Cancelled),
        };
        assert!(envelope.into_file_chooser().is_err());
    }

    #[test]
    fn typed_prompt_contract_round_trips_without_exposing_secrets() {
        let confirm = PrompterRequest::confirm(ConfirmRequest {
            title: "Share".into(),
            body: "Share account information?".into(),
            accept_label: Some("_Share".into()),
            modal: true,
            parent_window: Some("wayland:parent".into()),
        });
        let value = serde_json::to_value(&confirm).unwrap();
        assert_eq!(value["version"], PROCESS_CONTRACT_VERSION);
        assert_eq!(value["prompt"]["kind"], "confirm");
        assert!(matches!(
            serde_json::from_value::<PrompterRequest>(value)
                .unwrap()
                .into_prompt()
                .unwrap(),
            PromptRequest::Confirm(_)
        ));

        let secret = PrompterResponse::secret(SecretResponse::Secret {
            value: "do-not-log-this".into(),
        });
        let debug = format!("{secret:?}");
        assert!(!debug.contains("do-not-log-this"), "{debug}");
        let encoded = serde_json::to_value(secret).unwrap();
        assert_eq!(encoded["result"]["kind"], "secret");
        assert_eq!(encoded["result"]["response"]["status"], "secret");
    }

    #[test]
    fn confirmation_and_secret_text_are_bounded() {
        let empty = PrompterRequest::confirm(ConfirmRequest {
            title: String::new(),
            body: "Body".into(),
            accept_label: None,
            modal: true,
            parent_window: None,
        });
        assert!(empty.into_prompt().is_err());

        let oversized = PrompterRequest::secret(SecretRequest {
            title: "Unlock".into(),
            reason: Some("x".repeat(MAX_PROMPT_TEXT_BYTES + 1)),
        });
        assert!(oversized.into_prompt().is_err());
    }

    #[test]
    fn save_files_rejects_paths_and_parent_components() {
        for name in ["", ".", "..", "a/b", "/absolute"] {
            let mut req = request(FileChooserMode::SaveFiles);
            req.files.push(BytePath::from_path(name));
            assert!(req.validate().is_err(), "{name:?} must be rejected");
        }
    }

    #[test]
    fn request_rejects_non_absolute_locations_and_empty_filter_rules() {
        let mut req = request(FileChooserMode::OpenFile);
        req.current_folder = Some(BytePath::from_path("relative"));
        assert!(req.validate().is_err());

        req.current_folder = Some(BytePath::from_path("/tmp"));
        req.filters.push(FileFilter {
            label: "Files".into(),
            rules: vec![FilterRule {
                kind: FilterRuleKind::Glob,
                value: String::new(),
            }],
        });
        assert!(req.validate().is_err());
    }

    #[test]
    fn save_files_preserves_order_and_avoids_existing_names() {
        let folder = std::env::temp_dir().join(format!(
            "aegis-prompter-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("report.txt"), b"old").unwrap();
        std::fs::write(folder.join("report(1).txt"), b"old").unwrap();

        let mut req = request(FileChooserMode::SaveFiles);
        req.files = vec![
            BytePath::from_path("report.txt"),
            BytePath::from_path("image.png"),
        ];
        let paths = req.finish_paths(vec![folder.clone()]).unwrap();
        assert_eq!(paths[0], folder.join("report(2).txt"));
        assert_eq!(paths[1], folder.join("image.png"));
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn duplicate_choice_ids_are_rejected() {
        let mut req = request(FileChooserMode::OpenFile);
        req.choices = vec![
            Choice {
                id: "encoding".into(),
                label: "Encoding".into(),
                options: vec![("utf8".into(), "UTF-8".into())],
                selected: "utf8".into(),
            },
            Choice {
                id: "encoding".into(),
                label: "Again".into(),
                options: Vec::new(),
                selected: "false".into(),
            },
        ];
        assert!(req.validate().is_err());
    }

    #[test]
    fn selected_response_is_checked_against_the_request() {
        let mut req = request(FileChooserMode::OpenFile);
        req.choices.push(Choice {
            id: "encoding".into(),
            label: "Encoding".into(),
            options: vec![("utf8".into(), "UTF-8".into())],
            selected: "utf8".into(),
        });
        let valid = FileChooserResponse::Selected {
            paths: vec![BytePath::from_path("/tmp/file.txt")],
            current_filter: None,
            choices: vec![("encoding".into(), "utf8".into())],
        };
        assert!(valid.validate_for(&req).is_ok());

        let invalid = FileChooserResponse::Selected {
            paths: vec![BytePath::from_path("relative")],
            current_filter: None,
            choices: vec![("encoding".into(), "unknown".into())],
        };
        assert!(invalid.validate_for(&req).is_err());
    }

    #[test]
    fn save_files_avoids_duplicate_suggestions_in_one_request() {
        let folder =
            std::env::temp_dir().join(format!("aegis-prompter-duplicates-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let mut req = request(FileChooserMode::SaveFiles);
        req.files = vec![
            BytePath::from_path("same.txt"),
            BytePath::from_path("same.txt"),
        ];
        let paths = req.finish_paths(vec![folder.clone()]).unwrap();
        assert_eq!(paths, [folder.join("same.txt"), folder.join("same(1).txt")]);
        std::fs::remove_dir_all(folder).unwrap();
    }
}
