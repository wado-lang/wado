//! TIR synthesis phase.
//!
//! Generates synthetic TIR functions that cannot be expressed in user code:
//! 1. **Enum traits** — auto-derived `Eq` and `Ord` for enum types
//! 2. **Inspect/Display impls** — auto-generated `Inspect` and `Display` fallback impls
//! 3. **Template expansion** — expands `TemplateString` TIR nodes into formatting code
//! 4. **CM bindings** — Component Model boundary adapters for imports/exports
//!
//! All synthesis runs pre-monomorphize. Template expansion emits trait method calls
//! (`Display::fmt`, `Inspect::inspect`) that the monomorphizer resolves to concrete
//! implementations.

pub mod cm_binding;
pub mod common;
pub mod effect_dispatch;
pub mod from_synth;
pub mod kiln_synth;
pub mod serde_synth;
pub mod template;
pub mod traits;

use crate::package::Package;
use template::{TraitMethodLocations, trait_method_location_key};

/// Run pre-monomorphize synthesis phases on the project.
///
/// Execution order:
/// 1. Traits — generates `Eq`/`Ord` for enums, `Inspect`/`Display` for all types
/// 2. Template expansion — expands `TemplateString` nodes into trait method calls
/// 3. CM bindings — generates Component Model boundary adapters
pub fn synthesize(project: Package) -> Result<Package, String> {
    let project = traits::synthesize_traits(project);

    // Generate From impls from `impl From<T> for Type;` requests.
    // Must run before serde_synth which drains remaining synthesis requests.
    let mut project = project;
    for module in project.tir_modules.values_mut() {
        from_synth::synthesize_from(module);
    }

    // Kiln generators need `Options::deserialize` to decode the canonical
    // JSON wire form, but the author does not write
    // `impl Deserialize for Options;` explicitly. Inject the synthesis
    // request here so the following `serde_synth` pass picks it up.
    kiln_synth::prepare_kiln(&mut project);

    // Generate Serialize/Deserialize impls from `impl Trait for Type;` requests.
    serde_synth::synthesize_serde(&mut project);

    // Expand template strings into Display/Inspect trait calls.
    // This must run after traits synthesis (which generates the impls)
    // but before monomorphization (which resolves the trait calls).
    let trait_method_locations = collect_trait_method_locations(&project);
    for module in project.tir_modules.values_mut() {
        let tt = module.type_table.clone();
        template::expand_templates(module, &tt, &trait_method_locations);
    }

    // Effect-dispatch wrapper synthesis + call-site rewriting must
    // run BEFORE cm_binding so that user resource calls (like
    // `tx.write(payload)` on a `StreamWritable<u8>`) get intercepted
    // at their pre-cm_binding shape — `MethodCall`/`Call` with
    // `func.method_info.cm_name` set — and routed to the per-
    // monomorphisation dispatch wrapper. The wrapper bodies' fallback
    // paths emit the same cm_name-tagged placeholder shape that user
    // code emits, so cm_binding rewrites both uniformly afterward
    // (turning `stream-write` placeholders into `cm_stream_write_u8`
    // internal calls, etc.). The `WithHandler` desugaring stays late
    // (in `compile_after_load`'s phase 8c) because effect-check needs
    // the original `WithHandler` shape to know which effects are
    // satisfied locally.
    let project = effect_dispatch::synthesize_pre_cm_binding(project)?;

    let project = cm_binding::generate_adapters(project)?;
    Ok(project)
}

/// Build a project-wide index of trait-method definition locations from
/// every TIR module, keyed by [`trait_method_location_key`]. Template
/// expansion uses this to emit calls that point at the module containing
/// the impl, regardless of where the receiver type is declared.
fn collect_trait_method_locations(project: &Package) -> TraitMethodLocations {
    let mut locations = TraitMethodLocations::default();
    for tir_module in project.tir_modules.values() {
        let module_source = &tir_module.module_source;
        for func_rc in &tir_module.functions {
            let func = func_rc.borrow();
            if let Some(ref info) = func.method_info
                && info.trait_name.is_some()
                && let Some(key) = trait_method_location_key(info)
            {
                locations.entry(key).or_insert_with(|| module_source.clone());
            }
        }
        for impl_block in &tir_module.impls {
            if impl_block.trait_name.is_none() {
                continue;
            }
            for method in &impl_block.methods {
                if let Some(ref info) = method.method_info
                    && info.trait_name.is_some()
                    && let Some(key) = trait_method_location_key(info)
                {
                    locations.entry(key).or_insert_with(|| module_source.clone());
                }
            }
        }
    }
    locations
}
