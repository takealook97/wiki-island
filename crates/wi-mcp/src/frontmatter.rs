//! Shared, content-free diagnostics for note creation on CLI and MCP.
use wi_note::{Note, ReadOnlyReason};
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
    let text = std::str::from_utf8(bytes).unwrap_or_default();
    let opening = text[..front.content_range.start].lines().count();
    let (line, hint) = match reason {
        ReadOnlyReason::InvalidYaml(detail) => {
            let line = detail
                .split("line ")
                .nth(1)
                .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(1);
            (
                opening + line,
                "Check YAML indentation and quote values containing ': '.",
            )
        }
        ReadOnlyReason::DuplicateKey(_) => (opening + 1, "Remove the duplicate metadata key."),
        ReadOnlyReason::Unterminated => (
            opening,
            "Close the metadata block with a line containing --- .",
        ),
        _ => (
            opening + 1,
            "Use a simple YAML mapping with one key per line; avoid aliases and flow mappings.",
        ),
    };
    Err(StoreError::LifecycleRefused {
        code: "frontmatter-invalid",
        message: format!("Invalid frontmatter near line {line}. {hint}"),
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
