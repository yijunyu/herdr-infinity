//! Shared internals for the Herdr integration binaries.
//!
//! `herdr-infinity` and `herdr-letta` both report pane state to Herdr and both
//! read an agent runtime's on-disk state, so the pieces live here rather than
//! being included twice with `#[path]`. Compiling them once also lets
//! dead-code analysis see every caller.

pub mod herdr;
pub mod infinity;
pub mod letta;
pub mod memfs;
pub mod plugin;
pub mod sessions;
pub mod shim;

use std::path::PathBuf;

/// The user's home directory, or `/` when HOME is unset.
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}
