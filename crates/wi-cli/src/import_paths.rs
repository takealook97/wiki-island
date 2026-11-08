//! Expand vault-relative directory arguments without following symlinks.
use std::path::{Component, Path};

#[derive(Default)]
pub(crate) struct Expansion {
    pub paths: Vec<String>,
    pub failures: Vec<serde_json::Value>,
}

impl Expansion {
    fn failed(&mut self, path: &Path, code: &str, error: impl std::fmt::Display) {
        self.failures.push(serde_json::json!({
            "path": path.to_string_lossy(), "status": "failed", "code": code,
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
        if directory && wi_docs::notion::is_export(root, path) {
            // A Notion export folder is converted as a whole by wi-docs.
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
            let rel = relative_string(&path);
            if wi_docs::notion::is_export(root, &rel) {
                result.paths.push(rel);
            } else {
                // `walk` prunes a directory its ignore rules exclude.
                walk(root, &path, result);
            }
        } else if kind.is_file() && supported(&path) {
            // Each component was checked above; rel starts from a UTF-8 argument.
            result.paths.push(relative_string(&path));
        }
    }
}

fn relative_string(path: &Path) -> String {
    #[cfg(windows)]
    {
        path.to_string_lossy().replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy().into_owned()
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
            .arg(temp.path().join("docs/link"))
            .arg(outside.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let expanded = expand(temp.path(), &["docs".into()]);
        assert!(expanded.failures.is_empty());
        assert_eq!(expanded.paths, ["docs/nested/a.csv"]);
    }
}
