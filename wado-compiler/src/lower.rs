//! Lowering pass for Wado TIR
//!
//! The lower phase performs type-driven transformations on TIR:
//! - Wide integer match lowering (convert i128/u128 match to if-else chains)
//! - Pattern lowering (transform `LetDestructure`/`IfLet` to explicit statements)
//! - Global initializer lowering (extract non-constant initializers)
//! - Boxing lowering (transform `&primitive` / `&mut primitive` to `Box<T>` struct operations)
//! - Closure lowering (transform closures to functor structs with `__call` methods)
//! - String literal collection (for data section)
//! - Value-copy materialization (insert `$value_copy$T` markers and synthesize helpers)
//!
//! The phase exits as a TIR → NIR translation: TIR-mutating sub-passes run first
//! (`lower_in_place`), then [`plan::plan`] gathers analysis facts and synthesizes
//! artefacts, then [`translate::translate`] folds the TIR into NIR consuming the
//! plan. See `docs/wep-2026-05-11-nir.md`.

mod boxing;
mod globals;
mod pattern;
pub mod plan;
pub mod translate;
mod wide_int;

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexMap;

use crate::module_source::ModuleSource;
use crate::nir_package::NirPackage;
use crate::tir::TirModule;

use boxing::BoxLowerer;
use globals::{generate_initialize_modules_flat, lower_global_initializers};
use pattern::lower_patterns;
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
///
/// Closure lowering and string/bytes literal collection used to run
/// here; both moved into [`plan::plan`], which runs after
/// [`lower_in_place`] returns.
fn lower_post_boxing(_module: &mut TirModule) {}

/// Lower a `FlatPackage` and return a `NirPackage`.
///
/// The phase is structured as `in-place TIR mutation → planner → translator`:
/// the in-place chain implements the historical lower sub-passes; the
/// planner gathers analysis facts and synthesizes artefacts (currently:
/// value-copy markers and `$value_copy$T<id>` helpers); the translator
/// folds TIR into NIR consuming the plan.
pub fn lower(mut flat: FlatPackage) -> NirPackage {
    lower_in_place(&mut flat);
    let plan = plan::plan(&mut flat);
    translate::translate(flat, plan)
}

fn lower_in_place(flat: &mut FlatPackage) {
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

    // Closure functors are produced by `plan::closure`, not the in-place
    // pipeline; nothing to copy back here.

    // Post-processing: generate __initialize_modules
    generate_initialize_modules_flat(flat);

    // Rebuild variant index since data may have changed
    flat.rebuild_variant_indices();
}
