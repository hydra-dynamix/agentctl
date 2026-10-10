//! Library surface for agentctl.
//!
//! The library uses `anyhow` for now because early agentctl errors mostly need
//! path and parsing context; typed errors can be introduced when callers need
//! to match on stable failure categories.

pub mod config;
pub mod exec;
pub mod jobs;
pub mod output;
mod process;
pub mod provider;
pub mod summary;
#[cfg(windows)]
pub mod windows_process;
