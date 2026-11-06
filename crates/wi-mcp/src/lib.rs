//! MCP stdio surface over wi-store. No HTTP, model, or network query path.
#![forbid(unsafe_code)]

mod density;
mod dependencies;
mod engine;
mod frontmatter;
pub use frontmatter::validate_frontmatter;
pub mod provider;
mod result;
mod schema;
mod section;
mod window;

pub use engine::Engine;
pub use provider as lifecycle;
pub use provider::*;
pub use result::{MAX_BYTES, MAX_HITS};
pub use schema::tools;

#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod test_support;
