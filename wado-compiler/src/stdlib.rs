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
//!
//! // Raw WASI interfaces
//! use {Stdout, Stdout::{write_via_stream}} from "wasi:cli";
//! use {Preopens, Descriptor} from "wasi:filesystem";
//! ```

pub const CORE_PRELUDE: &str = include_str!("../lib/core/prelude.wado");
pub const CORE_CLI: &str = include_str!("../lib/core/cli.wado");
pub const CORE_INTERNAL: &str = include_str!("../lib/core/internal.wado");
pub const CORE_PRELUDE_STRING: &str = include_str!("../lib/core/prelude/string.wado");
pub const CORE_BUILTIN: &str = include_str!("../lib/core/builtin.wado");
pub const CORE_PRELUDE_TRAITS: &str = include_str!("../lib/core/prelude/traits.wado");
pub const CORE_PRELUDE_INT128: &str = include_str!("../lib/core/prelude/int128.wado");
pub const CORE_PRELUDE_TYPES: &str = include_str!("../lib/core/prelude/types.wado");
pub const CORE_PRELUDE_PRIMITIVES: &str = include_str!("../lib/core/prelude/primitives.wado");
pub const CORE_PRELUDE_FORMAT: &str = include_str!("../lib/core/prelude/format.wado");
pub const CORE_PRELUDE_ARRAY: &str = include_str!("../lib/core/prelude/array.wado");
pub const CORE_PRELUDE_FPFMT: &str = include_str!("../lib/core/prelude/fpfmt.wado");
pub const CORE_COLLECTIONS: &str = include_str!("../lib/core/collections.wado");
pub const CORE_ZLIB: &str = include_str!("../lib/core/zlib.wado");
pub const CORE_BASE64: &str = include_str!("../lib/core/base64.wado");
pub const WASI_CLI: &str = include_str!("../lib/wasi/cli.wado");
pub const WASI_FILESYSTEM: &str = include_str!("../lib/wasi/filesystem.wado");
pub const WASI_CLOCKS: &str = include_str!("../lib/wasi/clocks.wado");
pub const WASI_RANDOM: &str = include_str!("../lib/wasi/random.wado");
pub const WASI_SOCKETS: &str = include_str!("../lib/wasi/sockets.wado");
pub const WASI_HTTP: &str = include_str!("../lib/wasi/http.wado");

/// Get embedded module source by import path.
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
        "core:prelude/traits.wado" => Some(CORE_PRELUDE_TRAITS),
        "core:prelude/int128.wado" => Some(CORE_PRELUDE_INT128),
        "core:prelude/types.wado" => Some(CORE_PRELUDE_TYPES),
        "core:prelude/primitives.wado" => Some(CORE_PRELUDE_PRIMITIVES),
        "core:prelude/string.wado" => Some(CORE_PRELUDE_STRING),
        "core:prelude/format.wado" => Some(CORE_PRELUDE_FORMAT),
        "core:prelude/array.wado" => Some(CORE_PRELUDE_ARRAY),
        "core:prelude/fpfmt.wado" => Some(CORE_PRELUDE_FPFMT),
        "core:collections" => Some(CORE_COLLECTIONS),
        "core:cli" => Some(CORE_CLI),
        "core:internal" => Some(CORE_INTERNAL),
        "core:builtin" => Some(CORE_BUILTIN),
        "core:zlib" => Some(CORE_ZLIB),
        "core:base64" => Some(CORE_BASE64),

        // WASI library
        "wasi:cli" => Some(WASI_CLI),
        "wasi:filesystem" => Some(WASI_FILESYSTEM),
        "wasi:clocks" => Some(WASI_CLOCKS),
        "wasi:random" => Some(WASI_RANDOM),
        "wasi:sockets" => Some(WASI_SOCKETS),
        "wasi:http" => Some(WASI_HTTP),

        _ => None,
    }
}

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
        assert!(source.unwrap().contains("pub use"));
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
    fn test_get_wasi_clocks() {
        let source = get_stdlib_module("wasi:clocks");
        assert!(source.is_some());
        assert!(source.unwrap().contains("MonotonicClock"));
    }

    #[test]
    fn test_get_wasi_random() {
        let source = get_stdlib_module("wasi:random");
        assert!(source.is_some());
        assert!(source.unwrap().contains("Random"));
    }

    #[test]
    fn test_get_wasi_sockets() {
        let source = get_stdlib_module("wasi:sockets");
        assert!(source.is_some());
        assert!(source.unwrap().contains("TcpSocket"));
    }

    #[test]
    fn test_unknown_module() {
        assert!(get_stdlib_module("core:unknown").is_none());
        assert!(get_stdlib_module("wasi:unknown").is_none());
    }

    #[test]
    fn test_get_prelude_primitives() {
        let source = get_stdlib_module("core:prelude/primitives.wado");
        assert!(source.is_some(), "primitives module should exist");
        assert!(
            source.unwrap().contains("impl i32"),
            "should contain impl i32"
        );
    }

    #[test]
    fn test_non_stdlib_module() {
        assert!(get_stdlib_module("myapp:utils").is_none());
        assert!(get_stdlib_module("https://example.com/lib.wado").is_none());
    }
}
