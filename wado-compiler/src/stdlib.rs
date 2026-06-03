//! Embedded standard library sources
//!
//! The standard library is bundled into the compiler binary using `include_str!`.
//! This ensures the compiler is self-contained and doesn't need to locate
//! the library at runtime.
//!
//! ## Namespace Structure
//!
//! - `core:*` - High-level Wado standard library (e.g., `println()`, `read_file()`)
//! - `wasi:*` - Raw WASI packages, organized by interface (e.g., `wasi:cli/stdout`)
//!
//! ## Usage in Wado code
//!
//! ```wado
//! // High-level helpers
//! use {println, eprintln} from "core:cli";
//!
//! // Raw WASI interfaces (use the specific interface path)
//! use {Stdout} from "wasi:cli/stdout";
//! use {Descriptor, Filesize} from "wasi:filesystem/types";
//! use {Preopens} from "wasi:filesystem/preopens";
//! ```

pub const CORE_PRELUDE: &str = include_str!("../lib/core/prelude.wado");

// Wasm assets bundled with the compiler. Loaded as raw bytes (UTF-8 for
// `.wat`, binary for `.wasm`) and dispatched through `get_stdlib_wasm_asset`.
pub const CORE_LIBM_WAT: &[u8] = include_bytes!("../lib/core/libm.wat");
pub const CORE_CLI: &str = include_str!("../lib/core/cli.wado");
pub const CORE_INTERNAL: &str = include_str!("../lib/core/internal.wado");
pub const CORE_ALLOCATOR: &str = include_str!("../lib/core/allocator.wado");
pub const CORE_PRELUDE_STRING: &str = include_str!("../lib/core/prelude/string.wado");
pub const CORE_BUILTIN: &str = include_str!("../lib/core/builtin.wado");
pub const CORE_PRELUDE_TRAITS: &str = include_str!("../lib/core/prelude/traits.wado");
pub const CORE_PRELUDE_INT128: &str = include_str!("../lib/core/prelude/int128.wado");
pub const CORE_PRELUDE_TYPES: &str = include_str!("../lib/core/prelude/types.wado");
pub const CORE_PRELUDE_ARRAY: &str = include_str!("../lib/core/prelude/array.wado");
pub const CORE_PRELUDE_PRIMITIVE: &str = include_str!("../lib/core/prelude/primitive.wado");
pub const CORE_PRELUDE_FORMAT: &str = include_str!("../lib/core/prelude/format.wado");
pub const CORE_PRELUDE_LIST: &str = include_str!("../lib/core/prelude/list.wado");
pub const CORE_PRELUDE_FPFMT: &str = include_str!("../lib/core/prelude/fpfmt.wado");
pub const CORE_PRELUDE_INTPARSE: &str = include_str!("../lib/core/prelude/intparse.wado");
pub const CORE_PRELUDE_TUPLE: &str = include_str!("../lib/core/prelude/tuple.wado");
pub const CORE_PRELUDE_RANGE: &str = include_str!("../lib/core/prelude/range.wado");
pub const CORE_COLLECTIONS: &str = include_str!("../lib/core/collections.wado");
pub const CORE_ZLIB: &str = include_str!("../lib/core/zlib.wado");
pub const CORE_BASE64: &str = include_str!("../lib/core/base64.wado");
pub const CORE_BENCHMARK: &str = include_str!("../lib/core/benchmark.wado");
pub const CORE_SERDE: &str = include_str!("../lib/core/serde.wado");
pub const CORE_JSON: &str = include_str!("../lib/core/json.wado");
pub const CORE_JSON_NSD: &str = include_str!("../lib/core/json_nsd.wado");
pub const CORE_JSON_VALUE: &str = include_str!("../lib/core/json_value.wado");
pub const CORE_SIMD: &str = include_str!("../lib/core/simd.wado");
pub const CORE_KILN: &str = include_str!("../lib/core/kiln.wado");
pub const CORE_KILN_KILN_HOST: &str = include_str!("../lib/core/kiln/kiln_host.wado");
pub const CORE_KILN_TYPES: &str = include_str!("../lib/core/kiln/types.wado");
pub const CORE_KILN_WORLDS: &str = include_str!("../lib/core/kiln/worlds.wado");
pub const CORE_URL: &str = include_str!("../lib/core/url.wado");
pub const CORE_ROUTER: &str = include_str!("../lib/core/router.wado");
pub const CORE_DIGEST: &str = include_str!("../lib/core/digest.wado");
pub const CORE_UUID: &str = include_str!("../lib/core/uuid.wado");

// WASI flat package files — re-export from all sub-interfaces (backward compat)
pub const WASI_CLI: &str = include_str!("../lib/wasi/cli.wado");
pub const WASI_FILESYSTEM: &str = include_str!("../lib/wasi/filesystem.wado");
pub const WASI_CLOCKS: &str = include_str!("../lib/wasi/clocks.wado");
pub const WASI_RANDOM: &str = include_str!("../lib/wasi/random.wado");
pub const WASI_SOCKETS: &str = include_str!("../lib/wasi/sockets.wado");
pub const WASI_TLS: &str = include_str!("../lib/wasi/tls.wado");
pub const WASI_HTTP: &str = include_str!("../lib/wasi/http.wado");

// WASI interfaces — one constant per interface
pub const WASI_CLI_ENVIRONMENT: &str = include_str!("../lib/wasi/cli/environment.wado");
pub const WASI_CLI_EXIT: &str = include_str!("../lib/wasi/cli/exit.wado");
pub const WASI_CLI_RUN: &str = include_str!("../lib/wasi/cli/run.wado");
pub const WASI_CLI_TYPES: &str = include_str!("../lib/wasi/cli/types.wado");
pub const WASI_CLI_STDIN: &str = include_str!("../lib/wasi/cli/stdin.wado");
pub const WASI_CLI_STDOUT: &str = include_str!("../lib/wasi/cli/stdout.wado");
pub const WASI_CLI_STDERR: &str = include_str!("../lib/wasi/cli/stderr.wado");
pub const WASI_CLI_TERMINAL_INPUT: &str = include_str!("../lib/wasi/cli/terminal_input.wado");
pub const WASI_CLI_TERMINAL_OUTPUT: &str = include_str!("../lib/wasi/cli/terminal_output.wado");
pub const WASI_CLI_TERMINAL_STDIN: &str = include_str!("../lib/wasi/cli/terminal_stdin.wado");
pub const WASI_CLI_TERMINAL_STDOUT: &str = include_str!("../lib/wasi/cli/terminal_stdout.wado");
pub const WASI_CLI_TERMINAL_STDERR: &str = include_str!("../lib/wasi/cli/terminal_stderr.wado");
pub const WASI_CLI_WORLDS: &str = include_str!("../lib/wasi/cli/worlds.wado");

pub const WASI_CLOCKS_TYPES: &str = include_str!("../lib/wasi/clocks/types.wado");
pub const WASI_CLOCKS_MONOTONIC_CLOCK: &str =
    include_str!("../lib/wasi/clocks/monotonic_clock.wado");
pub const WASI_CLOCKS_SYSTEM_CLOCK: &str = include_str!("../lib/wasi/clocks/system_clock.wado");
pub const WASI_CLOCKS_TIMEZONE: &str = include_str!("../lib/wasi/clocks/timezone.wado");
pub const WASI_CLOCKS_WORLDS: &str = include_str!("../lib/wasi/clocks/worlds.wado");

pub const WASI_FILESYSTEM_TYPES: &str = include_str!("../lib/wasi/filesystem/types.wado");
pub const WASI_FILESYSTEM_PREOPENS: &str = include_str!("../lib/wasi/filesystem/preopens.wado");
pub const WASI_FILESYSTEM_WORLDS: &str = include_str!("../lib/wasi/filesystem/worlds.wado");

pub const WASI_HTTP_TYPES: &str = include_str!("../lib/wasi/http/types.wado");
pub const WASI_HTTP_HANDLER: &str = include_str!("../lib/wasi/http/handler.wado");
pub const WASI_HTTP_CLIENT: &str = include_str!("../lib/wasi/http/client.wado");
pub const WASI_HTTP_WORLDS: &str = include_str!("../lib/wasi/http/worlds.wado");

pub const WASI_RANDOM_INSECURE_SEED: &str = include_str!("../lib/wasi/random/insecure_seed.wado");
pub const WASI_RANDOM_INSECURE: &str = include_str!("../lib/wasi/random/insecure.wado");
pub const WASI_RANDOM_RANDOM: &str = include_str!("../lib/wasi/random/random.wado");
pub const WASI_RANDOM_WORLDS: &str = include_str!("../lib/wasi/random/worlds.wado");

pub const WASI_SOCKETS_TYPES: &str = include_str!("../lib/wasi/sockets/types.wado");
pub const WASI_SOCKETS_IP_NAME_LOOKUP: &str =
    include_str!("../lib/wasi/sockets/ip_name_lookup.wado");
pub const WASI_SOCKETS_WORLDS: &str = include_str!("../lib/wasi/sockets/worlds.wado");

pub const WASI_TLS_TYPES: &str = include_str!("../lib/wasi/tls/types.wado");
pub const WASI_TLS_CLIENT: &str = include_str!("../lib/wasi/tls/client.wado");
pub const WASI_TLS_WORLDS: &str = include_str!("../lib/wasi/tls/worlds.wado");

/// All core stdlib statics.
///
/// Each entry is `(import_path, source)` where `import_path` matches
/// what users write in `from "core:..."` expressions. This is the
/// single source of truth for the set of core stdlib modules: the
/// loader's `cached_stdlib_module`, `get_stdlib_module`, and the
/// `ModuleSourceInterner` well-known arc set all derive from it.
pub const ALL_CORE_MODULES: &[(&str, &str)] = &[
    ("core:allocator", CORE_ALLOCATOR),
    ("core:builtin", CORE_BUILTIN),
    ("core:cli", CORE_CLI),
    ("core:collections", CORE_COLLECTIONS),
    ("core:internal", CORE_INTERNAL),
    ("core:prelude", CORE_PRELUDE),
    ("core:prelude/array.wado", CORE_PRELUDE_ARRAY),
    ("core:prelude/list.wado", CORE_PRELUDE_LIST),
    ("core:prelude/format.wado", CORE_PRELUDE_FORMAT),
    ("core:prelude/fpfmt.wado", CORE_PRELUDE_FPFMT),
    ("core:prelude/int128.wado", CORE_PRELUDE_INT128),
    ("core:prelude/intparse.wado", CORE_PRELUDE_INTPARSE),
    ("core:prelude/primitive.wado", CORE_PRELUDE_PRIMITIVE),
    ("core:prelude/range.wado", CORE_PRELUDE_RANGE),
    ("core:prelude/string.wado", CORE_PRELUDE_STRING),
    ("core:prelude/traits.wado", CORE_PRELUDE_TRAITS),
    ("core:prelude/tuple.wado", CORE_PRELUDE_TUPLE),
    ("core:prelude/types.wado", CORE_PRELUDE_TYPES),
    ("core:zlib", CORE_ZLIB),
    ("core:base64", CORE_BASE64),
    ("core:benchmark", CORE_BENCHMARK),
    ("core:serde", CORE_SERDE),
    ("core:json", CORE_JSON),
    ("core:json_nsd", CORE_JSON_NSD),
    ("core:json_value", CORE_JSON_VALUE),
    ("core:simd", CORE_SIMD),
    ("core:url", CORE_URL),
    ("core:router", CORE_ROUTER),
    ("core:digest", CORE_DIGEST),
    ("core:uuid", CORE_UUID),
    ("core:kiln", CORE_KILN),
    ("core:kiln/kiln_host.wado", CORE_KILN_KILN_HOST),
    ("core:kiln/types.wado", CORE_KILN_TYPES),
    ("core:kiln/worlds.wado", CORE_KILN_WORLDS),
];

/// All bundled wasm assets, used for registry building.
///
/// Each entry is `(canonical_path, bytes)` matching the canonical
/// `wasm:`-style path that the loader assigns to
/// [`ModuleSource::Wasm`].
pub const ALL_CORE_WASM_ASSETS: &[(&str, &[u8])] = &[("core:libm.wat", CORE_LIBM_WAT)];

/// All WASI interface statics, used for registry building.
///
/// Each entry is `(import_path, source)` where `import_path` matches
/// what users write in `from "wasi:..."` expressions.
pub const ALL_WASI_MODULES: &[(&str, &str)] = &[
    // Flat package paths (user-facing, backward compatible)
    ("wasi:cli", WASI_CLI),
    ("wasi:filesystem", WASI_FILESYSTEM),
    ("wasi:clocks", WASI_CLOCKS),
    ("wasi:random", WASI_RANDOM),
    ("wasi:sockets", WASI_SOCKETS),
    ("wasi:tls", WASI_TLS),
    ("wasi:http", WASI_HTTP),
    // Per-interface paths (for specific imports and cross-package references)
    ("wasi:cli/environment.wado", WASI_CLI_ENVIRONMENT),
    ("wasi:cli/exit.wado", WASI_CLI_EXIT),
    ("wasi:cli/run.wado", WASI_CLI_RUN),
    ("wasi:cli/types.wado", WASI_CLI_TYPES),
    ("wasi:cli/stdin.wado", WASI_CLI_STDIN),
    ("wasi:cli/stdout.wado", WASI_CLI_STDOUT),
    ("wasi:cli/stderr.wado", WASI_CLI_STDERR),
    ("wasi:cli/terminal_input.wado", WASI_CLI_TERMINAL_INPUT),
    ("wasi:cli/terminal_output.wado", WASI_CLI_TERMINAL_OUTPUT),
    ("wasi:cli/terminal_stdin.wado", WASI_CLI_TERMINAL_STDIN),
    ("wasi:cli/terminal_stdout.wado", WASI_CLI_TERMINAL_STDOUT),
    ("wasi:cli/terminal_stderr.wado", WASI_CLI_TERMINAL_STDERR),
    ("wasi:cli/worlds.wado", WASI_CLI_WORLDS),
    ("wasi:clocks/types.wado", WASI_CLOCKS_TYPES),
    (
        "wasi:clocks/monotonic_clock.wado",
        WASI_CLOCKS_MONOTONIC_CLOCK,
    ),
    ("wasi:clocks/system_clock.wado", WASI_CLOCKS_SYSTEM_CLOCK),
    ("wasi:clocks/timezone.wado", WASI_CLOCKS_TIMEZONE),
    ("wasi:clocks/worlds.wado", WASI_CLOCKS_WORLDS),
    ("wasi:filesystem/types.wado", WASI_FILESYSTEM_TYPES),
    ("wasi:filesystem/preopens.wado", WASI_FILESYSTEM_PREOPENS),
    ("wasi:filesystem/worlds.wado", WASI_FILESYSTEM_WORLDS),
    ("wasi:http/types.wado", WASI_HTTP_TYPES),
    ("wasi:http/handler.wado", WASI_HTTP_HANDLER),
    ("wasi:http/client.wado", WASI_HTTP_CLIENT),
    ("wasi:http/worlds.wado", WASI_HTTP_WORLDS),
    ("wasi:random/insecure_seed.wado", WASI_RANDOM_INSECURE_SEED),
    ("wasi:random/insecure.wado", WASI_RANDOM_INSECURE),
    ("wasi:random/random.wado", WASI_RANDOM_RANDOM),
    ("wasi:random/worlds.wado", WASI_RANDOM_WORLDS),
    ("wasi:sockets/types.wado", WASI_SOCKETS_TYPES),
    (
        "wasi:sockets/ip_name_lookup.wado",
        WASI_SOCKETS_IP_NAME_LOOKUP,
    ),
    ("wasi:sockets/worlds.wado", WASI_SOCKETS_WORLDS),
    ("wasi:tls/types.wado", WASI_TLS_TYPES),
    ("wasi:tls/client.wado", WASI_TLS_CLIENT),
    ("wasi:tls/worlds.wado", WASI_TLS_WORLDS),
];

/// Get embedded wasm asset bytes by canonical path.
///
/// Used by the wasm-import loader path to resolve stdlib-bundled
/// `.wat`/`.wasm` files (e.g., `core:libm.wat`). User-supplied paths
/// (`./foo.wat`) are loaded through `CompilerHost::load_source` instead.
#[must_use]
pub fn get_stdlib_wasm_asset(import_path: &str) -> Option<&'static [u8]> {
    ALL_CORE_WASM_ASSETS
        .iter()
        .find(|(path, _)| *path == import_path)
        .map(|(_, bytes)| *bytes)
}

/// Get embedded module source by import path.
///
/// # Arguments
/// * `import_path` - Import path string, e.g., `"core:cli"` or `"wasi:filesystem/types.wado"`
///
/// # Returns
/// The source code of the module if found, or `None` if not a standard library module.
pub fn get_stdlib_module(import_path: &str) -> Option<&'static str> {
    ALL_CORE_MODULES
        .iter()
        .chain(ALL_WASI_MODULES.iter())
        .find(|(path, _)| *path == import_path)
        .map(|(_, src)| *src)
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
    fn test_get_wasi_cli_stdout() {
        let source = get_stdlib_module("wasi:cli/stdout.wado");
        assert!(source.is_some());
        assert!(source.unwrap().contains("Stdout"));
    }

    #[test]
    fn test_get_wasi_filesystem_types() {
        let source = get_stdlib_module("wasi:filesystem/types.wado");
        assert!(source.is_some());
        assert!(source.unwrap().contains("Descriptor"));
    }

    #[test]
    fn test_get_wasi_filesystem_preopens() {
        let source = get_stdlib_module("wasi:filesystem/preopens.wado");
        assert!(source.is_some());
        assert!(source.unwrap().contains("Preopens"));
    }

    #[test]
    fn test_get_wasi_clocks_monotonic_clock() {
        let source = get_stdlib_module("wasi:clocks/monotonic_clock.wado");
        assert!(source.is_some());
        assert!(source.unwrap().contains("MonotonicClock"));
    }

    #[test]
    fn test_get_wasi_random_random() {
        let source = get_stdlib_module("wasi:random/random.wado");
        assert!(source.is_some());
        assert!(source.unwrap().contains("Random"));
    }

    #[test]
    fn test_get_wasi_sockets_types() {
        let source = get_stdlib_module("wasi:sockets/types.wado");
        assert!(source.is_some());
        assert!(source.unwrap().contains("TcpSocket"));
    }

    #[test]
    fn test_get_wasi_tls_client() {
        let source = get_stdlib_module("wasi:tls/client.wado");
        assert!(source.is_some());
        assert!(source.unwrap().contains("Connector"));
    }

    #[test]
    fn test_get_wasi_tls_flat() {
        let source = get_stdlib_module("wasi:tls");
        assert!(source.is_some());
        assert!(source.unwrap().contains("Connector"));
    }

    #[test]
    fn test_get_wasi_cli_flat() {
        let source = get_stdlib_module("wasi:cli");
        assert!(source.is_some());
        assert!(source.unwrap().contains("Stdout"));
    }

    #[test]
    fn test_get_wasi_filesystem_flat() {
        let source = get_stdlib_module("wasi:filesystem");
        assert!(source.is_some());
        assert!(source.unwrap().contains("Descriptor"));
    }

    #[test]
    fn test_get_core_kiln_facade() {
        let source = get_stdlib_module("core:kiln");
        assert!(source.is_some(), "core:kiln facade should be registered");
        let s = source.unwrap();
        assert!(s.contains("pub struct Request"));
        assert!(s.contains("KilnHost"));
    }

    #[test]
    fn test_get_core_kiln_submodules() {
        assert!(
            get_stdlib_module("core:kiln/kiln_host.wado")
                .unwrap()
                .contains("pub interface KilnHost")
        );
        assert!(
            get_stdlib_module("core:kiln/types.wado")
                .unwrap()
                .contains("pub struct RawRequest")
        );
        assert!(
            get_stdlib_module("core:kiln/worlds.wado")
                .unwrap()
                .contains("pub world Generator")
        );
    }

    #[test]
    fn test_unknown_module() {
        assert!(get_stdlib_module("core:unknown").is_none());
        assert!(get_stdlib_module("wasi:unknown").is_none());
        // Without .wado extension for sub-interface paths
        assert!(get_stdlib_module("wasi:cli/stdout").is_none());
        assert!(get_stdlib_module("wasi:filesystem/types").is_none());
    }

    #[test]
    fn test_get_prelude_primitives() {
        let source = get_stdlib_module("core:prelude/primitive.wado");
        assert!(source.is_some(), "primitive module should exist");
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
