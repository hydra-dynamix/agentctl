//! Library surface for agentctl.
//!
//! The library uses `anyhow` for now because early agentctl errors mostly need
//! path and parsing context; typed errors can be introduced when callers need
//! to match on stable failure categories.

pub mod capture;
pub mod compatibility;
pub mod config;
pub mod discovery;
pub mod exec;
pub mod github_app;
pub mod jobs;
pub mod output;
pub mod platform;
mod process;
pub mod provider;
pub mod summary;
pub mod task;
#[cfg(windows)]
pub mod windows_process;

#[cfg(target_os = "linux")]
mod linux_process;
