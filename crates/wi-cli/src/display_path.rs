//! Display form of a vault-relative path: `/` separators on every platform.
use std::path::Path;

/// `rel` joined by `/`, whatever the platform separator is. Only for paths
/// relative to the vault; absolute paths shown to the owner keep native
/// separators and do not pass through here.
pub(crate) fn display_rel(rel: &Path) -> String {
    rel.components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn joins_components_with_forward_slashes() {
        let rel: PathBuf = ["notes", "people", "a.md"].iter().collect();
        assert_eq!(display_rel(&rel), "notes/people/a.md");
    }

    #[test]
    fn single_component_is_unchanged() {
        assert_eq!(display_rel(Path::new("a.md")), "a.md");
    }

    #[test]
    fn empty_path_is_empty() {
        assert_eq!(display_rel(Path::new("")), "");
    }

    #[cfg(windows)]
    #[test]
    fn backslash_separated_input_becomes_forward_slashes() {
        assert_eq!(display_rel(Path::new("a\\b\\c.md")), "a/b/c.md");
    }
}
