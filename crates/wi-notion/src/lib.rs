//! One-way publication of caller-supplied committed notes into a read-only view.
//! This crate is not used on capture, search, read, or MCP paths.
#![forbid(unsafe_code)]

mod client;
mod mapper;
mod mapstore;
mod publish;
mod token;

pub use mapper::{MappedNote, map_markdown};
pub use mapstore::{MapEntry, MapStore};
pub use publish::{
    NoteSource, PublishFailure, PublishNote, PublishOptions, PublishReport, publish,
};
pub use token::{KeychainTokenSource, TokenSource};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Could not read the committed notes: {0}")]
    Source(String),
    #[error("Could not save the publication map: {0}")]
    Io(#[from] std::io::Error),
    #[error("The publication map or response is invalid. Check the publisher state.")]
    Json(#[from] serde_json::Error),
    #[error("The note is not valid UTF-8.")]
    Utf8,
    #[error("The publisher settings are invalid: {0}")]
    Options(&'static str),
    #[error("Could not read the publishing token from the keychain. Set it up again.")]
    Token,
    #[error("The publishing request could not reach the service. Try again later.")]
    Transport,
    #[error("The publishing service returned HTTP {0}. Check access or try again later.")]
    Http(u16),
    #[error("The publishing service returned an invalid response. Try again later.")]
    Response,
    #[error("The note exceeds a publishing limit: {0}")]
    Limit(&'static str),
    #[error("The publication map belongs to another parent page. Use a separate map.")]
    Parent,
}

pub type Result<T> = std::result::Result<T, Error>;
