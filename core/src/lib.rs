//! ocs_core: scan ~/.config, snapshot per-app into a git repo, apply back with backup.
//! All functions take explicit paths. No globals, no deletes, copy-only.

pub mod apps;
pub mod git;
pub mod scanner;
pub mod store;
pub mod theme;

pub use apps::{builtin_apps, AppSpec};
