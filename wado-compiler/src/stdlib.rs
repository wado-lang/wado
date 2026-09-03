//! Standard library sources. `core:*` is the high-level Wado library
//! (`core:cli`'s `println`, …) and `wasi:*` the raw WASI packages, keyed by
//! interface (`wasi:cli/stdout`, `wasi:filesystem/types`).
//!
//! A release build — and any build for `wasm32`, which has no filesystem —
//! embeds every module with `include_str!`, so the compiler locates nothing at
//! runtime. A dev build reads them from `lib/` instead: `include_str!` makes
//! each module a build dependency, so editing one would rebuild the compiler
//! before it could be tried.

/// The `lib/` directory this crate was compiled from. A dev build reads its
/// stdlib from there.
#[cfg(all(debug_assertions, not(target_arch = "wasm32")))]
const STDLIB_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/lib");

#[cfg(all(debug_assertions, not(target_arch = "wasm32")))]
fn read_stdlib_module(file: &str) -> &'static str {
    let path = std::path::Path::new(STDLIB_ROOT).join(file);
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("dev build reads the stdlib from {}: {e}", path.display()));
    String::leak(source)
}

/// Declares a stdlib table as `(import_path, file)` pairs, `file` relative to
/// `lib/`. Both build modes are generated from the one list.
macro_rules! stdlib_table {
    ($(#[$meta:meta])* $vis:vis fn $name:ident; $($import:literal => $file:literal,)*) => {
        $(#[$meta])*
        #[cfg(any(not(debug_assertions), target_arch = "wasm32"))]
        $vis fn $name() -> &'static [(&'static str, &'static str)] {
            const TABLE: &[(&str, &str)] =
                &[$(($import, include_str!(concat!("../lib/", $file))),)*];
            TABLE
        }

        $(#[$meta])*
        #[cfg(all(debug_assertions, not(target_arch = "wasm32")))]
        $vis fn $name() -> &'static [(&'static str, &'static str)] {
            static TABLE: std::sync::OnceLock<Vec<(&'static str, &'static str)>> =
                std::sync::OnceLock::new();
            TABLE.get_or_init(|| vec![$(($import, read_stdlib_module($file)),)*])
        }
    };
}

stdlib_table! {
    /// Every core stdlib module.
    ///
    /// This is the single source of truth for the set: the loader's
    /// `cached_stdlib_module`, [`get_stdlib_module`], and the
    /// `ModuleSourceInterner` well-known arc set all derive from it.
    pub fn all_core_modules;
    "core:allocator" => "core/allocator.wado",
    "core:builtin" => "core/builtin.wado",
    "core:cli" => "core/cli.wado",
    "core:collections" => "core/collections.wado",
    "core:rt" => "core/rt.wado",
    "core:prelude" => "core/prelude.wado",
    "core:prelude/array.wado" => "core/prelude/array.wado",
    "core:prelude/slice.wado" => "core/prelude/slice.wado",
    "core:prelude/list.wado" => "core/prelude/list.wado",
    "core:prelude/format.wado" => "core/prelude/format.wado",
    "core:prelude/fpfmt.wado" => "core/prelude/fpfmt.wado",
    "core:prelude/int128.wado" => "core/prelude/int128.wado",
    "core:prelude/intparse.wado" => "core/prelude/intparse.wado",
    "core:prelude/primitive.wado" => "core/prelude/primitive.wado",
    "core:prelude/range.wado" => "core/prelude/range.wado",
    "core:prelude/bytes.wado" => "core/prelude/bytes.wado",
    "core:prelude/string.wado" => "core/prelude/string.wado",
    "core:prelude/traits.wado" => "core/prelude/traits.wado",
    "core:prelude/tuple.wado" => "core/prelude/tuple.wado",
    "core:prelude/types.wado" => "core/prelude/types.wado",
    "core:zlib" => "core/zlib.wado",
    "core:base64" => "core/base64.wado",
    "core:benchmark" => "core/benchmark.wado",
    "core:serde" => "core/serde.wado",
    "core:json" => "core/json.wado",
    "core:json_nsd" => "core/json_nsd.wado",
    "core:args" => "core/args.wado",
    "core:value" => "core/value.wado",
    "core:cbor" => "core/cbor.wado",
    "core:simd" => "core/simd.wado",
    "core:url" => "core/url.wado",
    "core:router" => "core/router.wado",
    "core:digest" => "core/digest.wado",
    "core:random" => "core/random.wado",
    "core:uuid" => "core/uuid.wado",
    "core:temporal" => "core/temporal.wado",
    "core:log" => "core/log.wado",
    "core:jwt" => "core/jwt.wado",
    "core:kiln" => "core/kiln.wado",
    "core:kiln/kiln_host.wado" => "core/kiln/kiln_host.wado",
    "core:kiln/types.wado" => "core/kiln/types.wado",
    "core:kiln/worlds.wado" => "core/kiln/worlds.wado",
}

stdlib_table! {
    /// Every bundled CM binding module.
    ///
    /// The import path is what users write in a `from "wasi:..."` / `from
    /// "web:..."` expression. The namespace of each is reserved because it
    /// appears here — see `docs/wep-2026-06-17-package-module-syntax.md`.
    /// A flat package path re-exports all of its sub-interfaces.
    pub fn all_binding_modules;
    "wasi:cli" => "wasi/cli.wado",
    "wasi:filesystem" => "wasi/filesystem.wado",
    "wasi:clocks" => "wasi/clocks.wado",
    "wasi:random" => "wasi/random.wado",
    "wasi:sockets" => "wasi/sockets.wado",
    "wasi:tls" => "wasi/tls.wado",
    "wasi:http" => "wasi/http.wado",
    "wasi:cli/environment.wado" => "wasi/cli/environment.wado",
    "wasi:cli/exit.wado" => "wasi/cli/exit.wado",
    "wasi:cli/run.wado" => "wasi/cli/run.wado",
    "wasi:cli/types.wado" => "wasi/cli/types.wado",
    "wasi:cli/stdin.wado" => "wasi/cli/stdin.wado",
    "wasi:cli/stdout.wado" => "wasi/cli/stdout.wado",
    "wasi:cli/stderr.wado" => "wasi/cli/stderr.wado",
    "wasi:cli/terminal_input.wado" => "wasi/cli/terminal_input.wado",
    "wasi:cli/terminal_output.wado" => "wasi/cli/terminal_output.wado",
    "wasi:cli/terminal_stdin.wado" => "wasi/cli/terminal_stdin.wado",
    "wasi:cli/terminal_stdout.wado" => "wasi/cli/terminal_stdout.wado",
    "wasi:cli/terminal_stderr.wado" => "wasi/cli/terminal_stderr.wado",
    "wasi:cli/worlds.wado" => "wasi/cli/worlds.wado",
    "wasi:clocks/types.wado" => "wasi/clocks/types.wado",
    "wasi:clocks/monotonic_clock.wado" => "wasi/clocks/monotonic_clock.wado",
    "wasi:clocks/system_clock.wado" => "wasi/clocks/system_clock.wado",
    "wasi:clocks/timezone.wado" => "wasi/clocks/timezone.wado",
    "wasi:clocks/worlds.wado" => "wasi/clocks/worlds.wado",
    "wasi:filesystem/types.wado" => "wasi/filesystem/types.wado",
    "wasi:filesystem/preopens.wado" => "wasi/filesystem/preopens.wado",
    "wasi:filesystem/worlds.wado" => "wasi/filesystem/worlds.wado",
    "wasi:http/types.wado" => "wasi/http/types.wado",
    "wasi:http/handler.wado" => "wasi/http/handler.wado",
    "wasi:http/client.wado" => "wasi/http/client.wado",
    "wasi:http/worlds.wado" => "wasi/http/worlds.wado",
    "wasi:random/insecure_seed.wado" => "wasi/random/insecure_seed.wado",
    "wasi:random/insecure.wado" => "wasi/random/insecure.wado",
    "wasi:random/random.wado" => "wasi/random/random.wado",
    "wasi:random/worlds.wado" => "wasi/random/worlds.wado",
    "wasi:sockets/types.wado" => "wasi/sockets/types.wado",
    "wasi:sockets/ip_name_lookup.wado" => "wasi/sockets/ip_name_lookup.wado",
    "wasi:sockets/worlds.wado" => "wasi/sockets/worlds.wado",
    "wasi:tls/types.wado" => "wasi/tls/types.wado",
    "wasi:tls/client.wado" => "wasi/tls/client.wado",
    "wasi:tls/worlds.wado" => "wasi/tls/worlds.wado",
    // Web platform bindings — the extern-handle slice Tide's WebIDL frontend replaces.
    "web:dom" => "web/dom.wado",
}

/// A wasm asset bundled with the compiler, always embedded: it is generated
/// rather than edited, and is bytes rather than source.
pub const CORE_LIBM_WAT: &[u8] = include_bytes!("../lib/core/libm.wat");

/// All bundled wasm assets, used for registry building.
///
/// Each entry is `(canonical_path, bytes)` matching the canonical
/// `wasm:`-style path that the loader assigns to
/// [`ModuleSource::Wasm`](crate::module_source::ModuleSource::Wasm).
pub const ALL_CORE_WASM_ASSETS: &[(&str, &[u8])] = &[("core:libm.wat", CORE_LIBM_WAT)];

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

/// Get module source by import path.
///
/// # Arguments
/// * `import_path` - Import path string, e.g., `"core:cli"`, `"wasi:filesystem/types.wado"` or `"web:dom"`
///
/// # Returns
/// The source code of the module if found, or `None` if not a standard library module.
#[must_use]
pub fn get_stdlib_module(import_path: &str) -> Option<&'static str> {
    all_core_modules()
        .iter()
        .chain(all_binding_modules())
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
                .contains("pub struct InputFile")
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

    #[test]
    fn import_paths_are_unique() {
        let mut seen = crate::hashmap::IndexSet::default();
        for (import, _) in all_core_modules().iter().chain(all_binding_modules()) {
            assert!(seen.insert(*import), "duplicate import path {import}");
        }
    }

    /// Each module is served from the file its import path names — so a dev
    /// build serves what is on disk now, rather than a copy the last `cargo
    /// build` froze into the binary, and a table entry pointing at the wrong
    /// file is a mismatch rather than a silent swap.
    #[cfg(all(debug_assertions, not(target_arch = "wasm32")))]
    #[test]
    fn a_dev_build_serves_the_file_its_import_path_names() {
        for (import, _) in all_core_modules().iter().chain(all_binding_modules()) {
            // `core:cli` names a package module, `core:kiln/types.wado` a file
            // within one — the sub-path is the form that already carries its
            // own file name.
            let (package, module) = import.split_once(':').expect(import);
            let file = if module.contains('/') {
                format!("{package}/{module}")
            } else {
                format!("{package}/{module}.wado")
            };
            let path = std::path::Path::new(STDLIB_ROOT).join(file);
            let on_disk = std::fs::read_to_string(&path).expect(import);
            assert_eq!(
                get_stdlib_module(import).expect(import),
                on_disk,
                "{import}"
            );
        }
    }
}
