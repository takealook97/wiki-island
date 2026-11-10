//! Keeping filesystem internals and engine jargon out of error sentences.

use std::path::{Path, PathBuf};

/// Words that name engine internals rather than anything the owner sees.
const JARGON: &[&str] = &[
    "engine-owned",
    "ingress",
    "fork",
    "terminal",
    "lifecycle note",
    "manifest",
];

/// The notes folder an error is presented against, in the forms a message
/// may spell it: as given and resolved.
#[derive(Debug, Default)]
pub(crate) struct Root {
    forms: Vec<PathBuf>,
}

impl Root {
    pub(crate) fn new(root: &Path) -> Self {
        let mut forms = Vec::new();
        for form in [
            std::path::absolute(root).ok(),
            std::fs::canonicalize(root).ok(),
        ]
        .into_iter()
        .flatten()
        {
            if !forms.contains(&form) {
                forms.push(form);
            }
        }
        Self { forms }
    }

    /// `path` relative to the notes folder when it is inside it, else only
    /// its file name.
    pub(crate) fn show(&self, path: &Path) -> String {
        if path.is_relative() {
            return path.display().to_string();
        }
        for form in &self.forms {
            if path == form {
                return name_of(path);
            }
            if let Ok(rel) = path.strip_prefix(form) {
                return rel.to_string_lossy().replace('\\', "/");
            }
        }
        name_of(path)
    }

    /// Free text with every absolute path replaced: by its vault-relative
    /// form inside the notes folder, otherwise by its file name.
    pub(crate) fn clean(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for form in &self.forms {
            let shown = form.display().to_string();
            for separator in ['/', '\\'] {
                text = text.replace(&format!("{shown}{separator}"), "");
            }
            text = text.replace(&shown, "your notes folder");
        }
        text.split_inclusive(char::is_whitespace)
            .map(clean_token)
            .collect()
    }

    /// `clean(text)` when it reads as owner-facing text: no engine jargon,
    /// no OS error text and no `: ` joining nested errors. `None` otherwise,
    /// so the caller falls back to a fixed sentence.
    pub(crate) fn plain(&self, text: &str) -> Option<String> {
        let cleaned = self.clean(text);
        let trimmed = cleaned.trim();
        let internal = trimmed.is_empty()
            || has_jargon(trimmed)
            || outside_quotes(trimmed).contains(": ")
            || trimmed.contains("os error");
        (!internal).then(|| trimmed.to_owned())
    }

    /// `plain(text)` as a sentence: a capital first letter (unless it opens
    /// with a path) and a final stop.
    pub(crate) fn sentence(&self, text: &str) -> Option<String> {
        let plain = self.plain(text)?;
        let opens_with_path = plain
            .split_whitespace()
            .next()
            .is_some_and(|word| word.contains(['/', '.']));
        let mut chars = plain.chars();
        let first = chars.next()?;
        let mut out: String = if opens_with_path {
            plain.clone()
        } else {
            first.to_uppercase().chain(chars).collect()
        };
        if !out.ends_with(['.', '!', '?']) {
            out.push('.');
        }
        Some(out)
    }
}

fn name_of(path: &Path) -> String {
    path.file_name().map_or_else(
        || "a file".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// `text` without its quoted spans (`'…'`, `"…"`, `` `…` ``), so a quoted
/// example such as `': '` is not read as two joined errors. A quote opens
/// only at the start or after whitespace or `(`.
pub(crate) fn outside_quotes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut open: Option<char> = None;
    let mut previous = ' ';
    for c in text.chars() {
        match open {
            Some(quote) if c == quote => open = None,
            Some(_) => {}
            None if matches!(c, '\'' | '"' | '`')
                && (previous.is_whitespace() || previous == '(') =>
            {
                open = Some(c);
            }
            None => out.push(c),
        }
        previous = c;
    }
    out
}

/// Whether `text` names an engine internal.
pub(crate) fn has_jargon(text: &str) -> bool {
    let lower = text.to_lowercase();
    JARGON.iter().any(|word| lower.contains(word))
        || lower
            .split(|c: char| !c.is_alphanumeric())
            .any(|word| word == "seq")
}

/// Whether `text` starts with an absolute path on any platform.
fn is_absolute(text: &str) -> bool {
    let bytes = text.as_bytes();
    text.starts_with('/')
        || text.starts_with("\\\\")
        || (bytes.len() > 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/'))
}

/// One whitespace-terminated token with an absolute path replaced by its
/// file name, keeping surrounding punctuation.
fn clean_token(token: &str) -> String {
    let body = token.trim_end_matches(char::is_whitespace);
    let space = &token[body.len()..];
    let start = body
        .find(|c: char| !matches!(c, '(' | '[' | '"' | '\'' | '`'))
        .unwrap_or(body.len());
    let (lead, rest) = body.split_at(start);
    let core = rest.trim_end_matches(|c: char| {
        matches!(c, ')' | ']' | '"' | '\'' | '`' | ',' | ';' | ':' | '.')
    });
    if !is_absolute(core) {
        return token.to_owned();
    }
    let tail = &rest[core.len()..];
    let name = core
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or("a file");
    format!("{lead}{name}{tail}{space}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_inside_the_folder_become_relative_and_others_keep_their_name() {
        let temp = std::env::temp_dir().join("wi-error-text");
        let root = Root::new(&temp);
        let inside = temp.join("notes").join("a.md");
        assert_eq!(root.show(&inside), "notes/a.md");
        assert_eq!(root.show(Path::new("/elsewhere/b.md")), "b.md");
        assert_eq!(root.show(Path::new("notes/c.md")), "notes/c.md");
        let text = format!("{} is not a regular file", inside.display());
        assert_eq!(root.clean(&text), "notes/a.md is not a regular file");
        assert_eq!(
            root.clean("open (/var/x/y.md), then C:\\Users\\o\\z.md."),
            "open (y.md), then z.md."
        );
    }

    #[test]
    fn jargon_is_detected_as_words() {
        assert!(has_jargon("field is engine-owned"));
        assert!(has_jargon("held by seq 4"));
        assert!(!has_jargon("a subsequent save"));
        assert!(
            Root::default()
                .plain("cannot keep engine-owned x")
                .is_none()
        );
        assert_eq!(
            Root::default().plain(" find text is empty ").as_deref(),
            Some("find text is empty")
        );
        assert!(
            Root::default()
                .plain("observation: bad (os error 2)")
                .is_none()
        );
        assert_eq!(
            Root::default().sentence("try again in a moment").as_deref(),
            Some("Try again in a moment.")
        );
    }
}
