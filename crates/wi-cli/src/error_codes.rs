//! Stable CLI codes for errors outside the store's own code contract.
use wi_lifecycle::{RevisionError, TimeError, WorkError};

/// Codes added by the CLI. Store errors retain `StoreError::code()`.
const ERROR_CODES: &[&str] = &[
    "document-io",
    "document-json",
    "document-invalid",
    "document-extraction",
    "document-conflict",
    "install-io",
    "install-config",
    "install-stale",
    "install-history",
    "install-unsupported",
    "service",
    "lifecycle-engine-owned",
    "lifecycle-not-claim-content",
    "lifecycle-empty-revision",
    "lifecycle-edit",
    "lifecycle-outcome-needs-limits",
    "lifecycle-already-closed",
    "lifecycle-timestamp",
    "lifecycle-duration",
    "io",
    "json",
    "error",
];

pub(crate) fn document(error: &wi_docs::Error) -> &'static str {
    match error {
        wi_docs::Error::Store(e) => e.code(),
        wi_docs::Error::Io(_) => "document-io",
        wi_docs::Error::Json(_) => "document-json",
        wi_docs::Error::Invalid(_) => "document-invalid",
        wi_docs::Error::Extraction(_) => "document-extraction",
        wi_docs::Error::Conflict(_) => "document-conflict",
    }
}

pub(crate) fn code(error: &anyhow::Error) -> &'static str {
    if let Some(e) = error.downcast_ref::<wi_docs::Error>() {
        return document(e);
    }
    let code = if let Some(e) = error.downcast_ref::<wi_export::install::Error>() {
        use wi_export::install::Error;
        match e {
            Error::Io(_) => "install-io",
            Error::Config(_) => "install-config",
            Error::Stale => "install-stale",
            Error::History(_) => "install-history",
            Error::Unsupported(_) => "install-unsupported",
            Error::Service(_) => "service",
        }
    } else if let Some(e) = error.downcast_ref::<RevisionError>() {
        match e {
            RevisionError::EngineOwned(_) => "lifecycle-engine-owned",
            RevisionError::NotClaimContent(_) => "lifecycle-not-claim-content",
            RevisionError::Empty => "lifecycle-empty-revision",
            RevisionError::Edit(_) => "lifecycle-edit",
        }
    } else if let Some(e) = error.downcast_ref::<WorkError>() {
        match e {
            WorkError::UnsupportedOutcomeNeedsLimits(_) => "lifecycle-outcome-needs-limits",
            WorkError::AlreadyClosed => "lifecycle-already-closed",
        }
    } else if let Some(e) = error.downcast_ref::<TimeError>() {
        match e {
            TimeError::Timestamp(_) => "lifecycle-timestamp",
            TimeError::Duration(_) => "lifecycle-duration",
        }
    } else if error.downcast_ref::<std::io::Error>().is_some() {
        "io"
    } else if error.downcast_ref::<serde_json::Error>().is_some() {
        "json"
    } else {
        "error"
    };
    debug_assert!(ERROR_CODES.contains(&code));
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stable_codes_cover_each_new_error_family_through_context() {
        for (error, expected) in [
            (
                anyhow::Error::new(wi_docs::Error::Invalid("bad".into())),
                "document-invalid",
            ),
            (
                anyhow::Error::new(RevisionError::Empty),
                "lifecycle-empty-revision",
            ),
            (
                anyhow::Error::new(WorkError::AlreadyClosed),
                "lifecycle-already-closed",
            ),
            (
                anyhow::Error::new(TimeError::Duration("bad".into())),
                "lifecycle-duration",
            ),
            (
                anyhow::Error::new(wi_export::install::Error::Config("bad".into())),
                "install-config",
            ),
            (
                anyhow::Error::new(wi_export::install::Error::Service("bad".into())),
                "service",
            ),
        ] {
            assert_eq!(code(&error.context("command failed")), expected);
            assert!(ERROR_CODES.contains(&expected));
        }
    }
}
