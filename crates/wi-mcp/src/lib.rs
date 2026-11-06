//! MCP stdio surface over wi-store. No HTTP, model, or network query path.
#![forbid(unsafe_code)]

mod brief;
mod density;
mod dependencies;
mod engine;
mod frontmatter;
pub use frontmatter::validate_frontmatter;
pub mod provider;
mod result;
mod schema;
mod section;
mod server;
mod window;

pub use brief::{BRIEF_MAX_BYTES, BRIEF_URI, brief};
pub use engine::Engine;
pub use provider as lifecycle;
pub use provider::*;
pub use result::{MAX_BYTES, MAX_HITS};
pub use schema::tools;
pub use server::{Runtime, Server, ServerError, serve_stdio, serve_stdio_with};

#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod test_support;
