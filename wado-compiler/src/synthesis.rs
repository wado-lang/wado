//! TIR synthesis phase.
//!
//! Generates synthetic TIR functions that cannot be expressed in user code:
//! 1. **Enum traits** — auto-derived `Eq` and `Ord` for enum types
//! 2. **Inspect/Display impls** — auto-generated `Inspect` and `Display` fallback impls
//! 3. **Template expansion** — expands `TemplateString` TIR nodes into formatting code
//! 4. **CM adapters** — Component Model boundary adapters for imports/exports
//!
//! All synthesis runs pre-monomorphize. Template expansion emits trait method calls
//! (`Display::fmt`, `Inspect::inspect`) that the monomorphizer resolves to concrete
//! implementations.

pub mod cm_adapter;
pub mod common;
pub mod serde_synth;
pub mod template;
pub mod traits;

use crate::project::Project;

/// Run pre-monomorphize synthesis phases on the project.
///
/// Execution order:
/// 1. Traits — generates `Eq`/`Ord` for enums, `Inspect`/`Display` for all types
/// 2. Template expansion — expands `TemplateString` nodes into trait method calls
/// 3. CM adapters — generates Component Model boundary adapters
pub fn synthesize(project: Project) -> Result<Project, String> {
    let project = traits::synthesize_traits(project);

    // Generate Serialize/Deserialize impls from `impl Trait for Type;` requests.
    let mut project = project;
    serde_synth::synthesize_serde(&mut project);

    // Expand template strings into Display/Inspect trait calls.
    // This must run after traits synthesis (which generates the impls)
    // but before monomorphization (which resolves the trait calls).
    for module in project.tir_modules.values() {
        let tt = module.type_table.clone();
        template::expand_templates(module, &tt);
    }

    let project = cm_adapter::generate_adapters(project)?;
    Ok(project)
}
