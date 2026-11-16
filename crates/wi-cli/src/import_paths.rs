//! Expand vault-relative directory arguments without following symlinks.
use std::path::{Component, Path};

use super::display_path::display_rel;

#[derive(Default)]
pub(crate) struct Expansion {
    pub paths: Vec<String>,
    pub failures: Vec<serde_json::Value>,
}

impl Expansion {
    fn failed(&mut self, path: &Path, code: &str, error: impl std::fmt::Display) {
        self.failures.push(serde_json::json!({
            "path": display_rel(path), "status": "failed", "code": code,
            "error": error.to_string(),
        }));
    }
}

pub(crate) fn expand(root: &Path, paths: &[String]) -> Expansion {
    let mut result = Expansion::default();
    for path in paths {
        if path == "." {
            walk(root, Path::new(""), &mut result);
            continue;
        }
        // Leave invalid and explicit file paths to wi-docs' authoritative validation.
        if Path::new(path)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            result.paths.push(path.clone());
            continue;
        }
        let mut current = root.to_path_buf();
        let mut directory = true;
        for part in Path::new(path).components() {
            current.push(part.as_os_str());
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                _ => {
                    directory = false;
                    break;
                }
            }
        }
        if directory && is_whole_export(root, path) {
            // A Notion or conversation export folder is converted as a whole by wi-docs.
            result.paths.push(path.clone());
        } else if directory {
            walk(root, Path::new(path), &mut result);
        } else {
            result.paths.push(path.clone());
        }
    }
    // Overlapping directories must not import a source more than once.
    let mut seen = std::collections::HashSet::new();
    result.paths.retain(|path| seen.insert(path.clone()));
    result
}

fn is_whole_export(root: &Path, rel: &str) -> bool {
    wi_docs::conversations::is_export(root, rel) || wi_docs::notion::is_export(root, rel)
}

/// `1 note` or `2 notes`: the form that agrees with the count.
fn counted(count: u64, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// One or two plain lines for a conversation import report, `None` for other imports.
pub(crate) fn conversation_summary(report: &serde_json::Value) -> Option<String> {
    let counts = report.get("conversations")?;
    let number = |key: &str| counts[key].as_u64().unwrap_or(0);
    let folder = report["sidecar_path"].as_str().unwrap_or("");
    let place = if folder.is_empty() {
        "No conversations found"
    } else {
        folder
    };
    let mut text = format!(
        "{place}: {} new, {} updated, {} unchanged.",
        number("created"),
        number("updated"),
        number("unchanged")
    );
    let mut notes = Vec::new();
    if number("conflicts") > 0 {
        let paths = counts["conflict_paths"]
            .as_array()
            .map(|p| {
                p.iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        notes.push(format!(
            "Kept {} you edited ({paths}); save your changes elsewhere to import them again.",
            counted(number("conflicts"), "note", "notes")
        ));
    }
    if number("skipped") > 0 {
        notes.push(format!(
            "Skipped {}.",
            counted(
                number("skipped"),
                "empty or unreadable item",
                "empty or unreadable items",
            )
        ));
    }
    if number("redacted") > 0 {
        notes.push(format!(
            "Replaced {} with [redacted].",
            counted(number("redacted"), "secret", "secrets")
        ));
    }
    if !notes.is_empty() {
        text.push('\n');
        text.push_str(&notes.join(" "));
    }
    Some(text)
}

fn walk(root: &Path, rel: &Path, result: &mut Expansion) {
    // Directory semantics, so an ignored `name/` is never read.
    if wi_store::ignored_dir(root, rel) {
        return;
    }
    let entries = match std::fs::read_dir(root.join(rel)) {
        Ok(entries) => entries,
        Err(error) => {
            result.failed(rel, "document-io", error);
            return;
        }
    };
    let mut sorted = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => sorted.push(entry),
            Err(error) => result.failed(rel, "document-io", error),
        }
    }
    sorted.sort_by_key(|entry| entry.file_name());
    for entry in sorted {
        let name = entry.file_name();
        let path = rel.join(&name);
        let Some(name) = name.to_str() else {
            result.failed(
                &path,
                "document-invalid",
                "document path must be valid UTF-8",
            );
            continue;
        };
        if name.starts_with('.')
            || wi_store::is_scratch_name(name)
            || wi_store::ignored(root, &path)
        {
            continue;
        }
        let kind = match entry.file_type() {
            Ok(kind) => kind,
            Err(error) => {
                result.failed(&path, "document-io", error);
                continue;
            }
        };
        if kind.is_dir() {
            let rel = display_rel(&path);
            if is_whole_export(root, &rel) {
                result.paths.push(rel);
            } else {
                // `walk` prunes a directory its ignore rules exclude.
                walk(root, &path, result);
            }
        } else if kind.is_file() && supported(&path) {
            // Each component was checked above; rel starts from a UTF-8 argument.
            result.paths.push(display_rel(&path));
        }
    }
}

fn supported(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "pdf"
                    | "docx"
                    | "pptx"
                    | "csv"
                    | "png"
                    | "jpg"
                    | "jpeg"
                    | "tif"
                    | "tiff"
                    | "bmp"
                    | "gif"
                    | "webp"
            )
        })
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn nested_directory_expansion_uses_portable_paths_and_prunes_junctions() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("docs/nested")).unwrap();
        std::fs::write(temp.path().join("docs/nested/a.csv"), "a,b\n").unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.csv"), "private\n").unwrap();
        let output = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(temp.path().join("docs").join("link"))
            .arg(outside.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let expanded = expand(temp.path(), &["docs".into()]);
        assert!(expanded.failures.is_empty());
        assert_eq!(expanded.paths, ["docs/nested/a.csv"]);
    }
}

#[cfg(test)]
mod summary_tests {
    use super::conversation_summary;
    use serde_json::json;

    fn summary(conflicts: u64, skipped: u64, redacted: u64) -> String {
        conversation_summary(&json!({
            "sidecar_path": "AI/Claude",
            "conversations": {
                "created": 1, "updated": 0, "unchanged": 0,
                "conflicts": conflicts, "conflict_paths": ["AI/Claude/a.md"],
                "skipped": skipped, "redacted": redacted
            }
        }))
        .unwrap()
    }

    #[test]
    fn one_of_each_uses_the_singular() {
        let text = summary(1, 1, 1);
        assert!(text.contains("Kept 1 note you edited"), "{text}");
        assert!(
            text.contains("Skipped 1 empty or unreadable item."),
            "{text}"
        );
        assert!(
            text.contains("Replaced 1 secret with [redacted]."),
            "{text}"
        );
    }

    #[test]
    fn several_use_the_plural() {
        let text = summary(2, 3, 4);
        assert!(text.contains("Kept 2 notes you edited"), "{text}");
        assert!(
            text.contains("Skipped 3 empty or unreadable items."),
            "{text}"
        );
        assert!(
            text.contains("Replaced 4 secrets with [redacted]."),
            "{text}"
        );
    }
}
