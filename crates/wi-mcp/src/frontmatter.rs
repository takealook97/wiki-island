//! Shared, content-free diagnostics for note creation on CLI and MCP.
use wi_note::Note;
use wi_store::StoreError;

/// Validate that provenance can be recorded without rewriting unsupported YAML.
pub fn validate_frontmatter(bytes: &[u8]) -> wi_store::Result<()> {
    let note = Note::parse(bytes)?;
    let Some(front) = note.frontmatter() else {
        return Ok(());
    };
    let Err(reason) = front.is_editable() else {
        return Ok(());
    };
    Err(StoreError::LifecycleRefused {
        code: "frontmatter-invalid",
        message: reason.owner_sentence_at(front.opening_lines()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_create_names_the_line_and_fix_without_echoing_content() {
        let error =
            validate_frontmatter(b"---\nsummary: Release: secret detail\n---\nbody").unwrap_err();
        assert_eq!(error.code(), "frontmatter-invalid");
        assert!(error.to_string().contains("line 2"), "{error}");
        assert!(error.to_string().contains("quote values"));
        assert!(!error.to_string().contains("secret detail"));
        validate_frontmatter(b"---\nsummary: 'Release: details'\n---\nbody").unwrap();
    }
}
