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

use crate::module_source::ModuleSource;
use crate::package::Package;
use crate::resolver::trait_env::{SynthesisedImpls, TraitEnv};
use crate::tir::ResolvedType;

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

    // Snapshot the synthesis-layer impls (auto-derives + From/serde adapters)
    // onto `TraitEnv` so subsequent phases query a single source of truth
    // instead of rescanning TIR. The AST layer is preserved unchanged.
    let synth_impls = collect_synthesised_impls(&project);
    project.trait_env = TraitEnv::extend_with_synthesised(project.trait_env, synth_impls);

    // Expand template strings into Display/Inspect trait calls.
    // This must run after traits synthesis (which generates the impls)
    // but before monomorphization (which resolves the trait calls).
    let trait_env = project.trait_env.clone();
    for module in project.tir_modules.values_mut() {
        let tt = module.type_table.clone();
        template::expand_templates(module, &tt, &trait_env);
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

/// Collect every `(type_name, trait_name) -> ModuleSource` triple that the
/// synthesis phase added to TIR, regardless of which sub-pass produced it
/// (auto-derives, `from_synth`, `serde_synth`). Walks both `module.functions`
/// (free functions and synthesized wrappers) and `module.impls` (user-written
/// impl blocks lowered into TIR).
///
/// User-written impls already live in [`TraitEnv::trait_impl_modules`] from
/// the AST layer, so they are excluded here to keep the synthesis layer a
/// genuine "delta" on top of the AST.
///
/// Concrete-ness is propagated alongside each entry so that the
/// monomorphizer can later distinguish "the impl is fully resolved (use the
/// impl block's module)" from "the impl is generic (fall back to the
/// receiver type's module so `call_rewrite`'s path-2alt mismatches the
/// queueing convention exactly the way the legacy `trait_method_locations`
/// did)". An impl is concrete here when the synthesized function carries
/// no impl-level type parameters.
///
/// Per-module synthesis stubs are deliberately *not* recorded: every
/// module that uses a closure of a given `Fn<arity, Ret>` shape (or an
/// opaque resource handle like `Stream<u8>`, `Future<i32>`, …) gets its
/// own independent stub, and a project-wide `(base, trait)` entry would
/// mis-route calls between modules. The decision is made from the impl's
/// receiver type via [`receiver_is_per_module_synth`]; template expansion
/// handles their dispatch via its own per-`ResolvedType` fallback.
fn collect_synthesised_impls(project: &Package) -> SynthesisedImpls {
    let mut impls = SynthesisedImpls::default();
    let ast_layer = &project.trait_env.trait_impl_modules;
    let mut record =
        |type_name: String, trait_name: String, module: &ModuleSource, is_concrete: bool| {
            let key = (type_name, trait_name);
            // Skip only when the *same* module is already represented at the
            // AST layer. Two same-name receiver types from different modules
            // (e.g. `struct Widget` in module A and module B) each get their
            // own auto-derived impl and both need to land in the synthesised
            // layer — a coarser `contains_key` short-circuit would drop the
            // second one and re-introduce the cross-module mis-dispatch the
            // multi-valued index is meant to fix.
            if ast_layer
                .get(&key)
                .is_some_and(|modules| modules.contains(module))
            {
                return;
            }
            impls.record_impl(key.0, key.1, module.clone(), is_concrete);
        };
    for tir_module in project.tir_modules.values() {
        let module_source = &tir_module.module_source;
        let type_table = tir_module.type_table.borrow();
        for func_rc in &tir_module.functions {
            let func = func_rc.borrow();
            if let Some(ref info) = func.method_info
                && let Some(ref trait_name) = info.trait_name
            {
                // Resolve the impl's receiver type from `self`'s declared
                // type and skip per-module synthesis stubs (Fn dispatch
                // stubs, opaque resource handles). Decision is made from
                // the type itself rather than name heuristics.
                if func
                    .params
                    .first()
                    .is_some_and(|p| receiver_is_per_module_synth(p.type_id, &type_table))
                {
                    continue;
                }
                let is_concrete = func.impl_type_params.is_empty();
                record(
                    info.base_struct_name.clone(),
                    trait_name.clone(),
                    module_source,
                    is_concrete,
                );
            }
        }
        for impl_block in &tir_module.impls {
            let Some(ref trait_name) = impl_block.trait_name else {
                continue;
            };
            let is_concrete = impl_block.type_params.is_empty();
            for method in &impl_block.methods {
                if let Some(ref info) = method.method_info
                    && info.trait_name.is_some()
                {
                    record(
                        info.base_struct_name.clone(),
                        trait_name.clone(),
                        module_source,
                        is_concrete,
                    );
                }
            }
        }
    }
    impls
}

/// Decide whether a synthesised trait-method impl whose `&self` parameter
/// is `receiver_type_id` should be skipped from the project-wide synthesis
/// layer because it is a *per-module* dispatch stub.
///
/// Strips `&` / `&mut` and consults the underlying `ResolvedType`:
///
/// - [`ResolvedType::Function`] / [`ResolvedType::GenericResource`] are
///   anonymous parameterized types with no source-level definition; every
///   module that uses them synthesises its own dispatch stub, so a
///   project-wide entry would mis-route cross-module calls.
/// - All other resolved types — `Struct`, `Enum`, `Variant`, `Newtype`,
///   `Flags`, `Primitive`, generic instances, and tuple instances — name
///   a single defining module, and any auto-derived impl for them is
///   project-wide.
fn receiver_is_per_module_synth(
    receiver_type_id: crate::tir::TypeId,
    tt: &crate::tir::TypeTable,
) -> bool {
    let mut tid = receiver_type_id;
    loop {
        match tt.get(tid) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => tid = *inner,
            ResolvedType::Function { .. } | ResolvedType::GenericResource { .. } => return true,
            _ => return false,
        }
    }
}
