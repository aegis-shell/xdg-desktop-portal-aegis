//! freedesktop application and content-type resolution for the AppChooser
//! portal.
//!
//! Everything here is hand-rolled against the Desktop Entry and MIME Apps
//! specifications so the portal stays free of GLib: `.desktop` files are
//! scanned from `$XDG_DATA_HOME/applications` and each
//! `$XDG_DATA_DIRS/applications` (nearer directories shadow farther ones by
//! desktop id), `mimeinfo.cache` supplies the content-type index with the
//! `MimeType=` keys as the cache-miss fallback, and `mimeapps.list` applies
//! the spec's Added/Removed/Default associations with config-before-data
//! precedence. Writes are limited to `set_default_app`, which edits only
//! `$XDG_CONFIG_HOME/mimeapps.list` and preserves every unrelated line.
//!
//! All filesystem access goes through [`AppDirs`] so tests drive the logic
//! with fixture directories instead of the host system. Inputs are bounded:
//! desktop files and `mimeapps.list` past a fixed byte cap are skipped,
//! candidate lists are truncated to a screenful, and desktop ids must be
//! plain file names.
//!
//! Launching expands the Desktop Entry `Exec` field codes and spawns
//! detached. Entries with `Terminal=true` are refused by the caller (this
//! module cannot know which terminal emulator to prefer, and guessing
//! `$TERMINAL` silently would strand the user on systems without one); the
//! metadata codes `%i`/`%c`/`%k` need entry context the launch surface
//! deliberately does not take, so they expand to nothing.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Desktop files and association files past this size are skipped; real
/// entries are a few KiB.
const MAX_ENTRY_BYTES: u64 = 256 * 1024;
/// Enumeration result cap: one screenful of candidates.
const MAX_LISTED_APPS: usize = 64;
/// Number of `.desktop` files parsed per `applications` directory.
const MAX_ENTRIES_PER_DIR: usize = 1024;
/// `globs2` databases past this size are ignored; the system database is
/// a few hundred KiB.
const MAX_GLOBS_BYTES: u64 = 1024 * 1024;
/// Glob rules read per database.
const MAX_GLOBS: usize = 16 * 1024;
/// Exec line and id length caps.
const MAX_EXEC_BYTES: usize = 8 * 1024;
const MAX_ID_BYTES: usize = 256;
/// A single launch carries at most this many URIs.
const MAX_LAUNCH_URIS: usize = 256;

/// One resolved desktop entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppInfo {
    /// The desktop file id, e.g. `org.foo.Bar.desktop`.
    pub(crate) id: String,
    pub(crate) name: String,
    /// The raw `Exec=` value; field codes are expanded only at launch.
    pub(crate) exec: String,
    pub(crate) icon: Option<String>,
    /// `Terminal=true`: the entry needs a terminal emulator the portal
    /// does not pick; the caller refuses to launch it.
    pub(crate) terminal: bool,
    /// `NoDisplay=true`: not enumerated, but still resolved when the
    /// portal frontend names the id explicitly.
    pub(crate) no_display: bool,
    /// The content types the entry's `MimeType=` declares.
    pub(crate) mime_types: Vec<String>,
}

/// The XDG directory set every lookup reads. Constructed from the process
/// environment in production and from fixture roots in tests.
#[derive(Debug, Clone)]
pub(crate) struct AppDirs {
    data_home: PathBuf,
    data_dirs: Vec<PathBuf>,
    config_home: PathBuf,
    config_dirs: Vec<PathBuf>,
}

impl AppDirs {
    /// The environment's directory set, with the spec's defaults for unset
    /// variables.
    pub(crate) fn from_env() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(".local/share")))
            .unwrap_or_else(|| PathBuf::from("/"));
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.map(|home| home.join(".config")))
            .unwrap_or_else(|| PathBuf::from("/"));
        let split = |variable: &str, default: &str| {
            std::env::var_os(variable)
                .filter(|value| !value.is_empty())
                .map(|value| {
                    std::env::split_paths(&value)
                        .filter(|dir| !dir.as_os_str().is_empty())
                        .collect()
                })
                .unwrap_or_else(|| {
                    default
                        .split(':')
                        .map(PathBuf::from)
                        .collect::<Vec<PathBuf>>()
                })
        };
        Self {
            data_home,
            data_dirs: split("XDG_DATA_DIRS", "/usr/local/share:/usr/share"),
            config_home,
            config_dirs: split("XDG_CONFIG_DIRS", "/etc/xdg"),
        }
    }

    /// An explicit directory set for fixture-driven tests (and for the
    /// app_chooser module's tests, which live outside this module).
    #[cfg(test)]
    pub(crate) fn fixture(
        data_home: PathBuf,
        data_dirs: Vec<PathBuf>,
        config_home: PathBuf,
        config_dirs: Vec<PathBuf>,
    ) -> Self {
        Self {
            data_home,
            data_dirs,
            config_home,
            config_dirs,
        }
    }

    /// `applications/` directories in shadowing order (nearest first).
    fn applications_dirs(&self) -> Vec<PathBuf> {
        std::iter::once(&self.data_home)
            .chain(&self.data_dirs)
            .map(|root| root.join("applications"))
            .collect()
    }

    /// `mimeapps.list` files in spec precedence order (most preferred
    /// first): config home, config dirs, data home, data dirs.
    fn mimeapps_files(&self) -> Vec<PathBuf> {
        std::iter::once(self.config_home.join("mimeapps.list"))
            .chain(
                self.config_dirs
                    .iter()
                    .map(|root| root.join("mimeapps.list")),
            )
            .chain(
                self.applications_dirs()
                    .iter()
                    .map(|dir| dir.join("mimeapps.list")),
            )
            .collect()
    }

    /// Parse one desktop file relative to an `applications/` root. Only
    /// plain file names are valid desktop ids here (no subdirectory ids).
    fn entry_at(&self, applications: &Path, id: &str) -> Option<AppInfo> {
        parse_desktop_file(&applications.join(id), id)
    }

    /// Look up one desktop id with shadowing applied. Ids with path
    /// separators or NUL never resolve.
    pub(crate) fn app_by_id(&self, id: &str) -> Option<AppInfo> {
        if !valid_desktop_id(id) {
            return None;
        }
        self.applications_dirs()
            .iter()
            .find_map(|dir| self.entry_at(dir, id))
    }

    /// Every desktop entry, nearer directories shadowing farther ones by
    /// id, in scan order. `Hidden=true` entries are dropped entirely.
    fn load_all(&self) -> Vec<AppInfo> {
        let mut seen = HashSet::new();
        let mut apps = Vec::new();
        for dir in self.applications_dirs() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut names: Vec<String> = entries
                .flatten()
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|name| name.ends_with(".desktop") && valid_desktop_id(name))
                .take(MAX_ENTRIES_PER_DIR)
                .collect();
            names.sort_unstable();
            for id in names {
                if !seen.insert(id.clone()) {
                    continue;
                }
                if let Some(app) = self.entry_at(&dir, &id) {
                    apps.push(app);
                }
            }
        }
        apps
    }

    /// The applications registered for `content_type`, best first, capped
    /// at [`MAX_LISTED_APPS`]. The list is the mimeapps.list Added
    /// associations (precedence order), then the `mimeinfo.cache` hits,
    /// then entries declaring the `MimeType=` key, minus every Removed
    /// association. `NoDisplay` entries are not enumerated; entries
    /// without an `Exec=` line are useless to the chooser and dropped.
    pub(crate) fn apps_for_content_type(&self, content_type: &str) -> Vec<AppInfo> {
        if !valid_content_type(content_type) {
            return Vec::new();
        }
        let apps = self.load_all();
        let by_id: HashMap<&str, &AppInfo> =
            apps.iter().map(|app| (app.id.as_str(), app)).collect();

        let mut added: Vec<String> = Vec::new();
        let mut removed: HashSet<String> = HashSet::new();
        for file in self.mimeapps_files() {
            let groups = parse_grouped_lists(&file);
            for id in groups
                .get("Added Associations")
                .and_then(|group| group.get(content_type))
                .into_iter()
                .flatten()
            {
                if !added.contains(id) {
                    added.push(id.clone());
                }
            }
            if let Some(ids) = groups
                .get("Removed Associations")
                .and_then(|group| group.get(content_type))
            {
                removed.extend(ids.iter().cloned());
            }
        }

        let mut ordered: Vec<String> = added;
        for dir in self.applications_dirs() {
            let cache = parse_grouped_lists(&dir.join("mimeinfo.cache"));
            if let Some(ids) = cache
                .get("MIME Cache")
                .and_then(|group| group.get(content_type))
            {
                for id in ids {
                    if !ordered.contains(id) {
                        ordered.push(id.clone());
                    }
                }
            }
        }
        for app in &apps {
            if app.mime_types.iter().any(|mime| mime == content_type) && !ordered.contains(&app.id)
            {
                ordered.push(app.id.clone());
            }
        }

        ordered
            .iter()
            .filter(|id| !removed.contains(*id))
            .filter_map(|id| by_id.get(id.as_str()).copied())
            .filter(|app| !app.no_display && !app.exec.is_empty())
            .take(MAX_LISTED_APPS)
            .cloned()
            .collect()
    }

    /// The configured default for `content_type`: the first resolvable id
    /// in the first `[Default Applications]` entry, following mimeapps.list
    /// precedence.
    pub(crate) fn default_app(&self, content_type: &str) -> Option<AppInfo> {
        if !valid_content_type(content_type) {
            return None;
        }
        for file in self.mimeapps_files() {
            let groups = parse_grouped_lists(&file);
            let Some(ids) = groups
                .get("Default Applications")
                .and_then(|group| group.get(content_type))
            else {
                continue;
            };
            for id in ids {
                if let Some(app) = self.app_by_id(id) {
                    return Some(app);
                }
            }
        }
        None
    }

    /// The content type for a file *name* per the shared-mime-info glob
    /// databases (`mime/globs2` under each data root). All roots'
    /// databases apply; the highest priority wins, ties go to the nearer
    /// root, and matching is case-insensitive unless the glob carries the
    /// `cs` flag. `None` means no database matched — the caller falls back
    /// to `application/octet-stream`.
    ///
    /// The files are parsed per call: `globs2` is small, OpenURI requests
    /// are rare, and a process-global cache would pin the host's database
    /// into fixture-driven tests.
    pub(crate) fn content_type_for_filename(&self, name: &str) -> Option<String> {
        let roots = std::iter::once(&self.data_home).chain(&self.data_dirs);
        let mut best: Option<(u32, String)> = None;
        for root in roots {
            for (priority, glob, mime, case_sensitive) in parse_globs2(&root.join("mime/globs2")) {
                let better = best
                    .as_ref()
                    .is_none_or(|(best_priority, _)| priority > *best_priority);
                if better && glob_matches(&glob, name, case_sensitive) {
                    best = Some((priority, mime));
                }
            }
        }
        best.map(|(_, mime)| mime)
    }

    /// Record `desktop_id` as the default for `content_type` in
    /// `$XDG_CONFIG_HOME/mimeapps.list`, preserving every unrelated line
    /// and any previously listed fallback ids. The write is atomic
    /// (temp file plus rename) so a crash never leaves a torn file.
    pub(crate) fn set_default_app(&self, content_type: &str, desktop_id: &str) -> io::Result<()> {
        if !valid_content_type(content_type) || !valid_desktop_id(desktop_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid content type or desktop id",
            ));
        }
        let path = self.config_home.join("mimeapps.list");
        let existing = match std::fs::read(&path) {
            Ok(bytes) if bytes.len() as u64 <= MAX_ENTRY_BYTES => {
                String::from_utf8_lossy(&bytes).into_owned()
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "mimeapps.list exceeds the size cap",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error),
        };
        let updated = update_default_applications(&existing, content_type, desktop_id);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = self
            .config_home
            .join(format!(".mimeapps.list.aegis-{}", std::process::id()));
        std::fs::write(&temp, updated)?;
        std::fs::rename(&temp, &path)
    }
}

/// Desktop ids handled here are plain UTF-8-ish file names: no separators,
/// no NUL, bounded length.
fn valid_desktop_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && !id.contains('/')
        && !id.contains('\\')
        && !id.contains('\0')
        && !id.contains('\n')
}

/// A content type is `type/subtype`-shaped text, bounded and NUL-free.
fn valid_content_type(content_type: &str) -> bool {
    let mut parts = content_type.split('/');
    let (Some(kind), Some(subtype), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    content_type.len() <= MAX_ID_BYTES
        && !content_type.contains('\0')
        && !content_type.contains('\n')
        && !kind.trim().is_empty()
        && !subtype.trim().is_empty()
}

/// Read and parse one INI-ish file into `group -> key -> id list` form.
/// Missing or oversized files yield an empty map. Only `;`-separated list
/// values are kept, which covers both `mimeinfo.cache` and `mimeapps.list`.
fn parse_grouped_lists(path: &Path) -> HashMap<String, HashMap<String, Vec<String>>> {
    let mut groups: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
    let Ok(file) = std::fs::File::open(path) else {
        return groups;
    };
    let mut bytes = Vec::new();
    if std::io::Read::read_to_end(
        &mut std::io::Read::take(file, MAX_ENTRY_BYTES + 1),
        &mut bytes,
    )
    .is_err()
        || bytes.len() as u64 > MAX_ENTRY_BYTES
    {
        return groups;
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut group = String::new();
    for line in text.lines() {
        let line = line.trim_end();
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
        {
            group = name.trim().to_owned();
            continue;
        }
        if group.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || key.contains('[') {
            continue;
        }
        let ids: Vec<String> = value
            .split(';')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .collect();
        groups
            .entry(group.clone())
            .or_default()
            .insert(key.to_owned(), ids);
    }
    groups
}

/// One `globs2` line: `priority:glob:mimetype[:flags]`. Only the `cs`
/// flag is honoured (case-sensitive matching); character classes inside
/// globs are not expanded — real-world `globs2` files use `*` and `?`.
/// Lines that do not fit the shape are skipped.
fn parse_globs2(path: &Path) -> Vec<(u32, String, String, bool)> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut bytes = Vec::new();
    if std::io::Read::read_to_end(
        &mut std::io::Read::take(file, MAX_GLOBS_BYTES + 1),
        &mut bytes,
    )
    .is_err()
        || bytes.len() as u64 > MAX_GLOBS_BYTES
    {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut globs = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let Some((priority, rest)) = line.split_once(':') else {
            continue;
        };
        let Ok(priority) = priority.trim().parse::<u32>() else {
            continue;
        };
        let Some((glob, rest)) = rest.split_once(':') else {
            continue;
        };
        let (mime, flags) = rest.split_once(':').unwrap_or((rest, ""));
        if glob.is_empty() || mime.is_empty() || globs.len() >= MAX_GLOBS {
            continue;
        }
        let case_sensitive = flags.split(',').any(|flag| flag.trim() == "cs");
        globs.push((priority, glob.to_owned(), mime.to_owned(), case_sensitive));
    }
    globs
}

/// Match a `globs2` pattern against a file name. Supports `*` (any run)
/// and `?` (one character); everything else is literal. Case-insensitive
/// unless `case_sensitive` (`cs` flag).
fn glob_matches(glob: &str, name: &str, case_sensitive: bool) -> bool {
    let fold = |text: &str| {
        if case_sensitive {
            text.chars().collect::<Vec<char>>()
        } else {
            text.to_lowercase().chars().collect()
        }
    };
    let pattern = fold(glob);
    let text = fold(name);
    let (mut p, mut t) = (0, 0);
    let (mut star_p, mut star_t) = (usize::MAX, 0);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star_p = p;
            star_t = t;
            p += 1;
        } else if star_p != usize::MAX {
            p = star_p + 1;
            star_t += 1;
            t = star_t;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// Parse one desktop file. Only the `[Desktop Entry]` group is read;
/// `Hidden=true` deletes the entry from view (returns `None`).
fn parse_desktop_file(path: &Path, id: &str) -> Option<AppInfo> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    if std::io::Read::read_to_end(
        &mut std::io::Read::take(file, MAX_ENTRY_BYTES + 1),
        &mut bytes,
    )
    .is_err()
        || bytes.len() as u64 > MAX_ENTRY_BYTES
    {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);

    let mut in_entry = false;
    let mut name = None;
    let mut exec = None;
    let mut icon = None;
    let mut no_display = false;
    let mut terminal = false;
    let mut mime_types = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if let Some(group) = line
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
        {
            in_entry = group.trim() == "Desktop Entry";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Name" => name = Some(value.to_owned()),
            "Exec" if value.len() <= MAX_EXEC_BYTES => exec = Some(value.to_owned()),
            "Icon" if !value.is_empty() => icon = Some(value.to_owned()),
            "NoDisplay" => no_display = value == "true",
            "Hidden" if value == "true" => return None,
            "Terminal" => terminal = value == "true",
            "MimeType" => {
                mime_types = value
                    .split(';')
                    .map(str::trim)
                    .filter(|mime| !mime.is_empty())
                    .map(str::to_owned)
                    .collect();
            }
            _ => {}
        }
    }

    Some(AppInfo {
        id: id.to_owned(),
        name: name?.chars().take(256).collect(),
        exec: exec?,
        icon,
        terminal,
        no_display,
        mime_types,
    })
}

/// Minimal INI edit: set `content_type`'s `[Default Applications]` value to
/// `desktop_id` (keeping any other listed ids as fallbacks behind it),
/// inserting the key or the whole group when absent. Every unrelated line
/// is preserved verbatim.
fn update_default_applications(text: &str, content_type: &str, desktop_id: &str) -> String {
    let key_line = |id: &str| format!("{content_type}={id};\n");
    let mut out: Vec<String> = Vec::new();
    let mut in_defaults = false;
    let mut seen_group = false;
    let mut wrote_key = false;

    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let header = body
            .trim_end()
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
            .map(str::trim);
        if let Some(header) = header {
            // Leaving the group without having seen the key: append it.
            if in_defaults && !wrote_key {
                out.push(key_line(desktop_id));
                wrote_key = true;
            }
            in_defaults = header == "Default Applications";
            seen_group |= in_defaults;
            out.push(line.to_owned());
            continue;
        }
        let is_target = in_defaults
            && body
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == content_type);
        if is_target {
            let existing = body.split_once('=').map_or("", |(_, value)| value);
            let mut ids: Vec<&str> = vec![desktop_id];
            ids.extend(
                existing
                    .split(';')
                    .map(str::trim)
                    .filter(|id| !id.is_empty() && *id != desktop_id),
            );
            out.push(key_line(&ids.join(";")));
            wrote_key = true;
            continue;
        }
        out.push(line.to_owned());
    }
    // The file ended inside the group without the key.
    if in_defaults && !wrote_key {
        out.push(key_line(desktop_id));
    }

    if !seen_group {
        if let Some(last) = out.last_mut()
            && !last.ends_with('\n')
        {
            last.push('\n');
        }
        if !out.is_empty() {
            out.push("\n".to_owned());
        }
        out.push("[Default Applications]\n".to_owned());
        out.push(key_line(desktop_id));
    }

    let mut result = out.concat();
    if !result.is_empty() && !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// Launch `exec` with `uris`, expanding the Desktop Entry field codes, and
/// return without waiting. The child is deliberately unreaped: the portal
/// is a long-lived daemon whose launched applications outlive the request,
/// and reaping belongs to the session's init (or a future SIGCHLD reaper).
pub(crate) fn launch(exec: &str, uris: &[String]) -> io::Result<()> {
    if exec.len() > MAX_EXEC_BYTES || uris.len() > MAX_LAUNCH_URIS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Exec line or URI list is oversized",
        ));
    }
    let argv = expand_exec(exec, uris);
    let Some((program, args)) = argv.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the Exec line expanded to nothing",
        ));
    };
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

/// Split an `Exec` line into tokens honouring the spec's quoting: double
/// quotes group, and a backslash escapes the next character (inside quotes
/// only before `"`, `` ` ``, `$`, or `\`; elsewhere before anything).
fn split_exec(exec: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut has_token = false;
    let mut chars = exec.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '"' => {
                quoted = !quoted;
                has_token = true;
            }
            '\\' => {
                let Some(&next) = chars.peek() else {
                    break;
                };
                if !quoted || matches!(next, '"' | '`' | '$' | '\\') {
                    token.push(next);
                    chars.next();
                } else {
                    token.push('\\');
                }
                has_token = true;
            }
            c if c.is_whitespace() && !quoted => {
                if has_token {
                    tokens.push(std::mem::take(&mut token));
                    has_token = false;
                }
            }
            c => {
                token.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        tokens.push(token);
    }
    tokens
}

/// Expand one `Exec` line into an argv vector. Whole-token `%u`/`%U`/`%f`/
/// `%F` expand to one/many arguments; embedded codes expand inline (`%%`
/// to `%`, the metadata codes `%i`/`%c`/`%k` and unknown codes to
/// nothing). When no URI code appears the URIs are appended, matching the
/// spec's fallback. `%f`/`%F` keep only `file://` URIs, decoded to paths.
fn expand_exec(exec: &str, uris: &[String]) -> Vec<String> {
    let mut argv = Vec::new();
    let mut used_uris = false;
    for token in split_exec(exec) {
        match token.as_str() {
            "%u" => {
                used_uris = true;
                if let Some(first) = uris.first() {
                    argv.push(first.clone());
                }
            }
            "%U" => {
                used_uris = true;
                argv.extend(uris.iter().cloned());
            }
            "%f" => {
                used_uris = true;
                if let Some(path) = uris.first().and_then(|uri| uri_to_path(uri)) {
                    argv.push(path);
                }
            }
            "%F" => {
                used_uris = true;
                argv.extend(uris.iter().filter_map(|uri| uri_to_path(uri)));
            }
            _ => {
                let mut expanded = String::new();
                let mut chars = token.chars().peekable();
                while let Some(character) = chars.next() {
                    if character != '%' {
                        expanded.push(character);
                        continue;
                    }
                    let Some(code) = chars.next() else {
                        break;
                    };
                    used_uris |= matches!(code, 'u' | 'U' | 'f' | 'F');
                    match code {
                        '%' => expanded.push('%'),
                        'u' => {
                            if let Some(first) = uris.first() {
                                expanded.push_str(first);
                            }
                        }
                        'U' => expanded.push_str(&uris.join(" ")),
                        'f' => {
                            if let Some(path) = uris.first().and_then(|uri| uri_to_path(uri)) {
                                expanded.push_str(&path);
                            }
                        }
                        'F' => expanded.push_str(
                            &uris
                                .iter()
                                .filter_map(|uri| uri_to_path(uri))
                                .collect::<Vec<_>>()
                                .join(" "),
                        ),
                        // %i/%c/%k need entry metadata this surface does
                        // not take; unknown codes are dropped (spec).
                        _ => {}
                    }
                }
                if !expanded.is_empty() {
                    argv.push(expanded);
                }
            }
        }
    }
    if !used_uris {
        argv.extend(uris.iter().cloned());
    }
    argv
}

/// Decode a `file://` URI into a local path; anything else returns `None`.
fn uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    let path = path.strip_prefix('/').map(|tail| format!("/{tail}"))?;
    percent_decode(&path)
}

/// Decode `%XX` escapes; malformed sequences pass through unchanged.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 3 <= bytes.len()
            && let Ok(value) = u8::from_str_radix(&text[index + 1..index + 3], 16)
        {
            out.push(value);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture XDG tree under a unique temp root.
    struct Fixture {
        root: PathBuf,
        dirs: AppDirs,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "aegis-apps-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let dirs = AppDirs {
                data_home: root.join("data-home"),
                data_dirs: vec![root.join("data-a"), root.join("data-b")],
                config_home: root.join("config-home"),
                config_dirs: vec![root.join("config-a")],
            };
            Self { root, dirs }
        }

        /// Write a desktop file under one of the data roots.
        fn desktop(&self, root: &str, id: &str, body: &str) {
            let dir = self.root.join(root).join("applications");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(id), body).unwrap();
        }

        fn write(&self, relative: &str, body: &str) {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    const EDITOR: &str = "[Desktop Entry]\nName=Foo Editor\nExec=foo-edit %U\nIcon=foo-edit\nMimeType=text/plain;text/markdown;\n";
    const VIEWER: &str = "[Desktop Entry]\nName=Bar Viewer\nExec=bar-view %u\nNoDisplay=true\nMimeType=text/plain;\n";

    #[test]
    fn nearer_directories_shadow_farther_ones() {
        let fixture = Fixture::new("shadow");
        fixture.desktop("data-a", "foo.desktop", EDITOR);
        fixture.desktop(
            "data-b",
            "foo.desktop",
            "[Desktop Entry]\nName=Wrong\nExec=wrong %u\n",
        );
        fixture.desktop(
            "data-b",
            "baz.desktop",
            "[Desktop Entry]\nName=Baz\nExec=baz\n",
        );

        let app = fixture.dirs.app_by_id("foo.desktop").unwrap();
        assert_eq!(app.name, "Foo Editor");
        assert!(fixture.dirs.app_by_id("baz.desktop").is_some());
        assert!(fixture.dirs.app_by_id("../escape.desktop").is_none());
        assert!(fixture.dirs.app_by_id("missing.desktop").is_none());
    }

    #[test]
    fn hidden_entries_are_dropped_and_terminal_is_recorded() {
        let fixture = Fixture::new("hidden");
        fixture.desktop(
            "data-home",
            "gone.desktop",
            "[Desktop Entry]\nName=Gone\nExec=gone\nHidden=true\n",
        );
        fixture.desktop(
            "data-home",
            "term.desktop",
            "[Desktop Entry]\nName=Term\nExec=term %f\nTerminal=true\n",
        );
        assert!(fixture.dirs.app_by_id("gone.desktop").is_none());
        assert!(fixture.dirs.app_by_id("term.desktop").unwrap().terminal);
    }

    #[test]
    fn associations_merge_cache_mimeapps_and_declared_types() {
        let fixture = Fixture::new("assoc");
        fixture.desktop("data-home", "editor.desktop", EDITOR);
        fixture.desktop(
            "data-a",
            "viewer.desktop",
            "[Desktop Entry]\nName=Bar Viewer\nExec=bar-view %u\nMimeType=text/plain;\n",
        );
        fixture.desktop(
            "data-a",
            "extra.desktop",
            "[Desktop Entry]\nName=Extra\nExec=extra %u\n",
        );
        fixture.write(
            "data-a/applications/mimeinfo.cache",
            "[MIME Cache]\ntext/plain=viewer.desktop;extra.desktop;\n",
        );
        // Added wins over the cache; a farther Removed still filters it.
        fixture.write(
            "config-home/mimeapps.list",
            "[Added Associations]\ntext/plain=editor.desktop;\n",
        );
        fixture.write(
            "data-b/applications/mimeapps.list",
            "[Removed Associations]\ntext/plain=extra.desktop;\n",
        );

        let apps = fixture.dirs.apps_for_content_type("text/plain");
        let ids: Vec<&str> = apps.iter().map(|app| app.id.as_str()).collect();
        // Added first, cache next (minus the removed), then MimeType=.
        assert_eq!(ids, ["editor.desktop", "viewer.desktop"]);
        assert!(fixture.dirs.apps_for_content_type("not-a-type").is_empty());
    }

    #[test]
    fn no_display_entries_resolve_only_when_explicit() {
        let fixture = Fixture::new("nodisplay");
        fixture.desktop("data-home", "viewer.desktop", VIEWER);
        assert!(fixture.dirs.apps_for_content_type("text/plain").is_empty());
        let explicit = fixture.dirs.app_by_id("viewer.desktop").unwrap();
        assert_eq!(explicit.name, "Bar Viewer");
        assert!(explicit.no_display);
    }

    #[test]
    fn default_app_follows_precedence_and_skips_missing_ids() {
        let fixture = Fixture::new("default");
        fixture.desktop("data-home", "editor.desktop", EDITOR);
        fixture.desktop("data-a", "viewer.desktop", VIEWER);
        fixture.write(
            "config-a/mimeapps.list",
            "[Default Applications]\ntext/plain=missing.desktop;viewer.desktop;\n",
        );
        fixture.write(
            "data-a/applications/mimeapps.list",
            "[Default Applications]\ntext/plain=editor.desktop;\n",
        );
        // The config dir beats the data dir; the missing id falls through.
        let default = fixture.dirs.default_app("text/plain").unwrap();
        assert_eq!(default.id, "viewer.desktop");
        assert!(fixture.dirs.default_app("image/png").is_none());
    }

    #[test]
    fn set_default_app_preserves_unrelated_content() {
        let fixture = Fixture::new("setdefault");
        fixture.write(
            "config-home/mimeapps.list",
            "[Default Applications]\ntext/html=browser.desktop;\n\n[Added Associations]\ntext/plain=editor.desktop;\n",
        );
        fixture
            .dirs
            .set_default_app("text/plain", "writer.desktop")
            .unwrap();
        fixture
            .dirs
            .set_default_app("text/plain", "editor.desktop")
            .unwrap();
        let text = std::fs::read_to_string(fixture.root.join("config-home/mimeapps.list")).unwrap();
        assert!(text.contains("text/html=browser.desktop;"));
        assert!(text.contains("[Added Associations]\ntext/plain=editor.desktop;"));
        // The second write keeps the first id as a fallback behind the new
        // default.
        assert!(text.contains("text/plain=editor.desktop;writer.desktop;"));
    }

    #[test]
    fn set_default_app_creates_the_file_and_group() {
        let fixture = Fixture::new("setdefault-new");
        fixture
            .dirs
            .set_default_app("image/png", "viewer.desktop")
            .unwrap();
        let text = std::fs::read_to_string(fixture.root.join("config-home/mimeapps.list")).unwrap();
        assert_eq!(text, "[Default Applications]\nimage/png=viewer.desktop;\n");
        assert!(
            fixture
                .dirs
                .set_default_app("image png", "viewer.desktop")
                .is_err()
        );
    }

    #[test]
    fn exec_splitting_honours_quoting_and_escapes() {
        assert_eq!(split_exec("foo bar baz"), ["foo", "bar", "baz"]);
        assert_eq!(split_exec("foo \"a b\" c"), ["foo", "a b", "c"]);
        assert_eq!(split_exec("foo a\\ b"), ["foo", "a b"]);
        assert_eq!(split_exec("foo \"a\\\"b\""), ["foo", "a\"b"]);
    }

    #[test]
    fn exec_expansion_covers_the_field_codes() {
        let uris = vec![
            "file:///tmp/a%20b.txt".to_owned(),
            "https://x.test/".to_owned(),
        ];
        assert_eq!(
            expand_exec("foo %U", &uris),
            ["foo", "file:///tmp/a%20b.txt", "https://x.test/"]
        );
        assert_eq!(
            expand_exec("foo %u", &uris),
            ["foo", "file:///tmp/a%20b.txt"]
        );
        assert_eq!(expand_exec("foo %F", &uris), ["foo", "/tmp/a b.txt"]);
        // The metadata codes %c/%i/%k expand to nothing, and with no URI
        // code present the URIs are appended.
        assert_eq!(
            expand_exec("foo --name=%c %% %i %k", &uris),
            [
                "foo",
                "--name=",
                "%",
                "file:///tmp/a%20b.txt",
                "https://x.test/"
            ]
        );
        // No URI code: the URIs are appended.
        assert_eq!(
            expand_exec("foo --flag", &uris),
            ["foo", "--flag", "file:///tmp/a%20b.txt", "https://x.test/"]
        );
        // An empty URI list still launches the bare program.
        assert_eq!(expand_exec("foo %u", &[]), ["foo"]);
    }

    #[test]
    fn launch_rejects_empty_exec_and_runs_true() {
        assert!(launch("", &[]).is_err());
        launch("true %u", &["file:///tmp/x".to_owned()]).unwrap();
    }

    #[test]
    fn globs2_resolve_by_priority_with_nearer_roots_winning_ties() {
        let fixture = Fixture::new("globs");
        fixture.write(
            "data-a/mime/globs2",
            "# comment\n50:*.txt:text/plain\n80:*.txt:text/x-special\n",
        );
        fixture.write(
            "data-home/mime/globs2",
            "80:*.txt:text/x-nearer\n60:*.md:text/markdown\n",
        );

        // Higher priority beats the nearer root's lower one.
        assert_eq!(
            fixture.dirs.content_type_for_filename("notes.TXT"),
            Some("text/x-nearer".to_owned()),
            "matching is case-insensitive without the cs flag"
        );
        assert_eq!(
            fixture.dirs.content_type_for_filename("readme.md"),
            Some("text/markdown".to_owned())
        );
        assert_eq!(fixture.dirs.content_type_for_filename("data.bin"), None);
    }

    #[test]
    fn globs2_honour_the_case_sensitive_flag_and_wildcards() {
        let fixture = Fixture::new("globs-cs");
        fixture.write(
            "data-home/mime/globs2",
            "50:*.CS:text/x-upper:cs\n50:makefile:text/x-makefile\n50:photo.???:image/x-any3\n",
        );
        let dirs = &fixture.dirs;
        assert_eq!(
            dirs.content_type_for_filename("FLAGS.CS"),
            Some("text/x-upper".to_owned())
        );
        assert_eq!(dirs.content_type_for_filename("flags.cs"), None);
        assert_eq!(
            dirs.content_type_for_filename("Makefile"),
            Some("text/x-makefile".to_owned())
        );
        assert_eq!(
            dirs.content_type_for_filename("photo.jpg"),
            Some("image/x-any3".to_owned())
        );
        assert_eq!(dirs.content_type_for_filename("photo.jpeg"), None);
    }
}
