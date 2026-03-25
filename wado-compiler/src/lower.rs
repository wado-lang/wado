//! Lowering pass for Wado TIR
//!
//! The lower phase performs type-driven transformations on TIR:
//! - Comparison lowering (convert `==`/`<`/etc. on structs to `Eq`/`Ord` trait method calls)
//! - Wide integer match lowering (convert i128/u128 match to if-else chains)
//! - Pattern lowering (transform `LetDestructure`/`IfLet` to explicit statements)
//! - Global initializer lowering (extract non-constant initializers)
//! - Boxing lowering (transform `&primitive` / `&mut primitive` to `Box<T>` struct operations)
//! - Closure lowering (transform closures to functor structs with `__call` methods)
//! - String literal collection (for data section)
//!
//! Note: All loop constructs are desugared at the AST level in desugar.rs.
//! Monomorphization has been moved to a separate phase (see `monomorphize.rs`).

mod boxing;
mod closure;
mod comparison;
mod globals;
mod pattern;
mod string;
mod wide_int;

use crate::hashmap::IndexMap;

use crate::name::ModuleSource;
use crate::project::Project;
use crate::tir::TirModule;

use boxing::BoxLowerer;
use closure::ClosureLowerer;
use comparison::lower_comparisons;
use globals::{generate_initialize_modules, lower_global_initializers};
use pattern::lower_patterns;
use string::StringCollector;
use wide_int::lower_wide_int_match_patterns;

/// Lower a TIR module
///
/// Performs:
/// 1. Global variable initialization lowering (extract non-constant initializers)
/// 2. Closure lowering (transform closures to functor structs with `__call` methods)
/// 3. String literal collection for the data section
///
/// Note: All loop constructs are desugared at the AST level in desugar.rs.
pub fn lower(module: TirModule) -> TirModule {
    let mut modules = IndexMap::default();
    modules.insert(module.module_source.clone(), module);
    let modules = modules;
    let mut modules = lower_modules_indexed(modules);
    modules.pop().unwrap().1
}

/// Run pre-boxing per-module lowering passes.
fn lower_pre_boxing(
    module: &mut TirModule,
    global_variant_map: &IndexMap<String, Vec<(String, u32)>>,
) {
    // Phase 0: Lower comparison operators on non-primitive types to trait method calls
    lower_comparisons(module);

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

/// Lower a Project (Project -> Project)
///
/// This is the main entry point for the lower phase. It lowers all TIR modules
/// in the project.
pub fn lower_project(mut project: Project) -> Project {
    project.tir_modules = lower_modules_indexed(project.tir_modules);

    // Post-processing: generate __initialize_modules in entry module
    generate_initialize_modules(&mut project.tir_modules);

    project
}

/// Lower multiple modules
///
/// Builds a global variant map from all modules so that pattern matching works
/// on imported variants, then applies lowering to each module.
pub fn lower_modules_indexed(
    modules: IndexMap<ModuleSource, TirModule>,
) -> IndexMap<ModuleSource, TirModule> {
    // Build a global variant map from ALL modules so that cross-module pattern
    // matching works (e.g., `if let Greater = ord` where Ordering is from another module)
    let mut global_variant_map: IndexMap<String, Vec<(String, u32)>> = IndexMap::default();
    for module in modules.values() {
        for variant in &module.variants {
            let cases: Vec<(String, u32)> = variant
                .cases
                .iter()
                .map(|c| (c.name.clone(), c.index))
                .collect();
            global_variant_map.insert(variant.name.clone(), cases);
        }
    }

    let mut modules: IndexMap<ModuleSource, TirModule> = modules
        .into_iter()
        .map(|(source, mut module)| {
            lower_pre_boxing(&mut module, &global_variant_map);
            (source, module)
        })
        .collect();

    // Phase 2: Lower boxing across ALL modules with a single BoxLowerer.
    // All modules share the same TypeTable, so box type creation and type
    // rewriting must happen once. The BoxLowerer scans the shared type table,
    // creates Box<T> struct types, rewrites Ref/MutRef types, then transforms
    // expressions in each module's functions.
    {
        let mut box_lowerer = BoxLowerer::new();

        // Build struct fields map and variant names from all modules
        for module in modules.values() {
            for s in &module.structs {
                box_lowerer.struct_fields_map.insert(
                    (s.name.clone(), module.module_source.clone()),
                    s.fields.clone(),
                );
            }
            for v in &module.variants {
                box_lowerer.variant_names.insert(v.name.clone());
            }
        }

        // Use any module's type_table (they all share the same Rc<RefCell<TypeTable>>)
        if let Some(first_module) = modules.values().next() {
            let mut type_table = first_module.type_table.borrow_mut();
            box_lowerer.create_needed_box_types(&mut type_table);
            box_lowerer.rewrite_types(&mut type_table);
        }

        // Transform expressions per module
        for module in modules.values_mut() {
            box_lowerer.lower_module_exprs(module);
        }

        // Inject generated Box structs into core:internal module (where they logically live).
        // Falls back to entry module if core:internal doesn't exist (e.g., single-module tests).
        if !box_lowerer.generated_structs.is_empty() {
            let internal_source = ModuleSource::internal();
            let has_internal = modules.contains_key(&internal_source);
            let target_module = if has_internal {
                modules.get_mut(&internal_source).unwrap()
            } else {
                modules.values_mut().next().unwrap()
            };
            target_module
                .structs
                .append(&mut box_lowerer.generated_structs);
        }
    }

    for module in modules.values_mut() {
        lower_post_boxing(module);
    }

    modules
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lower_passthrough() {
        let module = TirModule::new(ModuleSource::Local {
            path: "test".to_string(),
        });
        let lowered = lower(module);
        assert_eq!(
            lowered.module_source,
            ModuleSource::Local {
                path: "test".to_string()
            }
        );
    }

    #[test]
    fn test_string_collector_empty() {
        let module = TirModule::new(ModuleSource::Local {
            path: "test".to_string(),
        });
        let lowered = lower(module);
        assert!(lowered.string_literals.is_empty());
    }
}
