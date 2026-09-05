//! Standard library sources: `core:*` is the Wado library (`core:cli`'s
//! `println`, …), `wasi:*` the raw WASI packages keyed by interface.

/// The `lib/` directory this crate was compiled from.
#[cfg(all(debug_assertions, not(target_arch = "wasm32")))]
const STDLIB_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/lib");

#[cfg(all(debug_assertions, not(target_arch = "wasm32")))]
fn read_stdlib_module(file: &str) -> &'static str {
    let path = std::path::Path::new(STDLIB_ROOT).join(file);
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("dev build reads the stdlib from {}: {e}", path.display()));
    String::leak(source)
}

/// Declares a stdlib table, and the paths alone, from one list of
/// `(import path, file)` pairs. A dev build reads the files, so editing a
/// module needs no rebuild. A release build embeds them, as does `wasm32`,
/// which has no filesystem.
macro_rules! stdlib_table {
    (
        $(#[$meta:meta])* $vis:vis fn $name:ident;
        $(#[$pmeta:meta])* const $paths:ident;
        $($import:literal => $file:literal,)*
    ) => {
        $(#[$pmeta])*
        $vis const $paths: &[&str] = &[$($import,)*];

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
    /// Every core stdlib module — the one set the loader and
    /// [`get_stdlib_module`] both derive from.
    pub fn all_core_modules;
    /// The same set, named without its sources.
    const CORE_MODULE_PATHS;
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
    /// Every bundled CM binding module; a flat package path re-exports its
    /// sub-interfaces. Appearing here reserves the namespace — see
    /// `docs/wep-2026-06-17-package-module-syntax.md`.
    pub fn all_binding_modules;
    /// The same set, named without its sources.
    const BINDING_MODULE_PATHS;
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
    // Web platform bindings, generated from the WebIDL snapshot beside them.
    "web:dom" => "web/dom.wado",
}

/// Always embedded: generated bytes, not source anyone edits.
pub const CORE_LIBM_WAT: &[u8] = include_bytes!("../lib/core/libm.wat");

/// Every bundled wasm asset, keyed by the path the loader assigns to
/// [`ModuleSource::Wasm`](crate::module_source::ModuleSource::Wasm).
pub const ALL_CORE_WASM_ASSETS: &[(&str, &[u8])] = &[("core:libm.wat", CORE_LIBM_WAT)];

/// A bundled `.wat`/`.wasm` asset by canonical path. A user-supplied path
/// (`./foo.wat`) goes through `CompilerHost::load_source` instead.
#[must_use]
pub fn get_stdlib_wasm_asset(import_path: &str) -> Option<&'static [u8]> {
    ALL_CORE_WASM_ASSETS
        .iter()
        .find(|(path, _)| *path == import_path)
        .map(|(_, bytes)| *bytes)
}

/// The source of a stdlib module by import path — `"core:cli"`,
/// `"wasi:filesystem/types.wado"`, `"web:dom"` — or None for anything else.
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

    /// A flat package path and a sub-interface path both resolve, and each
    /// lands on the module that declares the named item.
    #[test]
    fn an_import_path_resolves_to_the_module_that_declares_its_items() {
        for (import, declaration) in [
            ("core:cli", "println"),
            ("core:prelude", "pub use"),
            ("core:prelude/primitive.wado", "impl i32"),
            ("core:kiln", "pub struct Request"),
            ("core:kiln", "KilnHost"),
            ("core:kiln/kiln_host.wado", "pub interface KilnHost"),
            ("core:kiln/types.wado", "pub struct InputFile"),
            ("core:kiln/worlds.wado", "pub world Generator"),
            ("wasi:cli", "Stdout"),
            ("wasi:cli/stdout.wado", "Stdout"),
            ("wasi:filesystem", "Descriptor"),
            ("wasi:filesystem/types.wado", "Descriptor"),
            ("wasi:filesystem/preopens.wado", "Preopens"),
            ("wasi:clocks/monotonic_clock.wado", "MonotonicClock"),
            ("wasi:random/random.wado", "Random"),
            ("wasi:sockets/types.wado", "TcpSocket"),
            ("wasi:tls", "Connector"),
            ("wasi:tls/client.wado", "Connector"),
        ] {
            let source = get_stdlib_module(import).unwrap_or_else(|| panic!("{import}"));
            assert!(source.contains(declaration), "{import} lacks {declaration}");
        }
    }

    #[test]
    fn a_path_outside_the_tables_is_not_a_stdlib_module() {
        for import in [
            "core:unknown",
            "wasi:unknown",
            // A sub-interface path without its `.wado` extension.
            "wasi:cli/stdout",
            "wasi:filesystem/types",
            "myapp:utils",
            "https://example.com/lib.wado",
        ] {
            assert!(get_stdlib_module(import).is_none(), "{import}");
        }
    }

    #[test]
    fn import_paths_are_unique() {
        let mut seen = crate::hashmap::IndexSet::default();
        for import in CORE_MODULE_PATHS.iter().chain(BINDING_MODULE_PATHS) {
            assert!(seen.insert(*import), "duplicate import path {import}");
        }
    }

    /// A dev build serves what is on disk now, and no table entry points at a
    /// file other than the one its import path names.
    #[cfg(all(debug_assertions, not(target_arch = "wasm32")))]
    #[test]
    fn a_dev_build_serves_the_file_its_import_path_names() {
        fn file_of(import: &str) -> String {
            let (package, module) = import.split_once(':').expect(import);
            let names_a_file = module.contains('/');
            if names_a_file {
                format!("{package}/{module}")
            } else {
                format!("{package}/{module}.wado")
            }
        }

        for (import, _) in all_core_modules().iter().chain(all_binding_modules()) {
            let path = std::path::Path::new(STDLIB_ROOT).join(file_of(import));
            let on_disk = std::fs::read_to_string(&path).expect(import);
            assert_eq!(
                get_stdlib_module(import).expect(import),
                on_disk,
                "{import}"
            );
        }
    }
}
