//! Dead-code elimination of imported wasm assets.
//!
//! When a Wado program name-imports a wat / wasm asset, the loader
//! synthesises declarations for **every** export (so `pub use` and
//! turbofish lookups don't depend on which exports the user happens to
//! mention). The codegen then prunes the embedded core module down to
//! the union of exports that actually survive DCE — see
//! `embed_imported_wasm_modules` in `wado-compiler/src/codegen/` and the
//! `wado-wasm-embed` crate it calls.
//!
//! This test holds that contract honest by inspecting the compiled
//! component bytes: it walks the embedded core wasm module's export
//! section and asserts that exactly the imports that the entry program
//! actually called survive — `add_one` only — even though the asset's
//! wat declares four exports (`add_one`, `twice`, `unused_no_args`,
//! `unused_squared`).

use std::path::Path;
use wasmparser::{Parser, Payload};

/// Locate the embedded core wasm module that holds the wat asset's
/// exports.
///
/// The component bytes contain several core modules (the user's "main"
/// module, the bundled "mem" module, and one per `wasm-mod-…`). We
/// pick the first core module that exports `add_one` (a function name
/// that only the wat asset emits) and return its raw bytes.
fn find_wat_asset_module(component_bytes: &[u8]) -> Vec<u8> {
    // Wasm Component Model wraps each core module with a binary header
    // identical to a standalone `.wasm` file (`\0asm` + version), so
    // `Parser::parse_all` over the component yields each core module
    // body via `Payload::ModuleSection { range, .. }` (or
    // `CoreModule(range)` on older wasmparser builds). We collect every
    // core module's bytes and then recognise the asset by its exports.
    let mut core_modules: Vec<Vec<u8>> = Vec::new();
    let parser = Parser::new(0);
    for payload in parser.parse_all(component_bytes) {
        let payload = payload.expect("component should parse");
        if let Payload::ModuleSection {
            unchecked_range, ..
        } = payload
        {
            let range = unchecked_range;
            core_modules.push(component_bytes[range.start..range.end].to_vec());
        }
    }
    assert!(
        !core_modules.is_empty(),
        "component should contain at least one core module"
    );

    for module in &core_modules {
        if module_has_export(module, "add_one") {
            return module.clone();
        }
    }
    panic!("no core module exports `add_one`; wat asset wasn't embedded");
}

/// `true` if the core wasm module exports a function under `name`.
fn module_has_export(module_bytes: &[u8], name: &str) -> bool {
    for payload in Parser::new(0).parse_all(module_bytes) {
        if let Ok(Payload::ExportSection(reader)) = payload {
            for export in reader {
                let export = export.expect("export entry should parse");
                if export.name == name {
                    return true;
                }
            }
        }
    }
    false
}

/// Collect every function-export name from a core wasm module.
fn function_exports(module_bytes: &[u8]) -> Vec<String> {
    let mut exports = Vec::new();
    for payload in Parser::new(0).parse_all(module_bytes) {
        if let Ok(Payload::ExportSection(reader)) = payload {
            for export in reader {
                let export = export.expect("export entry should parse");
                if matches!(export.kind, wasmparser::ExternalKind::Func) {
                    exports.push(export.name.to_string());
                }
            }
        }
    }
    exports
}

#[test]
fn unused_wat_exports_are_pruned() {
    // A Wado program that imports four exports from `sub/wasm_import_user.wat`
    // but only ever calls `add_one`. The synthesised module declares all
    // four; DCE should drop everything except `add_one` from the
    // embedded core module's export section.
    let source = r#"
use { println, Stdout } from "core:cli";
use { add_one, twice, unused_no_args, unused_squared } from "./sub/wasm_import_user.wat" with { type: "wat" };

export fn run() with Stdout {
    println(`${add_one(41)}`);
}
"#;
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let result = crate::common::compile_source_with_opts(
        &fixture_dir.join("__wasm_import_dce_entry__.wado"),
        source,
        wado_compiler::OptLevel::default(),
    )
    .expect("compile should succeed");

    let asset_module = find_wat_asset_module(&result.wasm);
    let exports = function_exports(&asset_module);

    assert!(
        exports.iter().any(|e| e == "add_one"),
        "DCE pruned `add_one` even though it is called; exports: {exports:?}"
    );
    for unused in ["twice", "unused_no_args", "unused_squared"] {
        assert!(
            exports.iter().all(|e| e != unused),
            "DCE failed to prune unused export `${unused}`; exports: {exports:?}"
        );
    }
}

#[test]
fn entirely_unused_wat_asset_is_not_embedded() {
    // A Wado program that name-imports wat exports but never references
    // any of them. The codegen should embed nothing from the asset —
    // the entire `wasm-mod-…` core module should be absent from the
    // component (or at least neither imported export should appear).
    let source = r#"
use { println, Stdout } from "core:cli";
use { add_one, twice } from "./sub/wasm_import_user.wat" with { type: "wat" };

export fn run() with Stdout {
    println("no wat call");
}
"#;
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let result = crate::common::compile_source_with_opts(
        &fixture_dir.join("__wasm_import_dce_unused_entry__.wado"),
        source,
        wado_compiler::OptLevel::default(),
    )
    .expect("compile should succeed");

    let parser = Parser::new(0);
    let mut core_modules: Vec<Vec<u8>> = Vec::new();
    for payload in parser.parse_all(&result.wasm) {
        if let Ok(Payload::ModuleSection {
            unchecked_range, ..
        }) = payload
        {
            core_modules.push(result.wasm[unchecked_range.start..unchecked_range.end].to_vec());
        }
    }
    for module in &core_modules {
        for export in function_exports(module) {
            assert!(
                export != "add_one" && export != "twice",
                "wat asset should be entirely DCE'd when no exports are called; \
                 found export `${export}`"
            );
        }
    }
}

/// The bundled `core:libm.wat` is the largest asset the prune ever sees: 87
/// functions, of which a program calling one math function needs a handful.
/// This holds the win honest against the real asset, not a toy fixture.
#[test]
fn the_bundled_libm_is_pruned_to_what_the_program_calls() {
    let full = wat::parse_bytes(wado_compiler::stdlib::CORE_LIBM_WAT).expect("libm.wat parses");
    let full_functions = code_entry_count(&full);

    let libm = embedded_libm("__libm_dce_entry__.wado");

    assert_eq!(
        function_exports(&libm),
        ["libm_sin"],
        "only the called export survives"
    );
    let kept = code_entry_count(&libm);
    assert!(
        kept < full_functions / 4,
        "libm should shrink sharply: kept {kept} of {full_functions} functions"
    );
}

/// The same asset carries a `wado.dataref` map, so the prune reaches its
/// rodata too. `sin` needs three of libm's tables and none of `exp2`'s 4 KB
/// one; without the map every program calling any math function carries all of
/// it.
#[test]
fn the_bundled_libm_keeps_only_the_data_the_program_reads() {
    let full = wat::parse_bytes(wado_compiler::stdlib::CORE_LIBM_WAT).expect("libm.wat parses");
    let full_bytes = data_byte_count(&full);
    assert!(
        full_bytes > 4096,
        "the asset should carry its tables: {full_bytes} bytes"
    );

    let libm = embedded_libm("__libm_data_dce_entry__.wado");
    let kept = data_byte_count(&libm);
    assert!(
        kept > 0 && kept < full_bytes / 4,
        "libm's data should shrink sharply: kept {kept} of {full_bytes} bytes"
    );
}

/// The map is what makes the assertion above possible, and an asset that
/// quietly lost it would only show up as a larger binary.
#[test]
fn the_bundled_libm_carries_its_data_reference_map() {
    let full = wat::parse_bytes(wado_compiler::stdlib::CORE_LIBM_WAT).expect("libm.wat parses");
    let mut carried = None;
    for payload in Parser::new(0).parse_all(&full) {
        if let Ok(Payload::CustomSection(reader)) = payload
            && reader.name() == wado_wasm_embed::dataref::SECTION_NAME
        {
            carried = Some(reader.data().to_vec());
        }
    }
    let carried = carried.unwrap_or_else(|| {
        panic!(
            "libm.wat must carry `{}`; regenerate with `mise run update-bundled`",
            wado_wasm_embed::dataref::SECTION_NAME
        )
    });
    let refs = wado_wasm_embed::dataref::DataRefs::parse(
        std::str::from_utf8(&carried).expect("the map is text"),
    )
    .expect("the map must parse");

    // Everything but the alignment padding, so anything short of that is drift.
    let claimed = refs.claimed_bytes() as usize;
    let total = data_byte_count(&full);
    assert!(
        claimed + 16 >= total && claimed <= total,
        "the map claims {claimed} of {total} data bytes; regenerate with \
         `mise run update-bundled`"
    );
}

/// The embedded libm core module of a program that calls `sin` and nothing
/// else. `black_box` keeps the argument off the constant-folding path, so the
/// bundled code really is reached.
fn embedded_libm(entry: &str) -> Vec<u8> {
    let source = r#"
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println(`${f64::sin(builtin::black_box(0.5))}`);
}
"#;
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let result = crate::common::compile_source_with_opts(
        &fixture_dir.join(entry),
        source,
        wado_compiler::OptLevel::default(),
    )
    .expect("compile should succeed");

    for payload in Parser::new(0).parse_all(&result.wasm) {
        if let Ok(Payload::ModuleSection {
            unchecked_range, ..
        }) = payload
        {
            let module = &result.wasm[unchecked_range.start..unchecked_range.end];
            if module_has_export(module, "libm_sin") {
                return module.to_vec();
            }
        }
    }
    panic!("the component embeds libm");
}

fn code_entry_count(module_bytes: &[u8]) -> usize {
    Parser::new(0)
        .parse_all(module_bytes)
        .filter(|p| matches!(p, Ok(Payload::CodeSectionEntry(_))))
        .count()
}

fn data_byte_count(module_bytes: &[u8]) -> usize {
    let mut total = 0;
    for payload in Parser::new(0).parse_all(module_bytes) {
        if let Ok(Payload::DataSection(reader)) = payload {
            for data in reader.into_iter().flatten() {
                total += data.data.len();
            }
        }
    }
    total
}
