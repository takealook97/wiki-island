//! Read-only evidence views and host registration, built on current eligibility.
#![forbid(unsafe_code)]
pub mod hosts;
pub mod install;
pub mod local;
pub mod rules;
pub mod skills;
pub use rules::{Host, Regenerator, export_rules};
