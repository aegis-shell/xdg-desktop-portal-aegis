//! Filesystem and filter logic for the file-selection dialog, kept pure so
//! it is testable without a GPU or a window.

use std::path::{Path, PathBuf};

use aegis_portal_prompter::{FileFilter, FilterRuleKind};

/// One row in the directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    /// Display name (lossy; the path bytes stay exact).
    pub name: String,
    pub is_dir: bool,
}

/// Read `dir` into dialog rows: directories first, then files, each group
/// sorted case-insensitively. Dotfiles are hidden unless `show_hidden`;
/// files must pass `filter` (directories always stay visible for
/// navigation).
pub fn list_dir(
    dir: &Path,
    show_hidden: bool,
    filter: Option<&FileFilter>,
) -> Result<Vec<Entry>, String> {
    let read = std::fs::read_dir(dir)
        .map_err(|error| format!("could not open {}: {error}", dir.display()))?;
    let mut entries = Vec::new();
    for item in read {
        let item = match item {
            Ok(item) => item,
            Err(error) => {
                log::warn!(
                    "prompter: skipping unreadable entry in {}: {error}",
                    dir.display()
                );
                continue;
            }
        };
        let name = item.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let is_dir = item.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        if !is_dir && filter.is_some_and(|filter| !filter_allows(filter, &item.path())) {
            continue;
        }
        entries.push(Entry {
            path: item.path(),
            name,
            is_dir,
        });
    }
    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    Ok(entries)
}

/// Whether a file passes a portal filter: any single rule matches (the
/// portal's rules within one filter are OR-ed, matching GTK).
pub fn filter_allows(filter: &FileFilter, path: &Path) -> bool {
    filter.rules.iter().any(|rule| match rule.kind {
        FilterRuleKind::Glob => path
            .file_name()
            .is_some_and(|name| glob_match(&rule.value, &name.to_string_lossy())),
        FilterRuleKind::Mime => mime_matches(&rule.value, path),
    })
}

/// A mime rule is the full essence (`image/png`) or a type prefix
/// (`image/*`). Types come from the filename's extension, the same source
/// GTK's mime filtering resolves through gio.
fn mime_matches(rule: &str, path: &Path) -> bool {
    let Some(guessed) = mime_guess::from_path(path).first() else {
        return false;
    };
    let essence = guessed.essence_str();
    if let Some(prefix) = rule.strip_suffix('*') {
        essence.starts_with(prefix)
    } else {
        essence == rule
    }
}

/// Match a filename against a glob pattern with `*` (any run), `?` (one
/// character), and `[...]` classes (ranges and `!` negation). Anything
/// else matches literally; a malformed class degrades to a literal `[`.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    glob(&pattern, &name)
}

fn glob(pattern: &[char], name: &[char]) -> bool {
    match pattern.first() {
        None => name.is_empty(),
        Some('*') => glob(&pattern[1..], name) || (!name.is_empty() && glob(pattern, &name[1..])),
        Some('?') => !name.is_empty() && glob(&pattern[1..], &name[1..]),
        Some('[') => match parse_class(pattern) {
            Some((matches, rest)) if !name.is_empty() => matches(name[0]) && glob(rest, &name[1..]),
            _ => name.first() == Some(&'[') && glob(&pattern[1..], &name[1..]),
        },
        Some(&literal) => name.first() == Some(&literal) && glob(&pattern[1..], &name[1..]),
    }
}

/// Parse `[...]` at the head of `pattern` into a predicate and the rest of
/// the pattern. Returns `None` when the class is unterminated or empty.
fn parse_class(pattern: &[char]) -> Option<(impl Fn(char) -> bool + use<>, &[char])> {
    debug_assert_eq!(pattern.first(), Some(&'['));
    let mut items: Vec<(char, char)> = Vec::new();
    let mut index = 1;
    let negated = pattern.get(index) == Some(&'!');
    if negated {
        index += 1;
    }
    while index < pattern.len() && pattern[index] != ']' {
        let low = pattern[index];
        let high = if pattern.get(index + 1) == Some(&'-')
            && pattern.get(index + 2).is_some_and(|&c| c != ']')
        {
            index += 2;
            pattern[index]
        } else {
            low
        };
        items.push((low, high));
        index += 1;
    }
    if index >= pattern.len() || items.is_empty() {
        return None;
    }
    Some((
        move |c: char| items.iter().any(|&(low, high)| (low..=high).contains(&c)) != negated,
        &pattern[index + 1..],
    ))
}

/// The path's ancestor chain from the root down to itself, for the
/// location breadcrumb.
pub fn breadcrumbs(dir: &Path) -> Vec<PathBuf> {
    let mut chain: Vec<PathBuf> = dir.ancestors().map(Path::to_path_buf).collect();
    chain.reverse();
    chain
}

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_portal_prompter::FilterRule;

    fn filter(label: &str, rules: &[(&str, FilterRuleKind)]) -> FileFilter {
        FileFilter {
            label: label.into(),
            rules: rules
                .iter()
                .map(|(value, kind)| FilterRule {
                    kind: *kind,
                    value: (*value).to_owned(),
                })
                .collect(),
        }
    }

    #[test]
    fn glob_matches_stars_questions_and_classes() {
        assert!(glob_match("*.png", "shot.png"));
        assert!(!glob_match("*.png", "shot.jpg"));
        assert!(glob_match("shot.p?g", "shot.png"));
        assert!(glob_match("*.tar.*", "a.tar.gz"));
        assert!(glob_match("[a-z].txt", "b.txt"));
        assert!(!glob_match("[a-z].txt", "B.txt"));
        assert!(glob_match("[!a-z].txt", "B.txt"));
        assert!(glob_match("*", "anything"));
        assert!(!glob_match("*.png", "png"));
        assert!(!glob_match("[.txt", "x.txt"));
        assert!(glob_match("[.txt", "[.txt"));
    }

    #[test]
    fn glob_respects_case_and_full_length() {
        assert!(!glob_match("*.PNG", "shot.png"));
        assert!(!glob_match("shot.pn", "shot.png"));
        assert!(!glob_match("shot.pngg", "shot.png"));
    }

    #[test]
    fn filter_rules_or_glob_and_mime() {
        let images = filter(
            "Images",
            &[
                ("*.png", FilterRuleKind::Glob),
                ("*.jpg", FilterRuleKind::Glob),
            ],
        );
        assert!(filter_allows(&images, Path::new("/tmp/a.png")));
        assert!(filter_allows(&images, Path::new("/tmp/b.jpg")));
        assert!(!filter_allows(&images, Path::new("/tmp/c.txt")));

        let mime = filter("Text", &[("text/plain", FilterRuleKind::Mime)]);
        assert!(mime_matches("text/*", Path::new("/tmp/a.txt")));
        assert!(filter_allows(&mime, Path::new("/tmp/a.txt")));
        assert!(!filter_allows(&mime, Path::new("/tmp/a.png")));
        assert!(!mime_matches("image/*", Path::new("/tmp/no-extension")));
    }

    #[test]
    fn breadcrumbs_walk_from_the_root() {
        assert_eq!(
            breadcrumbs(Path::new("/tmp")),
            vec![PathBuf::from("/"), PathBuf::from("/tmp")]
        );
        assert_eq!(breadcrumbs(Path::new("/")), vec![PathBuf::from("/")]);
    }

    #[test]
    fn listing_sorts_dirs_first_and_hides_dotfiles() {
        let root = std::env::temp_dir().join(format!("aegis-chooser-{}", std::process::id()));
        std::fs::create_dir_all(root.join("zeta")).unwrap();
        std::fs::create_dir_all(root.join("alpha")).unwrap();
        std::fs::write(root.join("beta.txt"), b"x").unwrap();
        std::fs::write(root.join(".hidden"), b"x").unwrap();

        let entries = list_dir(&root, false, None).unwrap();
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["alpha", "zeta", "beta.txt"]);

        let entries = list_dir(&root, true, None).unwrap();
        assert_eq!(entries.len(), 4);

        let only_txt = filter("Text", &[("*.txt", FilterRuleKind::Glob)]);
        let entries = list_dir(&root, false, Some(&only_txt)).unwrap();
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        // Directories stay visible for navigation; files are filtered.
        assert_eq!(names, ["alpha", "zeta", "beta.txt"]);

        std::fs::remove_dir_all(root).unwrap();
    }
}
