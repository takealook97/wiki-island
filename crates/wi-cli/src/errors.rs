//! Plain-sentence presentation of errors.

use serde_json::Value;
use wi_store::StoreError;

pub(crate) use super::error_text::Root;

/// The sentence shown for an error with no typed family.
pub(crate) const GENERIC: &str =
    "Something went wrong and wi could not finish. Run wi doctor to check your notes folder.";

/// One or two plain sentences for `error`, with the next step. Paths are
/// shown relative to `root`, or by file name; OS error text, error chains and
/// engine jargon are never shown.
pub(crate) fn plain_error(error: &anyhow::Error, root: &Root) -> String {
    if let Some(e) = error.downcast_ref::<StoreError>() {
        return store_sentence(e, root);
    }
    if let Some(e) = error.downcast_ref::<wi_docs::Error>() {
        return match e {
            wi_docs::Error::Store(e) => store_sentence(e, root),
            other => document_message(other).to_owned(),
        };
    }
    if let Some(e) = error.downcast_ref::<wi_export::install::Error>() {
        return install_sentence(e, root);
    }
    if let Some(e) = error.downcast_ref::<wi_lifecycle::RevisionError>() {
        return revision_sentence(e, root);
    }
    if let Some(e) = error.downcast_ref::<wi_lifecycle::WorkError>() {
        return match e {
            wi_lifecycle::WorkError::UnsupportedOutcomeNeedsLimits(_) => {
                "This outcome needs limits and a reopen trigger. Add them and close the item again."
                    .into()
            }
            wi_lifecycle::WorkError::AlreadyClosed => {
                "This work item is already closed. Nothing needs doing.".into()
            }
        };
    }
    if let Some(e) = error.downcast_ref::<wi_lifecycle::TimeError>() {
        return match e {
            wi_lifecycle::TimeError::Timestamp(_) => {
                "The date could not be read. Use a date like 2026-01-31 or 2026-01-31T09:00:00Z."
                    .into()
            }
            wi_lifecycle::TimeError::Duration(_) => {
                "The duration could not be read. Use a form like P30D for 30 days.".into()
            }
        };
    }
    if let Some(e) = error.downcast_ref::<std::io::Error>() {
        return bare_io_sentence(error, e, root);
    }
    if error.downcast_ref::<wi_note::ParseError>().is_some() {
        return PARSE.into();
    }
    // A message written for the owner (`bail!` and plain contexts).
    let message = error
        .downcast_ref::<&'static str>()
        .map(|m| (*m).to_owned())
        .or_else(|| error.downcast_ref::<String>().cloned());
    message
        .and_then(|m| root.sentence(&m))
        .unwrap_or_else(|| GENERIC.to_owned())
}

const PARSE: &str = "This note is not valid UTF-8 text. Open it in an editor and save it as UTF-8.";

fn store_sentence(error: &StoreError, root: &Root) -> String {
    let show = |path: &str| root.show(std::path::Path::new(path));
    match error {
        StoreError::UnsupportedPlatform => {
            "wi cannot keep notes safely on this operating system. Use macOS, Linux or Windows."
                .into()
        }
        StoreError::Protected { path, reason } if reason.contains("symlink") => format!(
            "{} is a link to another place, which wi does not follow. Use the real file or folder instead.",
            show(path)
        ),
        StoreError::Protected { path, .. } => format!(
            "{} is a protected location that wi does not change. Choose a normal note folder inside your notes folder.",
            show(path)
        ),
        StoreError::Exists { path, .. } => format!(
            "{} already exists. Pass its checksum from wi read --json with --expected-checksum to replace it.",
            show(path)
        ),
        StoreError::Stale { path, .. } => format!(
            "{} changed since it was read. Read it again and retry with the new checksum.",
            show(path)
        ),
        StoreError::NotFound { path } => format!(
            "{} does not exist. Check the name and try again.",
            show(path)
        ),
        StoreError::OperationMismatch { .. } => {
            "This operation id was already used for a different request. Use a new --op-id."
                .into()
        }
        StoreError::Locked { path } => format!(
            "{} is locked, so wi does not change it. Remove `locked: true` in an editor to unlock it.",
            show(path)
        ),
        StoreError::ConflictMarkers { path } => format!(
            "{} has unresolved merge conflicts. Resolve them in an editor, then try again.",
            show(path)
        ),
        StoreError::EngineOwnedField { field, .. } => format!(
            "The detail '{field}' is managed automatically. Leave it out of the request."
        ),
        StoreError::ClaimEditRequiresCandidate { path, .. } => format!(
            "The claim in {} cannot be changed in place. Save the revised claim as a new note so its history is kept.",
            show(path)
        ),
        StoreError::LifecycleRevertRequiresFork { path } | StoreError::TerminalNote { path } => {
            format!(
                "The claim in {} is kept as history. Restore its text as a new note.",
                show(path)
            )
        }
        StoreError::BudgetExhausted { .. } => {
            "The limit on automatic changes is used up for now, so this change was queued. It is retried automatically; nothing else is needed."
                .into()
        }
        StoreError::LifecycleRefused { message, .. } => {
            root.sentence(message).unwrap_or_else(|| {
                "This change was refused, and nothing was changed. Check the request and try again."
                    .into()
            })
        }
        StoreError::Edit(message) => match root.plain(message) {
            Some(reason) => format!(
                "The edit could not be applied ({reason}). Read the note again and adjust the request."
            ),
            None => "The edit could not be applied. Read the note again and adjust the request."
                .into(),
        },
        StoreError::Invalid(message) => match root.plain(message) {
            Some(reason) => format!(
                "The request could not be carried out ({reason}). Check the command and try again."
            ),
            None => "The request could not be carried out. Check the command and try again."
                .into(),
        },
        StoreError::Parse(_) => PARSE.into(),
        StoreError::History(_) => {
            "Your notes history could not be read or updated. Run wi doctor to check your notes folder."
                .into()
        }
        StoreError::Index(_) if error.is_query_too_long() => {
            "The search text is too long. Search for a few key words instead.".into()
        }
        StoreError::Index(_) => {
            "Search is recovering its local cache, and your notes are safe. Run the command again in a moment."
                .into()
        }
        StoreError::Config(_) => {
            "The settings file .wiki-island/config.toml could not be read. Fix the setting it names or remove the line, then try again."
                .into()
        }
        StoreError::TooLarge { path, .. } => format!(
            "{} is larger than the read limit. Open it in its own app, or raise max_note_bytes in .wiki-island/config.toml.",
            show(path)
        ),
        StoreError::Io { path, source } => io_sentence(source.kind(), &root.show(path)),
        StoreError::Watch(_) => {
            "wi could not watch your notes folder for changes. Run the command again; if it keeps happening, run wi doctor."
                .into()
        }
        StoreError::RecoveryHeld { path, .. } => format!(
            "An interrupted save of {} could not be finished automatically; other notes are unaffected. Close any app that holds the file or fix its permissions, and wi retries on the next run.",
            show(path)
        ),
        StoreError::NotInitialised(path) => format!(
            "The folder {} is not a wiki-island notes folder. Run wi init there, or pass --vault with your notes folder.",
            root.show(path)
        ),
    }
}

/// A sentence for a filesystem failure of the given kind on `what`.
fn io_sentence(kind: std::io::ErrorKind, what: &str) -> String {
    use std::io::ErrorKind;
    let fact = match kind {
        ErrorKind::PermissionDenied => format!("wi is not allowed to open {what}."),
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded => {
            format!("The disk is full, so {what} could not be saved.")
        }
        ErrorKind::NotFound => format!("{} could not be found.", subject(what)),
        _ => format!("{} could not be read or written.", subject(what)),
    };
    format!("{fact} {}", io_step(kind))
}

/// `what` opening a sentence: capitalized unless it is a path.
fn subject(what: &str) -> String {
    let mut chars = what.chars();
    match chars.next() {
        Some(first) if !what.contains(['/', '.']) => first.to_uppercase().chain(chars).collect(),
        _ => what.to_owned(),
    }
}

/// The next step for a filesystem failure of the given kind.
fn io_step(kind: std::io::ErrorKind) -> &'static str {
    use std::io::ErrorKind;
    match kind {
        ErrorKind::PermissionDenied => "Check the file's permissions, then try again.",
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded => "Free some space, then try again.",
        ErrorKind::NotFound => "Check that it exists, then try again.",
        _ => "Check that the disk is available, then try again.",
    }
}

/// A filesystem failure outside the store: the command's own description
/// of the step (its context, such as `read notes.txt`) when it is plain,
/// otherwise a general subject.
fn bare_io_sentence(error: &anyhow::Error, io: &std::io::Error, root: &Root) -> String {
    let outer = error.to_string();
    let step = (outer != io.to_string())
        .then(|| root.plain(&outer))
        .flatten();
    match step {
        Some(step) => format!("wi could not {step}. {}", io_step(io.kind())),
        None => io_sentence(io.kind(), "a file it needs"),
    }
}

const SERVICE_FAILED: &str =
    "The background service could not be changed. Run wi doctor to see what needs attention.";

fn install_sentence(error: &wi_export::install::Error, root: &Root) -> String {
    use wi_export::install::Error;
    match error {
        Error::Io(e) => io_sentence(e.kind(), "an AI app settings file"),
        Error::Config(message) | Error::Service(message) | Error::Unsupported(message) => {
            // Only a settings parse failure is a settings diagnostic; a service
            // runner's multi-line stderr is tool output and never shown.
            let multi_line = message.contains('\n');
            let settings = matches!(error, Error::Config(_))
                .then(|| multi_line_sentence(message))
                .flatten();
            let tool_output = match error {
                Error::Service(_) if multi_line => Some(SERVICE_FAILED.to_owned()),
                _ => None,
            };
            settings
                .or(tool_output)
                .or_else(|| root.sentence(message).filter(|_| !multi_line))
                .or_else(|| {
                    // Keep the owner-facing first clause, drop the tool output after it.
                    let head = message.split(": ").next()?;
                    root.sentence(head)
                        .map(|head| format!("{head} Run wi doctor to see what needs attention."))
                })
                .unwrap_or_else(|| GENERIC.to_owned())
        }
        Error::Stale => {
            "An AI app settings file changed while wi was working. Run the command again.".into()
        }
        Error::History(_) => {
            "Your notes history could not be read or updated. Run wi doctor to check your notes folder."
                .into()
        }
    }
}

/// A parser diagnostic that echoes lines of the settings file is never
/// shown. Only the Codex settings file is TOML (`config.toml`), so a TOML
/// diagnostic names that file and keeps its line and column.
fn multi_line_sentence(message: &str) -> Option<String> {
    if !message.contains('\n') {
        return None;
    }
    let first = message.lines().next().unwrap_or_default();
    let place = first
        .strip_prefix("TOML parse error ")
        .filter(|place| {
            place.starts_with("at line ")
                && place
                    .chars()
                    .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase() || " ,".contains(c))
        })
        .map(|place| format!(" ({place})"))
        .unwrap_or_default();
    Some(match first.starts_with("TOML parse error") {
        true => format!(
            "The settings file config.toml of an AI app is not valid{place}. Fix or remove that line, then run the command again."
        ),
        false => "A settings file of an AI app is not valid. Fix or remove it, then run the command again."
            .into(),
    })
}

fn revision_sentence(error: &wi_lifecycle::RevisionError, root: &Root) -> String {
    use wi_lifecycle::RevisionError;
    match error {
        RevisionError::EngineOwned(field) => {
            format!("The detail '{field}' is managed automatically. Leave it out of the revision.")
        }
        RevisionError::NotClaimContent(field) => {
            format!("'{field}' is not part of the claim. Change it with a normal edit instead.")
        }
        RevisionError::Empty => "The revision changes nothing. Give the new claim text.".into(),
        RevisionError::Edit(message) => match root.plain(message) {
            Some(reason) => format!(
                "The revision could not be applied ({reason}). Read the note again and adjust it."
            ),
            None => "The revision could not be applied. Read the note again and adjust it.".into(),
        },
    }
}

pub(crate) fn hint(error: &StoreError) -> Option<&'static str> {
    match error {
        StoreError::NotInitialised(_) => {
            Some("run `wi init` in the vault directory or pass --vault")
        }
        StoreError::Exists { .. } => {
            Some("run `wi read <path> --json` and pass its checksum with --expected-checksum")
        }
        StoreError::Stale { .. } => Some("read the note again and retry with the new checksum"),
        StoreError::Locked { .. } => Some("edit the file directly to remove `locked: true`"),
        StoreError::ConflictMarkers { .. } => Some("resolve the git conflict markers in an editor"),
        StoreError::Protected { .. } => Some("choose a normal note folder inside your vault"),
        StoreError::EngineOwnedField { .. } => {
            Some("leave automatically managed details out of the request")
        }
        StoreError::ClaimEditRequiresCandidate { .. } => {
            Some("save the corrected claim as a new note and link it to the previous claim")
        }
        StoreError::LifecycleRevertRequiresFork { .. } => {
            Some("copy the earlier text into a new note so its history is kept")
        }
        StoreError::OperationMismatch { .. } => Some("use a new --op-id for a different request"),
        StoreError::Index(_) if error.is_query_too_long() => None,
        StoreError::Index(_) => Some("search will recover automatically on the next run"),
        StoreError::History(_) => Some("run `wi doctor`"),
        // The sentence already holds the next step.
        StoreError::UnsupportedPlatform
        | StoreError::NotFound { .. }
        | StoreError::TerminalNote { .. }
        | StoreError::BudgetExhausted { .. }
        | StoreError::LifecycleRefused { .. }
        | StoreError::Edit(_)
        | StoreError::Invalid(_)
        | StoreError::Parse(_)
        | StoreError::Config(_)
        | StoreError::TooLarge { .. }
        | StoreError::Io { .. }
        | StoreError::Watch(_)
        | StoreError::RecoveryHeld { .. } => None,
    }
}

/// Carries ingest diagnostics through anyhow contexts without erasing typed errors.
#[derive(Debug)]
pub(crate) struct IngestWarnings(pub(crate) Vec<Value>);
impl std::fmt::Display for IngestWarnings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("command failed with ingest warnings")
    }
}

pub(crate) fn document_message(error: &wi_docs::Error) -> &'static str {
    match error {
        wi_docs::Error::Io(e) if e.kind() == std::io::ErrorKind::NotFound => {
            "The source file was not found. Check its name and location."
        }
        wi_docs::Error::Store(StoreError::NotFound { .. }) => {
            "The source file was not found. Check its name and location."
        }
        wi_docs::Error::Invalid(reason) if reason.contains("no page identifiers") => {
            "This is not a supported notes export. Export pages as Markdown and CSV, then import that folder or zip file."
        }
        wi_docs::Error::Conflict(_) => {
            "An imported file was edited. Its contents were kept; save your edits separately before importing again."
        }
        wi_docs::Error::Io(_) | wi_docs::Error::Store(StoreError::Io { .. }) => {
            "The source could not be read. Check that it is available and readable."
        }
        wi_docs::Error::Extraction(_) => {
            "The document could not be converted. Check that the file opens correctly and try again."
        }
        _ => {
            "The import could not finish. Check that the source is a supported document or a Markdown and CSV notes export."
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Error as IoError, ErrorKind};
    use std::path::{Path, PathBuf};

    use super::super::error_text::has_jargon;
    use super::*;

    /// Number of `StoreError` variants; `ordinal` must name each one.
    const VARIANTS: usize = 25;

    /// A new `StoreError` variant fails to compile here until it is given an
    /// ordinal, and the coverage check below fails until it has a sample.
    fn ordinal(error: &StoreError) -> usize {
        match error {
            StoreError::UnsupportedPlatform => 0,
            StoreError::Protected { .. } => 1,
            StoreError::Exists { .. } => 2,
            StoreError::Stale { .. } => 3,
            StoreError::NotFound { .. } => 4,
            StoreError::OperationMismatch { .. } => 5,
            StoreError::Locked { .. } => 6,
            StoreError::ConflictMarkers { .. } => 7,
            StoreError::EngineOwnedField { .. } => 8,
            StoreError::ClaimEditRequiresCandidate { .. } => 9,
            StoreError::TerminalNote { .. } => 10,
            StoreError::LifecycleRevertRequiresFork { .. } => 11,
            StoreError::BudgetExhausted { .. } => 12,
            StoreError::LifecycleRefused { .. } => 13,
            StoreError::Edit(_) => 14,
            StoreError::Invalid(_) => 15,
            StoreError::Parse(_) => 16,
            StoreError::History(_) => 17,
            StoreError::Index(_) => 18,
            StoreError::Config(_) => 19,
            StoreError::Io { .. } => 20,
            StoreError::TooLarge { .. } => 21,
            StoreError::Watch(_) => 22,
            StoreError::RecoveryHeld { .. } => 23,
            StoreError::NotInitialised(_) => 24,
        }
    }

    fn io(path: PathBuf, kind: ErrorKind) -> StoreError {
        StoreError::Io {
            path,
            source: IoError::new(kind, format!("{kind} (os error 13)")),
        }
    }

    /// Index errors come from a real vault: the crate has no direct index
    /// dependency on every platform.
    fn index_errors(root: &Path) -> Vec<StoreError> {
        let options = wi_store::OpenOptions {
            index_path: Some(root.with_extension("index.db")),
        };
        let (vault, _) = wi_store::Vault::init_with(root, options).unwrap();
        let over = wi_store::Query::text("a".repeat(4096));
        let too_long = vault
            .search(&over, wi_store::SearchOptions::default())
            .unwrap_err();
        let depth = vault.context("anything", 99).unwrap_err();
        vec![too_long, depth]
    }

    fn samples(temp: &Path, root: &Path) -> Vec<StoreError> {
        let inside = |rel: &str| root.join(rel).display().to_string();
        let outside = temp.join("elsewhere").join("x.md");
        let bytes = vec![0xff_u8];
        let utf8 = std::str::from_utf8(&bytes).unwrap_err();
        let mut all = vec![
            StoreError::UnsupportedPlatform,
            StoreError::Protected {
                path: inside("link/a.md"),
                reason: "symlink",
            },
            StoreError::Protected {
                path: inside(".wiki-island/x"),
                reason: "engine file, not a note",
            },
            StoreError::Exists {
                path: "notes/a.md".into(),
                current: "abc".into(),
            },
            StoreError::Stale {
                path: "notes/a.md".into(),
                expected: "a".into(),
                current: None,
            },
            StoreError::NotFound {
                path: "notes/missing.md".into(),
            },
            StoreError::OperationMismatch { op_id: "op".into() },
            StoreError::Locked {
                path: "notes/a.md".into(),
            },
            StoreError::ConflictMarkers {
                path: "notes/a.md".into(),
            },
            StoreError::EngineOwnedField {
                field: "created".into(),
                ingress: "cli",
            },
            StoreError::ClaimEditRequiresCandidate {
                path: "notes/a.md".into(),
                fields: vec!["claim".into()],
            },
            StoreError::TerminalNote {
                path: "notes/a.md".into(),
            },
            StoreError::LifecycleRevertRequiresFork {
                path: "notes/a.md".into(),
            },
            StoreError::BudgetExhausted {
                retry_at: Some("2026-01-01T00:00:00Z".into()),
                spent: 3,
                work: Some(inside(".wiki-island/work/w.md")),
            },
            StoreError::LifecycleRefused {
                code: "frontmatter-invalid",
                message: format!("{} has an unreadable header", inside("notes/a.md")),
            },
            StoreError::LifecycleRefused {
                code: "x",
                message: "lifecycle note is terminal; fork it".into(),
            },
            StoreError::Edit(format!("{} cannot be edited", outside.display())),
            StoreError::Edit("cannot keep engine-owned key".into()),
            StoreError::Invalid(format!("{} is not a regular file", inside("notes/dir"))),
            StoreError::Invalid(format!("manifest seq 4 at {}", outside.display())),
            StoreError::Parse(wi_note::ParseError::Utf8(utf8)),
            StoreError::History(wi_history::HistoryError::Io {
                path: root.join(".git/wi/manifest.log"),
                source: IoError::from_raw_os_error(2),
            }),
            StoreError::Config(wi_note::ConfigError::Field("max_note_bytes".into())),
            StoreError::TooLarge {
                path: "files/big.pdf".into(),
                size: 9,
                limit: 8,
            },
            StoreError::TooLarge {
                path: inside("files/big.pdf"),
                size: 9,
                limit: 8,
            },
            StoreError::Watch(format!("watch failed at {}: os error 24", inside("notes"))),
            StoreError::RecoveryHeld {
                path: "notes/a.md".into(),
                seq: 7,
            },
            StoreError::NotInitialised(root.to_path_buf()),
            StoreError::NotInitialised(outside.clone()),
        ];
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::StorageFull,
            ErrorKind::NotFound,
            ErrorKind::Other,
        ] {
            all.push(io(root.join("notes/a.md"), kind));
            all.push(io(outside.clone(), kind));
        }
        all.push(StoreError::Io {
            path: root.join("notes/b.md"),
            source: IoError::from_raw_os_error(2),
        });
        all.extend(index_errors(root));
        all
    }

    fn assert_plain(text: &str, temp: &Path, context: &str) {
        let temp_text = temp.display().to_string();
        let resolved = std::fs::canonicalize(temp)
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        assert!(!text.is_empty(), "{context}");
        assert!(text.ends_with('.'), "{context}: {text}");
        assert!(!text.contains("os error"), "{context}: {text}");
        assert!(!text.contains("No such file"), "{context}: {text}");
        assert!(!text.contains("Permission denied"), "{context}: {text}");
        assert!(!text.contains(&temp_text), "{context}: {text}");
        assert!(
            resolved.is_empty() || !text.contains(&resolved),
            "{context}: {text}"
        );
        // `: ` joins nested errors in a chain; the frontmatter key is quoted.
        let unquoted = super::super::error_text::outside_quotes(text);
        assert!(!unquoted.contains(": "), "{context}: {text}");
        assert!(!text.contains("while running"), "{context}: {text}");
        assert!(!has_jargon(text), "{context}: {text}");
        assert!(!text.contains("StoreError"), "{context}: {text}");
    }

    #[test]
    fn every_store_error_renders_as_a_plain_sentence_without_internals() {
        let temp = tempfile::TempDir::new().unwrap();
        let root_path = temp.path().join("vault");
        let root = Root::new(&root_path);
        let errors = samples(temp.path(), &root_path);
        let mut covered = [false; VARIANTS];
        for error in errors {
            covered[ordinal(&error)] = true;
            let debug = format!("{error:?}");
            let wrapped = anyhow::Error::new(error).context("while running the command");
            let text = plain_error(&wrapped, &root);
            assert_plain(&text, temp.path(), &debug);
            if let Some(hint) = wrapped.downcast_ref::<StoreError>().and_then(hint) {
                assert!(!has_jargon(hint), "{debug}: {hint}");
            }
        }
        let missing: Vec<usize> = (0..VARIANTS).filter(|i| !covered[*i]).collect();
        assert!(missing.is_empty(), "variants without a sample: {missing:?}");
    }

    #[test]
    fn paths_are_shown_relative_to_the_notes_folder_or_by_name() {
        let temp = tempfile::TempDir::new().unwrap();
        let root_path = temp.path().join("vault");
        let root = Root::new(&root_path);
        let inside = io(root_path.join("notes/a.md"), ErrorKind::PermissionDenied);
        let text = plain_error(&anyhow::Error::new(inside), &root);
        assert_eq!(
            text,
            "wi is not allowed to open notes/a.md. Check the file's permissions, then try again."
        );
        let outside = io(temp.path().join("other/b.md"), ErrorKind::StorageFull);
        let text = plain_error(&anyhow::Error::new(outside), &root);
        assert!(
            text.starts_with("The disk is full, so b.md could not"),
            "{text}"
        );
        let query = index_errors(&temp.path().join("v2")).remove(0);
        assert_eq!(query.code(), "invalid-request");
        assert_eq!(
            plain_error(&anyhow::Error::new(query), &root),
            "The search text is too long. Search for a few key words instead."
        );
    }

    #[test]
    fn typed_families_keep_their_own_message_and_others_get_the_generic_sentence() {
        let root = Root::default();
        let ownership = "The background task with this name was not created by wiki-island; remove it in Task Scheduler.";
        let service = anyhow::Error::new(wi_export::install::Error::Service(ownership.into()))
            .context("service uninstall");
        assert_eq!(plain_error(&service, &root), ownership);
        let refused = wi_export::install::Error::Config(
            "Task Scheduler could not delete the background task: ERROR: Access is denied.".into(),
        );
        assert_eq!(
            plain_error(&anyhow::Error::new(refused), &root),
            "Task Scheduler could not delete the background task. Run wi doctor to see what needs attention."
        );
        assert_eq!(
            plain_error(
                &anyhow::Error::new(wi_lifecycle::WorkError::AlreadyClosed),
                &root
            ),
            "This work item is already closed. Nothing needs doing."
        );
        let owner_message =
            anyhow::anyhow!("Use the Notion page link or its 32-character page id.");
        assert_eq!(
            plain_error(&owner_message, &root),
            "Use the Notion page link or its 32-character page id."
        );
        let json = serde_json::from_str::<Value>("{").unwrap_err();
        assert_eq!(plain_error(&anyhow::Error::new(json), &root), GENERIC);
        let jargon = anyhow::anyhow!("manifest seq 3 is held");
        assert_eq!(plain_error(&jargon, &root), GENERIC);
        let io = anyhow::Error::new(IoError::from(ErrorKind::PermissionDenied))
            .context("read standard input");
        assert_eq!(
            plain_error(&io, &root),
            "wi could not read standard input. Check the file's permissions, then try again."
        );
    }
}
