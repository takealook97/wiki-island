//! MCP stdio surface over wi-store. No HTTP, model, or network query path.
#![forbid(unsafe_code)]

mod density;
mod dependencies;
mod frontmatter;
pub use frontmatter::validate_frontmatter;
mod schema;

pub use schema::tools;

