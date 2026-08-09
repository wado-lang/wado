//! The `#![wasm_module("mem")]` core module — the allocator — goes through
//! `wir_optimize` as a package of its own. These pin that, via two rewrites only
//! `wir_optimize::peephole` performs (`wir_build` emits neither shape):
//! `i32.eq x, 0` → `i32.eqz`, and `local.set n; …; local.get n` → `local.tee n`.

use std::path::Path;

use wado_compiler::{CompilerOptions, OptLevel};

/// Any program pulls in the allocator, so the bodies under test are the bundled
/// `bump_realloc` / `grow_memory`, not anything this source writes.
const SOURCE: &str = r#"
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println("hi");
}
"#;

/// Just the `$mem-mod` core module: asserting on the whole component would also
/// match the main module, where these rewrites have always run.
fn mem_module_wat(opt_level: OptLevel) -> String {
    mem_module_wat_with_allocator(opt_level, None)
}

fn mem_module_wat_with_allocator(opt_level: OptLevel, allocator: Option<&str>) -> String {
    let options = CompilerOptions {
        opt_level,
        allocator: allocator.map(String::from),
        ..Default::default()
    };
    let result = crate::common::compile_source_with_compiler_options(
        Path::new("wasm_module_optimize_test.wado"),
        SOURCE,
        options,
    )
    .expect("compilation should succeed");
    let wat = wasmprinter::print_bytes(&result.wasm).expect("disassemble wasm to WAT");

    let start = wat
        .find("(core module $mem-mod")
        .unwrap_or_else(|| panic!("no $mem-mod core module in:\n{wat}"));
    let rest = &wat[start..];
    let end = rest[1..]
        .find("(core module ")
        .map_or(rest.len(), |i| i + 1);
    rest[..end].to_string()
}

#[test]
fn allocator_core_module_is_peepholed() {
    let mem = mem_module_wat(OptLevel::O2);
    assert!(
        mem.contains("i32.eqz"),
        "allocator's `newsize == 0` test should be peepholed to `i32.eqz`; \
         mem module was:\n{mem}"
    );
    assert!(
        mem.contains("local.tee"),
        "allocator's set-then-get local should be fused to `local.tee`; \
         mem module was:\n{mem}"
    );
}

/// DCE compacts the wasm module's function list, so its name section has to be
/// derived from the survivors. `bump` happens to leave its two survivors at
/// their original indices; `freelist` keeps functions that sit *behind* dropped
/// ones, so a name list built before compaction shifts onto the wrong functions
/// and hands every survivor a dead function's name.
#[test]
fn wasm_module_names_survive_dce_compaction() {
    let mem = mem_module_wat_with_allocator(OptLevel::O2, Some("freelist"));
    assert!(
        mem.contains(r#"(export "realloc" (func $realloc))"#),
        "the exported `realloc` should name the surviving realloc; mem module was:\n{mem}"
    );
    for dropped in ["bump_realloc", "debug_realloc"] {
        assert!(
            !mem.contains(dropped),
            "`{dropped}` is DCE'd under --allocator freelist, so its name must not \
             land on a survivor; mem module was:\n{mem}"
        );
    }
}
