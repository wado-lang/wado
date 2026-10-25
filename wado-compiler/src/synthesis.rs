//! TIR synthesis: the functions user code cannot express — auto-derived `Eq` /
//! `Ord` for enums, `Inspect` / `Display` fallback impls, `TemplateString`
//! expansion into formatting code, and Component Model import / export adapters.
//! All of it runs pre-monomorphize, so the trait calls template expansion emits
//! resolve to concrete implementations there.

pub mod affine_drop;
pub mod cm_binding;
pub mod common;
pub mod effect_dispatch;
pub mod reflect_bridge;
pub mod serde_synth;
pub mod template;
pub mod traits;

use crate::compiler_host::Code;
use crate::defs::DefId;
use crate::elaborator::trait_env::{SynthesisedImpls, TraitEnv};
use crate::module_source::ModuleSource;
use crate::package::Package;
use crate::tir;
use crate::tir::ResolvedType;
use crate::token::Span;
use crate::type_table::TypeTable;

/// A synthesis failure, at the declaration it concerns where one is known.
#[derive(Debug)]
pub struct SynthesisError {
    pub code: Code,
    pub message: String,
    pub span: Option<Span>,
    /// The module `span` is in. `None` reports it against the entry module.
    pub module: Option<Box<ModuleSource>>,
}

impl SynthesisError {
    /// An unsupported construct at `span` in the entry module.
    pub fn at(span: Span, message: String) -> Self {
        Self {
            code: Code::UnsupportedFeature,
            message,
            span: Some(span),
            module: None,
        }
    }
}

impl From<String> for SynthesisError {
    fn from(message: String) -> Self {
        Self {
            code: Code::UnsupportedFeature,
            message,
            span: None,
            module: None,
        }
    }
}

impl From<SynthesisError> for Vec<SynthesisError> {
    fn from(error: SynthesisError) -> Self {
        vec![error]
    }
}

/// The five reflection kinds' metadata (WEP 2026-06-13 §1, §3b–d). Driven by
/// the declarations themselves, not by demand, so it runs exactly once: a
/// second run would re-emit every member function.
fn synthesize_reflect_metadata(project: &mut Package) {
    traits::synthesize_reflect(project);
    traits::synthesize_reflect_variant(project);
    traits::synthesize_reflect_enum(project);
    traits::synthesize_reflect_flags(project);
    traits::synthesize_reflect_newtype(project);
    traits::synthesize_reflect_template(project);
}

/// Run pre-monomorphize synthesis phases on the project.
///
/// Execution order:
/// 1. Reflection metadata — `Reflect{Struct,Variant,Enum,Flags,Newtype}`, which
///    the auto-derived bodies dispatch through
/// 2. Traits — generates `Eq`/`Ord` for enums, `Inspect`/`Display` for all types
/// 3. Template expansion — expands `TemplateString` nodes into trait method calls
/// 4. CM bindings — generates Component Model boundary adapters
pub fn synthesize(project: Package) -> Result<Package, Vec<SynthesisError>> {
    let mut project = project;

    // Reflection metadata first: an auto-derived body dispatches through the
    // `Reflect*` blankets, and `blanket_dispatch_for` can only project a
    // blanket's `Members` / `FieldTypes` pack once the kind's synthesis has
    // registered it. Emitting a body before that leaves the call naming a
    // per-type impl that never exists. Nothing here reads what the later
    // passes produce — the targets come from the declarations.
    synthesize_reflect_metadata(&mut project);

    let mut project = traits::synthesize_traits(project);

    serde_synth::synthesize_serde(&mut project);

    // Drain `Default` after serde: `Deserialize` bodies record `Field::default()`
    // requests later than `synthesize_traits`' snapshot (WEP 2026-06-25).
    traits::synthesize_defaults(&mut project);

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
        // A tagged template's `TemplateHole::fmt` renders a hole through the
        // same per-specifier dispatch as the direct untagged lowering, so its
        // bridge is minted here, by that lowering's code.
        template::synthesize_hole_fmt_helpers(module, &tt, &trait_env);
    }

    // Effect-dispatch wrapper synthesis and call-site rewriting run before
    // cm_binding, so a user resource call is intercepted at its pre-cm_binding
    // `cm_name`-tagged shape and routed to the dispatch wrapper, whose fallback
    // emits that same shape for cm_binding to rewrite uniformly. `WithHandler`
    // desugaring stays late: effect-check reads the original shape.
    let project =
        effect_dispatch::synthesize_pre_cm_binding(project).map_err(SynthesisError::from)?;

    // Insert `resource.drop` for every owned Component Model resource that is
    // never transferred. Runs before CM-binding synthesis, whose rewrite of
    // resource method calls would obscure the reference-vs-transfer distinction.
    let mut project = project;
    affine_drop::elaborate_affine_drops(&mut project);
    effect_dispatch::rewrite_resource_drops(&project);

    let project = cm_binding::generate_adapters(project)?;
    Ok(project)
}

/// Collect every `(type_name, trait_name) -> ModuleSource` triple TIR carries,
/// by walking `module.functions` — reify flattens impl-block methods into it.
/// Each entry records whether the impl is concrete (no impl-level type params),
/// which is how the monomorphizer picks the impl block's module over the
/// receiver's. Per-module stubs are excluded via [`receiver_is_per_module_synth`].
fn collect_synthesised_impls(project: &Package) -> SynthesisedImpls {
    let mut impls = SynthesisedImpls::default();
    let mut instantiations: Vec<(String, DefId, ModuleSource)> = Vec::new();
    // Every impl TIR carries is recorded, user-written ones included. This
    // layer answers "where is the code", and reify flattens a user impl's
    // methods into `module.functions` exactly like a generated one — so
    // excluding them would leave a mangled query with nothing to find.
    for tir_module in project.tir_modules.values() {
        let module_source = &tir_module.module_source;
        let type_table = tir_module.type_table.borrow();
        for func_rc in &tir_module.functions {
            let func = func_rc.borrow();
            if let Some(ref info) = func.method_info
                && let Some(trait_) = info.trait_decl()
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
                impls.record_impl(info.receiver(), trait_, module_source, is_concrete);
                if info.struct_name() != info.base_struct_name() {
                    instantiations.push((info.struct_name(), trait_, module_source.clone()));
                }
            }
        }
    }
    for (mangled, trait_, module) in instantiations {
        impls.record_instantiation(mangled, trait_, &module);
    }
    impls
}

/// Whether an impl on `receiver_type_id` is a *per-module* dispatch stub, to be
/// kept out of the project-wide synthesis layer. Strips `&` / `&mut`:
/// [`ResolvedType::Function`] and [`ResolvedType::GenericResource`] are
/// anonymous, each using module synthesising its own stub, so a project-wide
/// entry would mis-route. Every other type names one defining module.
fn receiver_is_per_module_synth(receiver_type_id: tir::TypeId, tt: &TypeTable) -> bool {
    matches!(
        tt.get(tt.peel_refs(receiver_type_id)),
        ResolvedType::Function { .. } | ResolvedType::GenericResource { .. }
    )
}
