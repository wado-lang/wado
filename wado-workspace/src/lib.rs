//! Filesystem-backed discovery for Wado projects.
//!
//! [`wado_manifest`] resolves the dependency graph and performs no I/O; this is
//! the layer that reads the disk for it. Shared by `wado-lsp` and `wado-cli`.

pub mod dependency;
pub mod workspace;

pub use dependency::{DependencyEntry, RegistryComponentNeed, cache_root, package_lib_entry};
pub use workspace::{
    MANIFEST_FILENAME, WALK_MATCH_OPTIONS, absolutize, governing_workspace, nearest_manifest_dir,
    resolve_member_manifest,
};
