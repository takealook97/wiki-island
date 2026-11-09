//! Plain-sentence presentation of errors.

use serde_json::Value;
use wi_store::StoreError;

pub(crate) fn plain_error(error: &anyhow::Error) -> String {
    match error.downcast_ref::<StoreError>() {
        Some(StoreError::EngineOwnedField { field, .. }) => {
            format!("The detail '{field}' is managed automatically.")
        }
        Some(StoreError::ClaimEditRequiresCandidate { path, .. }) => {
            format!("Save the revised claim in {path} as a new note so its history is kept.")
        }
        Some(
            StoreError::LifecycleRevertRequiresFork { path } | StoreError::TerminalNote { path },
        ) => format!("The claim in {path} is kept as history; restore its text as a new note."),
        Some(StoreError::Index(_)) => {
            "Search is recovering its local cache. Your notes are safe.".into()
        }
        Some(StoreError::LifecycleRefused { message, .. }) => message.clone(),
        _ => format!("{error:#}"),
    }
}

pub(crate) fn hint(error: &StoreError) -> Option<&'static str> {
    Some(match error {
        StoreError::NotInitialised(_) => "run `wi init` in the vault directory or pass --vault",
        StoreError::Exists { .. } => {
            "run `wi read <path> --json` and pass its checksum with --expected-checksum"
        }
        StoreError::Stale { .. } => "read the note again and retry with the new checksum",
        StoreError::Locked { .. } => "edit the file directly to remove `locked: true`",
        StoreError::ConflictMarkers { .. } => "resolve the git conflict markers in an editor",
        StoreError::Protected { .. } => "choose a normal note folder inside your vault",
        StoreError::EngineOwnedField { .. } => {
            "leave automatically managed details out of the request"
        }
        StoreError::ClaimEditRequiresCandidate { .. } => {
            "save the corrected claim as a new note and link it to the previous claim"
        }
        StoreError::LifecycleRevertRequiresFork { .. } => {
            "copy the earlier text into a new note so its history is kept"
        }
        StoreError::OperationMismatch { .. } => "use a new --op-id for a different request",
        StoreError::Index(_) => "search will recover automatically on the next run",
        StoreError::History(_) => "run `wi doctor`",
        _ => return None,
    })
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
