//! TIR synthesis phase.
//!
//! Generates synthetic TIR functions that cannot be expressed in user code:
//! 1. **Enum traits** — auto-derived `Eq` and `Ord` for enum types
//! 2. **Template expansion** — expands `TemplateString` TIR nodes into formatting code
//! 3. **Inspect** — debug output functions for `:?` format specifier and Display fallback
//! 4. **CM adapters** — Component Model boundary adapters for imports/exports
//!
//! Template and inspect synthesis run in a single post-monomorphize pass,
//! where all type parameters have been substituted with concrete types.
//! This eliminates the need for marker-based deferred resolution.

pub mod cm_adapter;
pub mod common;
pub mod inspect;
pub mod template;
pub mod traits;

use crate::name::ModuleSource;
use crate::project::Project;

/// Run pre-monomorphize synthesis phases on the project.
///
/// Execution order:
/// 1. Traits — generates `Eq`/`Ord` for enums
/// 2. CM adapters — generates Component Model boundary adapters
///
/// Template expansion and inspect synthesis are deferred to
/// `synthesize_post_monomorphize` where all types are concrete.
pub fn synthesize(project: Project) -> Result<Project, String> {
    let project = traits::synthesize_traits(project);
    let project = cm_adapter::generate_adapters(project)?;
    Ok(project)
}

/// Run post-monomorphize synthesis: template expansion + inspect generation.
///
/// After monomorphization, all type parameters are concrete, so we can:
/// 1. Expand `TemplateString` nodes into formatting code with correct Display/Inspect dispatch
/// 2. Generate inspect functions for all types encountered during expansion
///
/// This replaces the previous two-pass inspect system (pre- and post-monomorphize)
/// with a single, unified pass.
pub fn synthesize_post_monomorphize(mut project: Project) -> Project {
    let module_sources: Vec<ModuleSource> = project.tir_modules.keys().cloned().collect();

    for module_source in &module_sources {
        let new_functions = {
            let module = project.tir_modules.get(module_source).unwrap();
            let tt = module.type_table.clone();
            let all_modules: Vec<_> = project.tir_modules.values().collect();

            let formatter_struct = tt
                .borrow_mut()
                .make_struct("Formatter".to_string(), ModuleSource::format());
            let fmt_type = tt.borrow_mut().make_mut_ref(formatter_struct);

            let mut reg = inspect::InspectRegistry::new();

            // Phase 1: Expand templates (this may register inspect functions)
            template::expand_templates(
                module,
                &tt,
                &all_modules,
                &mut reg,
                fmt_type,
                module_source,
            );

            // Phase 2: Replace any remaining builtin::inspect markers
            // (from non-template code like `println` with inspect format)
            inspect::replace_markers_in_module(
                module,
                &tt,
                &all_modules,
                &mut reg,
                fmt_type,
                module_source,
            );

            // Phase 3: Generate inspect function bodies
            inspect::generate_pending_inspect_fns(
                &mut reg,
                &tt,
                &all_modules,
                fmt_type,
                module_source,
            );

            reg.into_functions()
        };

        let module = project.tir_modules.get_mut(module_source).unwrap();
        for func_rc in new_functions {
            module.functions.push(func_rc);
        }
    }

    project
}
