//! Embedded standard library sources
//!
//! The standard library is bundled into the compiler binary using `include_str!`.
//! This ensures the compiler is self-contained and doesn't need to locate
//! the library at runtime.
//!
//! ## Namespace Structure
//!
//! - `core:*` - High-level Wado standard library (e.g., `println()`, `read_file()`)
//! - `wasi:*` - Raw WASI packages (low-level interfaces like `Stdout`, `FileSystem`)
//!
//! ## Usage in Wado code
//!
//! ```wado
//! // High-level helpers
//! use {println, eprintln} from "core:cli";
//! use {read_file, exists} from "core:filesystem";
//!
//! // Raw WASI interfaces
//! use {Stdout, Stdout::{write_via_stream}} from "wasi:cli";
//! use {FileSystem, Preopens} from "wasi:filesystem";
//! ```

// ============================================================================
// Core Library (core:*)
// ============================================================================

/// Embedded source for core:prelude
pub const CORE_PRELUDE: &str = include_str!("../lib/core/prelude.wado");

/// Embedded source for core:cli
pub const CORE_CLI: &str = include_str!("../lib/core/cli.wado");

/// Embedded source for core:filesystem
pub const CORE_FILESYSTEM: &str = include_str!("../lib/core/filesystem.wado");

// ============================================================================
// WASI Library (wasi:*)
// ============================================================================

/// Embedded source for wasi:io
pub const WASI_IO: &str = include_str!("../lib/wasi/io.wado");

/// Embedded source for wasi:cli
pub const WASI_CLI: &str = include_str!("../lib/wasi/cli.wado");

/// Embedded source for wasi:filesystem
pub const WASI_FILESYSTEM: &str = include_str!("../lib/wasi/filesystem.wado");

// ============================================================================
// Module Resolution
// ============================================================================

/// Get embedded module source by import path.
///
/// Supports the new ESM-like import syntax:
/// - `"core:prelude"` -> core library prelude
/// - `"core:cli"` -> core library CLI helpers
/// - `"core:filesystem"` -> core library filesystem helpers
/// - `"wasi:io"` -> WASI I/O (poll, Pollable)
/// - `"wasi:cli"` -> WASI CLI interfaces
/// - `"wasi:filesystem"` -> WASI filesystem interfaces
///
/// # Arguments
/// * `import_path` - Import path string, e.g., `"core:cli"` or `"wasi:filesystem"`
///
/// # Returns
/// The source code of the module if found, or `None` if not a standard library module.
pub fn get_stdlib_module(import_path: &str) -> Option<&'static str> {
    match import_path {
        // Core library
        "core:prelude" => Some(CORE_PRELUDE),
        "core:cli" => Some(CORE_CLI),
        "core:filesystem" => Some(CORE_FILESYSTEM),

        // WASI library
        "wasi:io" => Some(WASI_IO),
        "wasi:cli" => Some(WASI_CLI),
        "wasi:filesystem" => Some(WASI_FILESYSTEM),

        _ => None,
    }
}

/// Check if an import path refers to a standard library module.
pub fn is_stdlib_module(import_path: &str) -> bool {
    get_stdlib_module(import_path).is_some()
}

/// Check if an import path starts with a known namespace.
pub fn is_stdlib_namespace(import_path: &str) -> bool {
    import_path.starts_with("core:") || import_path.starts_with("wasi:")
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_core_cli() {
        let source = get_stdlib_module("core:cli");
        assert!(source.is_some());
        assert!(source.unwrap().contains("println"));
    }

    #[test]
    fn test_get_core_prelude() {
        let source = get_stdlib_module("core:prelude");
        assert!(source.is_some());
        assert!(source.unwrap().contains("Stream"));
    }

    #[test]
    fn test_get_core_filesystem() {
        let source = get_stdlib_module("core:filesystem");
        assert!(source.is_some());
        assert!(source.unwrap().contains("read_file"));
    }

    #[test]
    fn test_get_wasi_io() {
        let source = get_stdlib_module("wasi:io");
        assert!(source.is_some());
        assert!(source.unwrap().contains("Pollable"));
    }

    #[test]
    fn test_get_wasi_cli() {
        let source = get_stdlib_module("wasi:cli");
        assert!(source.is_some());
        assert!(source.unwrap().contains("Stdout"));
    }

    #[test]
    fn test_get_wasi_filesystem() {
        let source = get_stdlib_module("wasi:filesystem");
        assert!(source.is_some());
        assert!(source.unwrap().contains("Descriptor"));
    }

    #[test]
    fn test_unknown_module() {
        assert!(get_stdlib_module("core:unknown").is_none());
        assert!(get_stdlib_module("wasi:unknown").is_none());
    }

    #[test]
    fn test_non_stdlib_module() {
        assert!(get_stdlib_module("myapp:utils").is_none());
        assert!(get_stdlib_module("https://example.com/lib.wado").is_none());
    }

    #[test]
    fn test_is_stdlib_module() {
        assert!(is_stdlib_module("core:cli"));
        assert!(is_stdlib_module("wasi:filesystem"));
        assert!(!is_stdlib_module("myapp:utils"));
    }

    #[test]
    fn test_is_stdlib_namespace() {
        assert!(is_stdlib_namespace("core:cli"));
        assert!(is_stdlib_namespace("core:unknown"));
        assert!(is_stdlib_namespace("wasi:cli"));
        assert!(is_stdlib_namespace("wasi:unknown"));
        assert!(!is_stdlib_namespace("myapp:utils"));
        assert!(!is_stdlib_namespace("https://example.com"));
    }
}
