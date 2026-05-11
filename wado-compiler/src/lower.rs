//! Lowering pass for Wado TIR
//!
//! The lower phase performs type-driven transformations on TIR:
//! - Wide integer match lowering (convert i128/u128 match to if-else chains)
//! - Pattern lowering (transform `LetDestructure`/`IfLet` to explicit statements)
//! - Global initializer lowering (extract non-constant initializers)
//! - Boxing lowering (transform `&primitive` / `&mut primitive` to `Box<T>` struct operations)
//! - Closure lowering (transform closures to functor structs with `__call` methods)
//! - String literal collection (for data section)
//! - Value-copy materialization (insert + synthesize `$value_copy$T` helpers)
//!
//! Note: All loop constructs are desugared at the AST level in desugar.rs.
//! Monomorphization has been moved to a separate phase (see `monomorphize.rs`).

mod boxing;
mod closure;
mod globals;
mod pattern;
mod string;
pub(crate) mod value_copy;
mod wide_int;

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexMap;

use crate::module_source::ModuleSource;
use crate::tir::TirModule;

use boxing::BoxLowerer;
use closure::ClosureLowerer;
use globals::{generate_initialize_modules_flat, lower_global_initializers};
use pattern::lower_patterns;
use string::StringCollector;
use wide_int::lower_wide_int_match_patterns;

/// Run pre-boxing per-module lowering passes.
fn lower_pre_boxing(
    module: &mut TirModule,
    global_variant_map: &IndexMap<(String, ModuleSource), Vec<(String, u32)>>,
) {
    // Phase 1: Lower i128/u128 match patterns to if-else chains
    lower_wide_int_match_patterns(module);

    // Phase 1.5: Lower patterns (LetDestructure, IfLet) to explicit Let statements
    lower_patterns(module, global_variant_map);

    // Phase 2: Lower global variable initializers
    lower_global_initializers(module);
}

/// Run post-boxing per-module lowering passes.
fn lower_post_boxing(module: &mut TirModule) {
    // Phase 3: Lower closures to functor structs
    let mut closure_lowerer = ClosureLowerer::new(&module.module_source);
    closure_lowerer.lower_module(module);

    // Phase 3b: Collect string literals (and bytes literals) and their function mappings
    let mut collector = StringCollector::new();
    collector.collect_module(module);
    let (strings, bytes, function_strings, function_method_info) = collector.into_results();
    module.string_literals = strings;
    module.bytes_literals = bytes;
    module.function_strings = function_strings;
    module.function_method_info = function_method_info;
}

/// Lower a `FlatPackage` in place.
///
/// This is the main entry point for the lower phase. It creates a temporary
/// `TirModule` from the flat data, runs lowering, and writes results back.
pub fn lower(flat: &mut FlatPackage) {
    let entry_module_source = flat.entry_module_source.clone();

    // Create a temporary TirModule with all flat data for lowering.
    let mut temp_module = TirModule::new(entry_module_source);
    temp_module.type_table = flat.type_table.clone();
    temp_module.functions = std::mem::take(&mut flat.functions);
    temp_module.structs = std::mem::take(&mut flat.structs);
    temp_module.globals = std::mem::take(&mut flat.globals);
    temp_module.variants = std::mem::take(&mut flat.variants);
    temp_module.enums = std::mem::take(&mut flat.enums);
    temp_module.flags = std::mem::take(&mut flat.flags);
    temp_module.imports = std::mem::take(&mut flat.imports);

    // Build global variant map from all variants. Keyed by
    // (name, module_source) so two modules can each declare a
    // variant with the same name without colliding — pattern
    // lowering looks up by both axes via the resolved type's
    // module_source. Keying by name only would silently overwrite
    // one module's cases with the other's and break cross-module
    // pattern lookups (regression test
    // `cross_module_same_name_variant_iflet.wado`).
    let mut global_variant_map: IndexMap<(String, ModuleSource), Vec<(String, u32)>> =
        IndexMap::default();
    for variant in &temp_module.variants {
        let cases: Vec<(String, u32)> = variant
            .cases
            .iter()
            .map(|c| (c.name.clone(), c.index))
            .collect();
        global_variant_map.insert((variant.name.clone(), variant.module_source.clone()), cases);
    }

    // Pre-boxing passes
    lower_pre_boxing(&mut temp_module, &global_variant_map);

    // Boxing pass (single BoxLowerer on all data)
    {
        let box_module_source = temp_module
            .type_table
            .borrow()
            .box_module_source
            .clone()
            .unwrap_or_else(ModuleSource::prelude);
        let mut box_lowerer = BoxLowerer::new(box_module_source);

        for s in &temp_module.structs {
            box_lowerer
                .struct_fields_map
                .insert((s.name.clone(), s.module_source.clone()), s.fields.clone());
        }
        for v in &temp_module.variants {
            box_lowerer.variant_names.insert(v.name.clone());
        }

        {
            let mut type_table = temp_module.type_table.borrow_mut();
            box_lowerer.create_needed_box_types(&mut type_table);
            box_lowerer.rewrite_types(&mut type_table);
        }

        box_lowerer.lower_module_exprs(&mut temp_module);

        temp_module
            .structs
            .append(&mut box_lowerer.generated_structs);
    }

    // Post-boxing passes (closures, string collection)
    lower_post_boxing(&mut temp_module);

    // Write results back to FlatPackage
    flat.functions = temp_module.functions;
    flat.structs = temp_module.structs;
    flat.globals = temp_module.globals;
    flat.variants = temp_module.variants;
    flat.enums = temp_module.enums;
    flat.flags = temp_module.flags;
    flat.imports = temp_module.imports;

    // String/bytes literals and function string maps from the string collector
    // The StringCollector already uses (module_source, func_name) compound keys
    flat.string_literals = temp_module.string_literals;
    flat.bytes_literals = temp_module.bytes_literals;
    flat.function_strings = temp_module.function_strings;
    flat.function_method_info = temp_module.function_method_info;

    // Closure functors
    flat.closure_functors = temp_module.closure_functors;

    // Post-processing: generate __initialize_modules
    generate_initialize_modules_flat(flat);

    // Rebuild variant index since data may have changed
    flat.rebuild_variant_indices();

    // Materialize Wado's value-copy semantics in TIR. Insertion places
    // `builtin::copy_value::<T>(x)` wrappers at every defensive deep-copy
    // position; synthesis replaces those wrappers with calls to per-type
    // `$value_copy$T<id>` helpers. Both run here — before the optimizer —
    // so that every aliasing edge downstream passes care about is explicit
    // in the TIR fed to `optimize`. Wrapper elision happens later as a
    // regular pass inside the optimizer fixed-point loop.
    value_copy::insert_value_copy_calls(flat);
    value_copy::synthesize_value_copy_funcs(flat);
}
