//! Effect, stores, and default-purity checking for Wado (Design B).
//!
//! These checks validate, respectively, that every call holds the effects its
//! callee requires, that a reference parameter that escapes declares
//! `stores[param]`, and that parameter / field defaults are pure.
//!
//! All three operate on [`Semantics`] (the AST plus the facts recorded during
//! `annotate`), not on the emitted TIR. They therefore see every source
//! function regardless of what reify emits — immune to dead-code gating — and
//! run on the LSP path, which builds no TIR. Each returns its violations so the
//! caller can route them (LSP diagnostics or the batch logger).

use crate::hashmap::{IndexMap, IndexSet};

use crate::module_source::ModuleSource;
use crate::tir::{EffectRef, FunctionRef, ResolvedType, TypeId, TypeSet, TypeTable};
use crate::token::Span;

use crate::ast::{self, AstId, AstVisitor, Expr, Function, Item, Stmt};
use crate::semantics::Semantics;

/// Whether a missing `with` entry refers to a resource or a regular effect.
/// Used to select the diagnostic wording (`missing resource` vs `missing effect`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectKind {
    /// A `resource` declaration — the caller needs the resource capability.
    Resource,
    /// An `effect` declaration (or unknown — default wording).
    Effect,
}

impl EffectKind {
    fn noun(self) -> &'static str {
        match self {
            EffectKind::Resource => "resource",
            EffectKind::Effect => "effect",
        }
    }
}

/// Error from effect checking
#[derive(Debug, Clone)]
pub struct EffectError {
    /// The function being called
    pub callee: String,
    /// The missing effect
    pub missing_effect: String,
    /// Whether the missing item is a resource or a regular effect
    pub kind: EffectKind,
    /// Source location of the call
    pub span: Span,
    pub module: String,
}

impl std::fmt::Display for EffectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: missing {} '{}' required by '{}'",
            self.span.line,
            self.span.column,
            self.kind.noun(),
            self.missing_effect,
            self.callee
        )
    }
}

impl std::error::Error for EffectError {}

impl From<EffectError> for crate::compiler_host::Diagnostic {
    fn from(e: EffectError) -> Self {
        use crate::compiler_host::{Code, DiagnosticSpan, Severity};
        crate::compiler_host::Diagnostic {
            severity: Severity::Error,
            code: Code::TypeMismatch,
            message: format!(
                "missing {} '{}' required by '{}'",
                e.kind.noun(),
                e.missing_effect,
                e.callee
            ),
            span: Some(DiagnosticSpan::from_span(&e.span, Some(&e.module))),
        }
    }
}

/// Error from stores checking
#[derive(Debug, Clone)]
pub struct StoresError {
    /// Description of the violation
    pub message: String,
    /// Source location
    pub span: Span,
    pub module: String,
}

impl std::fmt::Display for StoresError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: {}",
            self.span.line, self.span.column, self.message
        )
    }
}

impl std::error::Error for StoresError {}

impl From<StoresError> for crate::compiler_host::Diagnostic {
    fn from(e: StoresError) -> Self {
        use crate::compiler_host::{Code, DiagnosticSpan, Severity};
        crate::compiler_host::Diagnostic {
            severity: Severity::Error,
            code: Code::TypeMismatch,
            message: e.message.clone(),
            span: Some(DiagnosticSpan::from_span(&e.span, Some(&e.module))),
        }
    }
}

/// Error from default-value purity checking
#[derive(Debug, Clone)]
pub struct DefaultPurityError {
    pub callee: String,
    pub span: Span,
    pub module: String,
}

impl std::fmt::Display for DefaultPurityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: default value expression must be pure (no effects), but calls effectful function '{}'",
            self.span.line, self.span.column, self.callee
        )
    }
}

impl std::error::Error for DefaultPurityError {}

impl From<DefaultPurityError> for crate::compiler_host::Diagnostic {
    fn from(e: DefaultPurityError) -> Self {
        use crate::compiler_host::{Code, DiagnosticSpan, Severity};
        crate::compiler_host::Diagnostic {
            severity: Severity::Error,
            code: Code::TypeMismatch,
            message: format!(
                "default value expression must be pure (no effects), but calls effectful function '{}'",
                e.callee
            ),
            span: Some(DiagnosticSpan::from_span(&e.span, Some(&e.module))),
        }
    }
}

/// Walk a type recursively, collecting every resource (`Resource` or
/// `GenericResource`) reference as an `EffectRef::Concrete`.
///
/// Handles nested containers (`Option<T>`, `Result<T,E>`, tuples, `List<T>`,
/// function types, refs, newtypes, struct fields, variant case payloads).
/// Uses `visited` to stop at cycles (e.g. recursive struct types).
fn collect_resource_refs(
    type_id: TypeId,
    tt: &TypeTable,
    struct_fields: &IndexMap<(ModuleSource, String), Vec<TypeId>>,
    variant_payloads: &IndexMap<(ModuleSource, String), Vec<TypeId>>,
    out: &mut IndexSet<EffectRef>,
    visited: &mut TypeSet,
) {
    if !visited.insert(type_id) {
        return;
    }
    let ty = tt.get(type_id);
    match ty {
        ResolvedType::Resource {
            name,
            module_source,
        }
        | ResolvedType::GenericResource {
            name,
            module_source,
            ..
        } => {
            out.insert(EffectRef::Concrete {
                name: name.clone(),
                module_source: module_source.clone(),
            });
            if let ResolvedType::GenericResource { type_args, .. } = ty {
                for ta in type_args {
                    collect_resource_refs(*ta, tt, struct_fields, variant_payloads, out, visited);
                }
            }
        }
        ResolvedType::GenericInstance { type_args, .. } => {
            for ta in type_args {
                collect_resource_refs(*ta, tt, struct_fields, variant_payloads, out, visited);
            }
        }
        ResolvedType::Ref(t)
        | ResolvedType::MutRef(t)
        | ResolvedType::Reactive(t)
        | ResolvedType::BuiltinArray(t) => {
            collect_resource_refs(*t, tt, struct_fields, variant_payloads, out, visited);
        }
        ResolvedType::Function {
            params,
            return_type,
            ..
        } => {
            for p in params {
                collect_resource_refs(*p, tt, struct_fields, variant_payloads, out, visited);
            }
            collect_resource_refs(
                *return_type,
                tt,
                struct_fields,
                variant_payloads,
                out,
                visited,
            );
        }
        ResolvedType::Newtype { base_type, .. } => {
            collect_resource_refs(
                *base_type,
                tt,
                struct_fields,
                variant_payloads,
                out,
                visited,
            );
        }
        ResolvedType::Struct {
            name,
            module_source,
            ..
        } => {
            if let Some(fields) = struct_fields.get(&(module_source.clone(), name.clone())) {
                for ft in fields {
                    collect_resource_refs(*ft, tt, struct_fields, variant_payloads, out, visited);
                }
            }
        }
        ResolvedType::Variant {
            name,
            module_source,
        } => {
            if let Some(payloads) = variant_payloads.get(&(module_source.clone(), name.clone())) {
                for pt in payloads {
                    collect_resource_refs(*pt, tt, struct_fields, variant_payloads, out, visited);
                }
            }
        }
        // Primitives, Unit, Never, Enum, Flags, TypeParam, TypePack,
        // AssocTypeProjection, Unknown, Error — no resource refs.
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Semantics-based effect checking (Design B, Phase 1b)
// ---------------------------------------------------------------------------

/// Effect checking over [`Semantics`] (AST + recorded facts) — the Design B
/// effect checker. It runs after `annotate_bodies`, so it sees every function,
/// dead or live, and is independent of what reify emits. It also works on the
/// LSP path, which builds no TIR. Violations are returned rather than emitted
/// so the caller routes them (LSP diagnostics or the batch logger).
///
/// Covers free-function, method, and static dispatch with resource injection,
/// the effect / resource propagation closure, signature-resource inference,
/// effect-parameter resolution, `#[benign]`, handler-scope grants, and
/// indirect (closure) calls, over user-authored modules.
#[must_use]
pub fn check_effects_semantic(sem: &Semantics) -> Vec<EffectError> {
    let mut out = Vec::new();
    let Some(state) = sem.state.as_ref() else {
        return out;
    };
    let data = OwnedEffectData::build(sem, state);
    run_effect_checks(sem, &data.index(), &mut out);
    out
}

/// All three Design-B semantic diagnostics, computed in one pass that builds the
/// shared [`OwnedEffectData`] once. Used by the batch driver and the LSP so
/// effect / stores / purity stay in lockstep across both.
#[must_use]
pub fn check_semantics(sem: &Semantics) -> SemanticDiagnostics {
    let mut diags = SemanticDiagnostics::default();
    let Some(state) = sem.state.as_ref() else {
        return diags;
    };
    let data = OwnedEffectData::build(sem, state);
    let index = data.index();
    run_effect_checks(sem, &index, &mut diags.effects);
    run_purity_checks(sem, &index, &mut diags.purity);
    diags.stores = check_stores_semantic(sem);
    diags
}

/// Bundle of the Design-B semantic diagnostics returned by [`check_semantics`].
#[derive(Default)]
pub struct SemanticDiagnostics {
    pub effects: Vec<EffectError>,
    pub stores: Vec<StoresError>,
    pub purity: Vec<DefaultPurityError>,
}

impl SemanticDiagnostics {
    /// Whether any check produced a violation.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.effects.is_empty() && self.stores.is_empty() && self.purity.is_empty()
    }
}

/// Walk every user-authored function / method / trait method, appending effect
/// violations. Shared by [`check_effects_semantic`] and [`check_semantics`].
fn run_effect_checks(sem: &Semantics, index: &EffectIndex, out: &mut Vec<EffectError>) {
    for (src, module) in &sem.modules {
        if !crate::elaborator::liveness::is_user_authored(src) {
            continue;
        }
        for item in &module.items {
            match item {
                Item::Function(func) => {
                    check_function_effects_sem(sem, src, func, index, out);
                }
                Item::Impl(impl_block) => {
                    for method in &impl_block.methods {
                        check_function_effects_sem(sem, src, method, index, out);
                    }
                }
                Item::Trait(trait_decl) => {
                    for method in &trait_decl.methods {
                        check_function_effects_sem(sem, src, method, index, out);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Owns the cross-module effect maps so multiple checks (effects, default
/// purity) can borrow a single [`EffectIndex`] view over them. Assembled once
/// from [`Semantics`] + [`AnnotateState`].
struct OwnedEffectData {
    fn_effects: IndexMap<crate::ast::AstId, Vec<EffectRef>>,
    fn_params: IndexMap<crate::ast::AstId, Vec<TypeId>>,
    mangled_index: IndexMap<(ModuleSource, String), Vec<EffectRef>>,
    mangled_params: IndexMap<(ModuleSource, String), Vec<TypeId>>,
    resource_names: IndexSet<(ModuleSource, String)>,
    struct_fields: IndexMap<(ModuleSource, String), Vec<TypeId>>,
    variant_payloads: IndexMap<(ModuleSource, String), Vec<TypeId>>,
    closure: IndexMap<EffectRef, IndexSet<EffectRef>>,
    effect_by_name: IndexMap<String, EffectRef>,
    interface_meta: IndexMap<String, (ModuleSource, Option<String>)>,
    effect_by_cm_fq: IndexMap<String, EffectRef>,
}

impl OwnedEffectData {
    fn build(sem: &Semantics, state: &crate::elaborator::orchestration::AnnotateState) -> Self {
        // Resolved effect lists, indexed two ways: by the function's
        // declaration key (free calls resolve through `references`) and by
        // `(module, mangled name)` (method dispatch carries a `FunctionRef`).
        let mut fn_effects: IndexMap<crate::ast::AstId, Vec<EffectRef>> = IndexMap::default();
        let mut fn_params: IndexMap<crate::ast::AstId, Vec<TypeId>> = IndexMap::default();
        let mut mangled_index: IndexMap<(ModuleSource, String), Vec<EffectRef>> =
            IndexMap::default();
        let mut mangled_params: IndexMap<(ModuleSource, String), Vec<TypeId>> = IndexMap::default();
        for (src, module_sem) in &state.module_semantics {
            let types = &module_sem.types;
            for (key, effects) in &types.function_effects {
                fn_effects.insert(*key, effects.clone());
            }
            for (key, params) in &types.fn_param_types {
                fn_params.insert(*key, params.clone());
            }
            for (key, names) in &types.method_names {
                if let Some(effects) = types.function_effects.get(key) {
                    mangled_index.insert((src.clone(), names.mangled.clone()), effects.clone());
                }
                if let Some(params) = types.fn_param_types.get(key) {
                    mangled_params.insert((src.clone(), names.mangled.clone()), params.clone());
                }
            }
        }

        let mut resource_names: IndexSet<(ModuleSource, String)> = IndexSet::default();
        // `(module, struct name)` → field type ids, so resource detection
        // follows resources nested in struct fields of a signature / op type.
        let mut struct_fields: IndexMap<(ModuleSource, String), Vec<TypeId>> = IndexMap::default();
        for (src, module) in &sem.modules {
            let annotations = state.module_semantics.get(src).map(|m| &m.types);
            for item in &module.items {
                match item {
                    Item::Resource(resource) => {
                        resource_names.insert((src.clone(), resource.name.clone()));
                    }
                    Item::Struct(struct_decl) => {
                        if let Some(field_types) =
                            annotations.and_then(|ann| ann.struct_field_types.get(&struct_decl.id))
                        {
                            struct_fields.insert(
                                (src.clone(), struct_decl.name.clone()),
                                field_types.clone(),
                            );
                        }
                    }
                    _ => {}
                }
            }
        }

        // `(module, variant name)` → case payload type ids, so resource
        // detection descends into variant case payloads.
        let mut variant_payloads: IndexMap<(ModuleSource, String), Vec<TypeId>> =
            IndexMap::default();
        for (module, variants) in state.tysys.all_variant_cases.iter() {
            for (variant_name, info) in variants {
                variant_payloads.insert(
                    (module.clone(), variant_name.clone()),
                    info.cases.iter().map(|case| case.payload).collect(),
                );
            }
        }

        // Effect / resource propagation closure: holding effect `E` admits the
        // resources `E`'s operations reference (e.g. `Stdout` → `Stream`).
        let closure = build_propagation_closure_sem(sem, state, &struct_fields, &variant_payloads);

        // Name → resolved `EffectRef` for every declared effect / resource,
        // used to resolve `#[benign(E)]` names and to canonicalise.
        let mut effect_by_name: IndexMap<String, EffectRef> = IndexMap::default();
        for key in closure.keys() {
            if let EffectRef::Concrete { name, .. } = key {
                effect_by_name
                    .entry(name.clone())
                    .or_insert_with(|| key.clone());
            }
        }

        // `interface_meta` resolves a `Local(E)` callee to (declaring module,
        // `#[cm]` FQ); `effect_by_cm_fq` maps a CM FQ back to the effect it
        // declares, restricted to closure keys so a host-leaf import resolves to
        // an effect while a type-only interface (`wasi:cli/types`) resolves to
        // nothing.
        let mut interface_meta: IndexMap<String, (ModuleSource, Option<String>)> =
            IndexMap::default();
        let mut effect_by_cm_fq: IndexMap<String, EffectRef> = IndexMap::default();
        for (src, module) in &sem.modules {
            for item in &module.items {
                let Item::Interface(decl) = item else {
                    continue;
                };
                let cm_fq = decl
                    .attrs
                    .iter()
                    .find_map(|a| a.as_cm_import())
                    .map(crate::ast::CmImport::interface_path);
                interface_meta
                    .entry(decl.name.clone())
                    .or_insert_with(|| (src.clone(), cm_fq.clone()));
                let key = EffectRef::Concrete {
                    name: decl.name.clone(),
                    module_source: src.clone(),
                };
                if closure.contains_key(&key)
                    && let Some(fq) = cm_fq
                {
                    effect_by_cm_fq.entry(fq).or_insert(key);
                }
            }
        }

        Self {
            fn_effects,
            fn_params,
            mangled_index,
            mangled_params,
            resource_names,
            struct_fields,
            variant_payloads,
            closure,
            effect_by_name,
            interface_meta,
            effect_by_cm_fq,
        }
    }

    fn index(&self) -> EffectIndex<'_> {
        EffectIndex {
            fn_effects: &self.fn_effects,
            fn_params: &self.fn_params,
            mangled_index: &self.mangled_index,
            mangled_params: &self.mangled_params,
            resource_names: &self.resource_names,
            struct_fields: &self.struct_fields,
            variant_payloads: &self.variant_payloads,
            closure: &self.closure,
            effect_by_name: &self.effect_by_name,
            interface_meta: &self.interface_meta,
            effect_by_cm_fq: &self.effect_by_cm_fq,
        }
    }
}

/// The cross-module effect data the body walk consults, assembled once.
struct EffectIndex<'a> {
    /// Declaration key → resolved effects (free calls resolve via `references`).
    fn_effects: &'a IndexMap<crate::ast::AstId, Vec<EffectRef>>,
    /// Declaration key → parameter type ids (for effect-parameter resolution).
    fn_params: &'a IndexMap<crate::ast::AstId, Vec<TypeId>>,
    /// `(module, mangled name)` → effects (method / static dispatch).
    mangled_index: &'a IndexMap<(ModuleSource, String), Vec<EffectRef>>,
    /// `(module, mangled name)` → parameter type ids.
    mangled_params: &'a IndexMap<(ModuleSource, String), Vec<TypeId>>,
    /// Declared resources, for resource injection and effect classification.
    resource_names: &'a IndexSet<(ModuleSource, String)>,
    /// `(module, struct name)` → field type ids, for nested-resource detection.
    struct_fields: &'a IndexMap<(ModuleSource, String), Vec<TypeId>>,
    /// `(module, variant name)` → case payload type ids.
    variant_payloads: &'a IndexMap<(ModuleSource, String), Vec<TypeId>>,
    /// Effect → implied resources propagation closure.
    closure: &'a IndexMap<EffectRef, IndexSet<EffectRef>>,
    /// Declared effect / resource name → resolved `EffectRef` (`#[benign]`).
    effect_by_name: &'a IndexMap<String, EffectRef>,
    /// Declared interface name → (declaring module, its `#[cm]` FQ), for
    /// resolving a direct `E::op()` callee to its effect and FQ.
    interface_meta: &'a IndexMap<String, (ModuleSource, Option<String>)>,
    /// CM interface FQ → the effect it declares, for reconstructing a
    /// component's host-leaf imports into effects.
    effect_by_cm_fq: &'a IndexMap<String, EffectRef>,
}

fn check_function_effects_sem(
    sem: &Semantics,
    module: &ModuleSource,
    func: &Function,
    index: &EffectIndex,
    out: &mut Vec<EffectError>,
) {
    let Some(body) = &func.body else {
        return;
    };
    // `#[ambient]` bypasses the effect system; test helpers implicitly hold
    // every effect.
    if func.attrs.iter().any(|attr| attr.name == "ambient") || func.name.starts_with("__test_") {
        return;
    }
    let caller_key = func.id;

    // Per-module annotations carry the dispatch facts and signature types that
    // have no flattened `Semantics` mirror (static-method dispatch,
    // param / return type ids).
    let annotations = sem
        .state
        .as_ref()
        .and_then(|state| state.module_semantics.get(module))
        .map(|module_sem| &module_sem.types);

    // Declared effects, plus resources that appear in the signature so a
    // `fn f(s: Stream<u8>)` need not repeat `with Stream`.
    let mut current: IndexSet<EffectRef> = index
        .fn_effects
        .get(&caller_key)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect();
    if let Some(ann) = annotations {
        add_signature_resources(
            ann,
            caller_key,
            &sem.types,
            index.struct_fields,
            index.variant_payloads,
            &mut current,
        );
    }
    // `#[benign(E)]` admits `E` in the body without a `with E` clause.
    for name in benign_effect_names(&func.attrs) {
        if let Some(effect) = index.effect_by_name.get(&name) {
            current.insert(effect.clone());
        }
    }
    // Canonicalise before expanding so the closure keys (built from the
    // declarations, i.e. canonical) match, then expand: a function holding
    // `Stdout` may call operations that internally need `Stream`, etc.
    let current: IndexSet<EffectRef> = current
        .iter()
        .map(|effect| canonicalize_effect(effect, index.effect_by_name))
        .collect();
    let current = expand_through_closure(&current, index.closure);

    // Parameter name → type id (aligned with the recorded signature types),
    // for resolving indirect calls through function-typed parameters.
    let mut param_types: IndexMap<String, TypeId> = IndexMap::default();
    if let Some(type_ids) = annotations.and_then(|ann| ann.fn_param_types.get(&caller_key)) {
        for (param, type_id) in func.params.iter().zip(type_ids.iter()) {
            param_types.insert(param.name.clone(), *type_id);
        }
    }

    let mut walker = SemEffectWalker {
        sem,
        annotations,
        index,
        current,
        param_types,
        module: module.source_path(),
        out,
    };
    ast::walk_block(&mut walker, body);
}

/// Build the effect / resource propagation closure from `Semantics`: for each
/// effect or resource declaration, the resources its operations' parameter and
/// return types reference, transitively closed. Reads the resolved operation
/// signatures from the `effect_ops` facts; `struct_fields` / `variant_payloads`
/// let resource detection descend into struct fields and variant payloads of an
/// operation's types.
fn build_propagation_closure_sem(
    sem: &Semantics,
    state: &crate::elaborator::orchestration::AnnotateState,
    struct_fields: &IndexMap<(ModuleSource, String), Vec<TypeId>>,
    variant_payloads: &IndexMap<(ModuleSource, String), Vec<TypeId>>,
) -> IndexMap<EffectRef, IndexSet<EffectRef>> {
    let type_table = &sem.types;
    let mut direct: IndexMap<EffectRef, IndexSet<EffectRef>> = IndexMap::default();

    for (src, module) in &sem.modules {
        let Some(annotations) = state.module_semantics.get(src).map(|m| &m.types) else {
            continue;
        };
        for item in &module.items {
            let (decl_id, decl_name, is_resource) = match item {
                Item::Interface(decl) => (decl.id, &decl.name, false),
                Item::Resource(decl) => (decl.id, &decl.name, true),
                _ => continue,
            };
            let Some(ops) = annotations.effect_ops.get(&decl_id) else {
                continue;
            };
            let mut refs: IndexSet<EffectRef> = IndexSet::default();
            for op in ops {
                for param in &op.params {
                    collect_resource_refs(
                        param.type_id,
                        type_table,
                        struct_fields,
                        variant_payloads,
                        &mut refs,
                        &mut TypeSet::default(),
                    );
                }
                collect_resource_refs(
                    op.return_type,
                    type_table,
                    struct_fields,
                    variant_payloads,
                    &mut refs,
                    &mut TypeSet::default(),
                );
            }
            let key = EffectRef::Concrete {
                name: decl_name.clone(),
                module_source: src.clone(),
            };
            if is_resource {
                // Holding `with R` already implies `R` — drop the self-reference.
                refs.shift_remove(&key);
            }
            let entry = direct.entry(key).or_default();
            for r in refs {
                entry.insert(r);
            }
        }
    }

    // Transitive closure to a fixpoint.
    loop {
        let mut changed = false;
        let keys: Vec<EffectRef> = direct.keys().cloned().collect();
        for key in &keys {
            let cur = direct.get(key).cloned().unwrap_or_default();
            let mut merged = cur.clone();
            for eff in &cur {
                if matches!(eff, EffectRef::Concrete { .. })
                    && let Some(child) = direct.get(eff).cloned()
                {
                    for e in &child {
                        if merged.insert(e.clone()) {
                            changed = true;
                        }
                    }
                }
            }
            if merged.len() != cur.len() {
                direct.insert(key.clone(), merged);
            }
        }
        if !changed {
            break;
        }
    }
    direct
}

/// Canonicalise an effect by name through the declaration index. The raw
/// `EffectRef::Concrete.module_source` recorded in `function_effects` reflects
/// the recording module's import perspective, so two references to the same
/// effect can carry different module sources (user entry vs `wasi:cli`). The
/// declaration index (built from the propagation-closure keys) holds one
/// canonical `EffectRef` per name; mapping through it makes cross-module
/// effect comparison and closure lookups consistent. Effect parameters and
/// names without a declaration are returned unchanged.
fn canonicalize_effect(
    effect: &EffectRef,
    effect_by_name: &IndexMap<String, EffectRef>,
) -> EffectRef {
    match effect {
        EffectRef::Concrete { name, .. } => effect_by_name
            .get(name)
            .cloned()
            .unwrap_or_else(|| effect.clone()),
        EffectRef::Param { .. } => effect.clone(),
    }
}

/// Expand an effect set through the propagation closure.
fn expand_through_closure(
    effects: &IndexSet<EffectRef>,
    closure: &IndexMap<EffectRef, IndexSet<EffectRef>>,
) -> IndexSet<EffectRef> {
    let mut out: IndexSet<EffectRef> = IndexSet::default();
    for effect in effects {
        out.insert(effect.clone());
        if matches!(effect, EffectRef::Concrete { .. })
            && let Some(extra) = closure.get(effect)
        {
            for e in extra {
                out.insert(e.clone());
            }
        }
    }
    out
}

/// Union into `out` the resources that appear in a function's signature —
/// parameter types, the return type, and the async task-return type — so a
/// signature that already exposes a resource does not also require an explicit
/// `with R`.
///
/// Resources nested inside struct fields and variant case payloads are
/// followed via `struct_fields` / `variant_payloads`; direct and
/// container-nested resources (`Option<R>`, `List<R>`, `&R`, `fn() -> R`) are
/// too.
fn add_signature_resources(
    annotations: &crate::elaborator::sem::types::TypeAnnotations,
    fn_key: crate::ast::AstId,
    type_table: &TypeTable,
    struct_fields: &IndexMap<(ModuleSource, String), Vec<TypeId>>,
    variant_payloads: &IndexMap<(ModuleSource, String), Vec<TypeId>>,
    out: &mut IndexSet<EffectRef>,
) {
    let mut visited = TypeSet::default();
    for &type_id in annotations
        .fn_param_types
        .get(&fn_key)
        .into_iter()
        .flatten()
    {
        collect_resource_refs(
            type_id,
            type_table,
            struct_fields,
            variant_payloads,
            out,
            &mut visited,
        );
    }
    if let Some(&return_type) = annotations.fn_return_types.get(&fn_key) {
        collect_resource_refs(
            return_type,
            type_table,
            struct_fields,
            variant_payloads,
            out,
            &mut visited,
        );
    }
    if let Some(&task_return) = annotations.function_task_returns.get(&fn_key) {
        collect_resource_refs(
            task_return,
            type_table,
            struct_fields,
            variant_payloads,
            out,
            &mut visited,
        );
    }
}

/// `#[benign(E, F)]` effect names declared on a function.
fn benign_effect_names(attrs: &[crate::ast::Attribute]) -> Vec<String> {
    attrs
        .iter()
        .filter(|attr| attr.name == "benign")
        .flat_map(|attr| attr.args.iter().map(crate::ast::AttrArg::as_str))
        .map(str::to_string)
        .collect()
}

/// Best-effort display name for a call's callee, for the diagnostic message.
fn callee_name(callee: &Expr) -> &str {
    match callee {
        Expr::Ident(ident) => &ident.name,
        _ => "(call)",
    }
}

/// Walks a function body, checking that each call's required effects are held.
struct SemEffectWalker<'a> {
    sem: &'a Semantics,
    annotations: Option<&'a crate::elaborator::sem::types::TypeAnnotations>,
    index: &'a EffectIndex<'a>,
    /// Effects available at the current point: the function's declared +
    /// signature + benign + propagated set, plus any effects granted by an
    /// enclosing `with H => … do { … }` handler scope (pushed / popped as the
    /// walk enters / leaves the do-block body).
    current: IndexSet<EffectRef>,
    /// This function's parameter name → type id, for resolving the callee of an
    /// indirect call through a function-typed parameter (which leaves no
    /// `references` edge or recorded expression type at the call site).
    param_types: IndexMap<String, TypeId>,
    module: String,
    out: &'a mut Vec<EffectError>,
}

impl EffectIndex<'_> {
    /// Effects a method dispatch requires: the callee's declared effects plus,
    /// for a direct (non-trait) method on a `resource`, the resource effect.
    fn method_effects(&self, func_ref: &FunctionRef) -> Vec<EffectRef> {
        let mut effects = self
            .mangled_index
            .get(&(func_ref.module_source.clone(), func_ref.name.clone()))
            .cloned()
            .unwrap_or_default();
        if let Some(method_info) = &func_ref.method_info
            && method_info.trait_name.is_none()
        {
            let resource_key = (
                func_ref.module_source.clone(),
                method_info.base_struct_name.clone(),
            );
            if self.resource_names.contains(&resource_key) {
                let resource_effect = EffectRef::Concrete {
                    name: method_info.base_struct_name.clone(),
                    module_source: func_ref.module_source.clone(),
                };
                if !effects.contains(&resource_effect) {
                    effects.push(resource_effect);
                }
            }
        }
        effects
    }

    /// Parameter type ids for a method / static dispatch target.
    fn method_param_types(&self, func_ref: &FunctionRef) -> Vec<TypeId> {
        self.mangled_params
            .get(&(func_ref.module_source.clone(), func_ref.name.clone()))
            .cloned()
            .unwrap_or_default()
    }
}

impl SemEffectWalker<'_> {
    fn method_effects(&self, func_ref: &FunctionRef) -> Vec<EffectRef> {
        self.index.method_effects(func_ref)
    }

    /// The effects a direct `E::op()` call requires. A bare interface-operation
    /// call resolves to a `ModuleSource::Local(E)` free-call `func_ref` that
    /// carries no declared effects, so without this it would slip through the
    /// checker unflagged. For a host/WASI effect the requirement is the effect
    /// itself; for a CM-component-imported interface it is the reconstructed
    /// host-leaf effect set — empty for a purely-computational component, so its
    /// operations need no `with`. Returns empty for a non-effect-op callee.
    fn effect_op_requirement(&self, func_ref: &FunctionRef) -> Vec<EffectRef> {
        if func_ref.method_info.is_some() {
            return Vec::new();
        }
        if !matches!(func_ref.module_source, ModuleSource::Local { .. }) {
            return Vec::new();
        }
        let interface = func_ref.module_source.to_string();
        let Some((decl_module, cm_fq)) = self.index.interface_meta.get(&interface) else {
            return Vec::new();
        };
        // Only a host-backed effect (`#[cm]`) is a capability the caller must
        // hold. A user-defined effect is resolved by the handler machinery, so
        // its operations — including a handler's self-delegation — are not a
        // direct-op requirement.
        let Some(fq) = cm_fq else {
            return Vec::new();
        };
        if let Some(registry) = self.sem.cm_interface_registry()
            && registry.is_component_interface(fq)
        {
            // Composition-relative: the imported interface is composed away, so
            // its operations demand the dependency's own host-leaf capabilities
            // (empty for a purely-computational component).
            return registry
                .host_leaf_imports_for(fq)
                .iter()
                .filter_map(|leaf| self.index.effect_by_cm_fq.get(leaf).cloned())
                .collect();
        }
        vec![EffectRef::Concrete {
            name: interface,
            module_source: decl_module.clone(),
        }]
    }

    fn binding_granted_effects(
        &self,
        binding: &crate::ast::EffectHandlerBinding,
    ) -> Vec<EffectRef> {
        if let Some(facts) = self
            .annotations
            .and_then(|a| a.handler_bindings.get(&binding.id))
        {
            return facts
                .effects
                .iter()
                .filter_map(|entry| {
                    let resolved = self.index.effect_by_name.get(&entry.name).cloned();
                    debug_assert!(
                        resolved.is_some(),
                        "granted effect '{}' from handler_bindings facts is absent from effect_by_name",
                        entry.name
                    );
                    resolved
                })
                .collect();
        }
        binding
            .effect
            .as_ref()
            .and_then(|ty| match ty {
                crate::ast::Type::Named(named) => {
                    self.index.effect_by_name.get(&named.name).cloned()
                }
                _ => None,
            })
            .into_iter()
            .collect()
    }

    /// Resolve `EffectRef::Param` effects to concrete effects by matching the
    /// callee's function-typed parameters against the actual argument types.
    /// `is_method` drops the leading `self` parameter so params line up with
    /// `args`.
    fn resolve_effect_params(
        &self,
        callee_effects: &[EffectRef],
        param_types: &[TypeId],
        is_method: bool,
        args: &[Expr],
    ) -> Vec<EffectRef> {
        let param_names: IndexSet<String> = callee_effects
            .iter()
            .filter_map(|e| match e {
                EffectRef::Param { name } => Some(name.clone()),
                EffectRef::Concrete { .. } => None,
            })
            .collect();
        if param_names.is_empty() {
            return callee_effects.to_vec();
        }
        let mut concrete: IndexMap<String, IndexSet<EffectRef>> = param_names
            .iter()
            .map(|n| (n.clone(), IndexSet::default()))
            .collect();
        let type_table = &self.sem.types;
        let skip = usize::from(is_method && !param_types.is_empty());
        for (param_type, arg) in param_types.iter().skip(skip).zip(args.iter()) {
            let ResolvedType::Function {
                effects: formal, ..
            } = type_table.get(*param_type)
            else {
                continue;
            };
            if !formal
                .iter()
                .any(|e| e.is_param() && param_names.contains(e.name()))
            {
                continue;
            }
            let Some(arg_type) = self.sem.expression_types.get(&arg.id()).copied() else {
                continue;
            };
            let ResolvedType::Function {
                effects: actual, ..
            } = type_table.get(arg_type)
            else {
                continue;
            };
            for formal_effect in formal {
                if let EffectRef::Param { name } = formal_effect
                    && let Some(set) = concrete.get_mut(name)
                {
                    for a in actual {
                        set.insert(a.clone());
                    }
                }
            }
        }
        let mut resolved = Vec::new();
        for effect in callee_effects {
            match effect {
                EffectRef::Param { name } => {
                    if let Some(set) = concrete.get(name) {
                        for c in expand_through_closure(set, self.index.closure) {
                            resolved.push(c);
                        }
                    }
                }
                EffectRef::Concrete { .. } => resolved.push(effect.clone()),
            }
        }
        resolved
    }

    fn report_missing(&mut self, effects: &[EffectRef], callee: &str, span: Span) {
        for effect in effects {
            // Canonicalise: `EffectRef::Concrete.module_source` reflects the
            // recording module's import perspective (a user `with Stdout`
            // records `Stdout` against the entry module, while stdlib records
            // it against `wasi:cli`), so compare through the declaration's
            // canonical form rather than by raw `module_source`.
            let effect = canonicalize_effect(effect, self.index.effect_by_name);
            // Any `Param` left after resolution did not bind to a concrete
            // effect; skip it rather than report a spurious miss.
            if effect.is_param() || self.current.contains(&effect) {
                continue;
            }
            let effect = &effect;
            let kind = match effect {
                EffectRef::Concrete {
                    name,
                    module_source,
                } if self
                    .index
                    .resource_names
                    .contains(&(module_source.clone(), name.clone())) =>
                {
                    EffectKind::Resource
                }
                _ => EffectKind::Effect,
            };
            self.out.push(EffectError {
                callee: callee.to_string(),
                missing_effect: effect.name().to_string(),
                kind,
                span,
                module: self.module.clone(),
            });
        }
    }
}

impl AstVisitor for SemEffectWalker<'_> {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        // `for let v of iterable { … }` desugars to synthetic `.into_iter()` /
        // `.next()` calls that have no source call id, so they record no
        // `method_dispatch` fact for `visit_expr` to consult. Check their
        // declared effects here from the recorded `for_of_iterator` fact.
        if let Stmt::ForOf(for_of) = stmt
            && let Some(info) = self
                .annotations
                .and_then(|ann| ann.for_of_iterator.get(&for_of.id))
        {
            for func_ref in [&info.into_iter, &info.next] {
                let effects = self.index.method_effects(func_ref);
                let callee = func_ref
                    .method_info
                    .as_ref()
                    .map_or(func_ref.name.as_str(), |m| m.method_name.as_str());
                self.report_missing(&effects, callee, for_of.span);
            }
        }
        ast::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Call(call) => {
                // Free calls resolve through `references` on the callee
                // identifier. `Type::method(...)` / `Self::method(...)` parse as
                // a `Call` with a path callee whose identifier has no free-
                // function reference; they resolve through
                // `static_method_dispatch` keyed by the call id. (Free
                // functions also appear in `static_method_dispatch`, so try
                // `references` first — it is the authoritative free-call edge.)
                let free = if let Expr::Ident(ident) = &call.callee {
                    self.sem.references.get(&ident.id).and_then(|def| {
                        self.index
                            .fn_effects
                            .get(def)
                            .map(|effects| (*def, effects.clone(), ident.name.clone()))
                    })
                } else {
                    None
                };
                if let Some((def, effects, name)) = free {
                    let params = self.index.fn_params.get(&def).cloned().unwrap_or_default();
                    let resolved = self.resolve_effect_params(&effects, &params, false, &call.args);
                    self.report_missing(&resolved, &name, call.span);
                } else if let Some(func_ref) = self
                    .annotations
                    .and_then(|ann| ann.static_method_dispatch.get(&call.id))
                    .map(|dispatch| dispatch.function_ref.clone())
                {
                    let mut effects = self.method_effects(&func_ref);
                    effects.extend(self.effect_op_requirement(&func_ref));
                    let params = self.method_param_types(&func_ref);
                    let is_method = func_ref.method_info.is_some();
                    let resolved =
                        self.resolve_effect_params(&effects, &params, is_method, &call.args);
                    self.report_missing(&resolved, callee_name(&call.callee), call.span);
                } else if let Some(callee_type) = self.indirect_callee_type(call) {
                    // Indirect call: the callee is a function-typed value (a
                    // closure or `fn(...)` parameter). Its type carries the
                    // effects it performs when invoked.
                    if let ResolvedType::Function { effects, .. } = self.sem.types.get(callee_type)
                    {
                        let effects = effects.clone();
                        self.report_missing(&effects, "(indirect call)", call.span);
                    }
                }
            }
            Expr::MethodCall(method_call) => {
                let call_key = method_call.id;
                if let Some(dispatch) = self.sem.method_dispatch.get(&call_key) {
                    let func_ref = dispatch.function_ref.clone();
                    let mut effects = self.method_effects(&func_ref);
                    effects.extend(self.effect_op_requirement(&func_ref));
                    let params = self.method_param_types(&func_ref);
                    let resolved =
                        self.resolve_effect_params(&effects, &params, true, &method_call.args);
                    self.report_missing(&resolved, &method_call.method, method_call.span);
                }
            }
            Expr::StaticMethodCall(static_call) => {
                let call_key = static_call.id;
                if let Some(func_ref) = self
                    .annotations
                    .and_then(|ann| ann.static_method_dispatch.get(&call_key))
                    .map(|dispatch| dispatch.function_ref.clone())
                {
                    let mut effects = self.method_effects(&func_ref);
                    effects.extend(self.effect_op_requirement(&func_ref));
                    let params = self.method_param_types(&func_ref);
                    let is_method = func_ref.method_info.is_some();
                    let resolved =
                        self.resolve_effect_params(&effects, &params, is_method, &static_call.args);
                    self.report_missing(&resolved, &static_call.method, static_call.span);
                }
            }
            Expr::WithHandler(with_handler) => {
                // `with H => h do { body }` installs handlers, granting each
                // handled effect to the body (calls inside it — directly or via
                // helpers — observe the installed handler). The handler
                // expressions themselves run outside the grant.
                for binding in &with_handler.handlers {
                    ast::walk_expr(self, &binding.handler);
                }
                let granted: Vec<EffectRef> = with_handler
                    .handlers
                    .iter()
                    .flat_map(|binding| self.binding_granted_effects(binding))
                    .collect();
                let added: Vec<EffectRef> = granted
                    .into_iter()
                    .filter(|effect| self.current.insert(effect.clone()))
                    .collect();
                ast::walk_block(self, &with_handler.body);
                for effect in added {
                    self.current.shift_remove(&effect);
                }
                return;
            }
            _ => {}
        }
        ast::walk_expr(self, expr);
    }
}

impl SemEffectWalker<'_> {
    fn method_param_types(&self, func_ref: &FunctionRef) -> Vec<TypeId> {
        self.index.method_param_types(func_ref)
    }

    /// Type of an indirect call's callee. When the callee is an identifier
    /// bound to a local or parameter, its type lives in `local_types` keyed by
    /// the binding's def (resolved through `references`); function-typed
    /// parameters are recorded there, not in `expression_types`. Other callee
    /// shapes fall back to the expression's recorded type.
    fn indirect_callee_type(&self, call: &crate::ast::CallExpr) -> Option<TypeId> {
        if let Expr::Ident(ident) = &call.callee {
            // A function-typed parameter callee leaves no `references` edge or
            // recorded expression type at the call, so resolve it against the
            // enclosing function's parameter types by name first.
            if let Some(type_id) = self.param_types.get(&ident.name) {
                return Some(*type_id);
            }
            if let Some(type_id) = self
                .sem
                .references
                .get(&ident.id)
                .and_then(|def| self.sem.local_types.get(def))
            {
                return Some(*type_id);
            }
        }
        self.sem.expression_types.get(&call.callee.id()).copied()
    }
}

// ---------------------------------------------------------------------------
// Semantics-based stores checking (Design B)
// ---------------------------------------------------------------------------

/// Stores checking over [`Semantics`] — the Design B reference-escape checker.
///
/// Two soundness obligations, both enforced before lowering so the functor
/// optimization can trust a functor slot's declared `stores`:
///
/// 1. **Named-function / method honesty.** Every function whose reference
///    parameter *escapes* (is returned, placed in an aggregate / global, stored
///    through a reference, or forwarded to a callee that stores that position)
///    must declare `stores[param]`.
/// 2. **Closure coercion.** A closure type is always `stores=[]` (never
///    inferred) and functor coercion ignores `stores`, so a closure that stores
///    one of its own reference parameters would slip through type-checking. Each
///    closure body is analysed with allowance `[]` — no closure parameter may
///    escape.
///
/// The escape analysis mirrors `lower/plan/value_copy/stores.rs` but stays
/// *precise* rather than the optimizer's conservative over-approximation: a
/// value only carries a parameter reference when reference-flow reaches it, and
/// a storing call *folds* its stored argument into the call result (gated by
/// whether the result type can hold a reference) rather than being an
/// unconditional escape — so a locally-consumed borrow (`self.as_bytes()` fed
/// to a loop) is not flagged, while a genuine persistence (`return g(p)`,
/// `GLOBAL = p`, `Wrapper { r: p }`) is.
#[must_use]
pub fn check_stores_semantic(sem: &Semantics) -> Vec<StoresError> {
    let mut out = Vec::new();
    let Some(state) = sem.state.as_ref() else {
        return out;
    };

    let oracle = StoresOracle::build(sem, state);
    let tyctx = TypeRefCtx::build(sem, state);

    for (src, module) in &sem.modules {
        if !crate::elaborator::liveness::is_user_authored(src) {
            continue;
        }
        let Some(annotations) = state.module_semantics.get(src).map(|m| &m.types) else {
            continue;
        };
        let ctx = StoresCtx {
            sem,
            annotations,
            oracle: &oracle,
            tyctx: &tyctx,
            module: src.source_path(),
        };
        for item in &module.items {
            match item {
                Item::Function(func) => ctx.check_function(func, &mut out),
                Item::Impl(impl_block) => {
                    for method in &impl_block.methods {
                        ctx.check_function(method, &mut out);
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Declared `stores[...]` positions of every function / method in the program,
/// so a call site can look up the callee's stored parameter positions modularly
/// (each function's own declaration is independently verified by obligation #1).
struct StoresOracle {
    /// Function decl [`AstId`] → declared stored parameter positions.
    fn_stores: IndexMap<AstId, Vec<u32>>,
    /// `(module, mangled name)` → declared stored positions (method dispatch).
    mangled_stores: IndexMap<(ModuleSource, String), Vec<u32>>,
}

impl StoresOracle {
    fn build(sem: &Semantics, state: &crate::elaborator::orchestration::AnnotateState) -> Self {
        let mut fn_stores: IndexMap<AstId, Vec<u32>> = IndexMap::default();
        let record = |func: &Function, fn_stores: &mut IndexMap<AstId, Vec<u32>>| {
            let positions: Vec<u32> = func
                .params
                .iter()
                .enumerate()
                .filter(|(_, p)| func.stores.contains(&p.name))
                .map(|(i, _)| u32::try_from(i).unwrap())
                .collect();
            fn_stores.insert(func.id, positions);
        };
        // Every module (including stdlib) so cross-module callees resolve.
        for module in sem.modules.values() {
            for item in &module.items {
                match item {
                    Item::Function(func) => record(func, &mut fn_stores),
                    Item::Impl(impl_block) => {
                        for method in &impl_block.methods {
                            record(method, &mut fn_stores);
                        }
                    }
                    Item::Trait(trait_decl) => {
                        for method in &trait_decl.methods {
                            record(method, &mut fn_stores);
                        }
                    }
                    _ => {}
                }
            }
        }

        let mut mangled_stores: IndexMap<(ModuleSource, String), Vec<u32>> = IndexMap::default();
        for (src, module_sem) in &state.module_semantics {
            for (key, names) in &module_sem.types.method_names {
                if let Some(positions) = fn_stores.get(key) {
                    mangled_stores
                        .insert((src.clone(), names.mangled.clone()), positions.clone());
                }
            }
        }

        Self {
            fn_stores,
            mangled_stores,
        }
    }
}

/// Answers "can a value of this type transitively hold a reference?" — the gate
/// that keeps carrying precise: a value whose type cannot contain a reference
/// (`i32`, `String`, `Unit`) never carries a parameter, so a storing call that
/// returns such a type folds nothing (e.g. `list.push(x)` returning `Unit`).
struct TypeRefCtx {
    struct_fields: IndexMap<(ModuleSource, String), Vec<TypeId>>,
    variant_payloads: IndexMap<(ModuleSource, String), Vec<TypeId>>,
    memo: std::cell::RefCell<IndexMap<TypeId, bool>>,
}

impl TypeRefCtx {
    fn build(sem: &Semantics, state: &crate::elaborator::orchestration::AnnotateState) -> Self {
        let mut struct_fields: IndexMap<(ModuleSource, String), Vec<TypeId>> = IndexMap::default();
        for (src, module) in &sem.modules {
            let annotations = state.module_semantics.get(src).map(|m| &m.types);
            for item in &module.items {
                if let Item::Struct(struct_decl) = item
                    && let Some(field_types) =
                        annotations.and_then(|ann| ann.struct_field_types.get(&struct_decl.id))
                {
                    struct_fields
                        .insert((src.clone(), struct_decl.name.clone()), field_types.clone());
                }
            }
        }
        let mut variant_payloads: IndexMap<(ModuleSource, String), Vec<TypeId>> =
            IndexMap::default();
        for (module, variants) in state.tysys.all_variant_cases.iter() {
            for (variant_name, info) in variants {
                variant_payloads.insert(
                    (module.clone(), variant_name.clone()),
                    info.cases.iter().map(|case| case.payload).collect(),
                );
            }
        }
        Self {
            struct_fields,
            variant_payloads,
            memo: std::cell::RefCell::new(IndexMap::default()),
        }
    }

    fn can_hold_ref(&self, tt: &TypeTable, type_id: TypeId) -> bool {
        if let Some(&b) = self.memo.borrow().get(&type_id) {
            return b;
        }
        let mut visited = TypeSet::default();
        let r = self.walk(tt, type_id, &mut visited);
        self.memo.borrow_mut().insert(type_id, r);
        r
    }

    fn walk(&self, tt: &TypeTable, type_id: TypeId, visited: &mut TypeSet) -> bool {
        if !visited.insert(type_id) {
            return false;
        }
        match tt.get(type_id) {
            ResolvedType::Ref(_) | ResolvedType::MutRef(_) => true,
            ResolvedType::Reactive(t) | ResolvedType::BuiltinArray(t) => self.walk(tt, *t, visited),
            ResolvedType::GenericInstance { type_args, .. }
            | ResolvedType::GenericResource { type_args, .. } => {
                type_args.iter().any(|t| self.walk(tt, *t, visited))
            }
            ResolvedType::Newtype { base_type, .. } => self.walk(tt, *base_type, visited),
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => self
                .struct_fields
                .get(&(module_source.clone(), name.clone()))
                .is_some_and(|fields| fields.iter().any(|t| self.walk(tt, *t, visited))),
            ResolvedType::Variant {
                name,
                module_source,
            } => self
                .variant_payloads
                .get(&(module_source.clone(), name.clone()))
                .is_some_and(|payloads| payloads.iter().any(|t| self.walk(tt, *t, visited))),
            // A functor is a value (`funcref`); storing it needs no `stores`.
            ResolvedType::Function { .. } => false,
            // Pre-mono / unresolved: conservatively assume a reference may appear.
            ResolvedType::TypeParam { .. }
            | ResolvedType::TypePack { .. }
            | ResolvedType::AssocTypeProjection { .. }
            | ResolvedType::Unknown
            | ResolvedType::Error => true,
            ResolvedType::Primitive(_)
            | ResolvedType::Unit
            | ResolvedType::Never
            | ResolvedType::Enum { .. }
            | ResolvedType::Resource { .. }
            | ResolvedType::Flags { .. } => false,
        }
    }
}

/// Per-module context for the escape checks.
struct StoresCtx<'a> {
    sem: &'a Semantics,
    annotations: &'a crate::elaborator::sem::types::TypeAnnotations,
    oracle: &'a StoresOracle,
    tyctx: &'a TypeRefCtx,
    module: String,
}

impl StoresCtx<'_> {
    /// Obligation #1 for one function/method, plus obligation #2 for every
    /// closure literal in its body.
    fn check_function(&self, func: &Function, out: &mut Vec<StoresError>) {
        let Some(body) = &func.body else {
            return;
        };
        // `#[ambient]` bypasses the reference discipline; test helpers are exempt.
        if func.attrs.iter().any(|attr| attr.name == "ambient")
            || func.name.starts_with("__test_")
        {
            return;
        }
        // Default to no param types so a function with none still gets walked —
        // the walk must run to discover closure literals for obligation #2.
        let param_types = self
            .annotations
            .fn_param_types
            .get(&func.id)
            .map_or(&[][..], Vec::as_slice);

        // Seed carries with the reference parameters (only `&T` / `&mut T` can
        // be stored). `allowed` are the declared-`stores` positions.
        let mut carries: IndexMap<AstId, IndexSet<u32>> = IndexMap::default();
        let mut names: IndexMap<u32, String> = IndexMap::default();
        let mut allowed: IndexSet<u32> = IndexSet::default();
        for (i, (param, &type_id)) in func.params.iter().zip(param_types.iter()).enumerate() {
            let pos = u32::try_from(i).unwrap();
            if matches!(
                self.sem.types.get(type_id),
                ResolvedType::Ref(_) | ResolvedType::MutRef(_)
            ) {
                carries.entry(param.id).or_default().insert(pos);
                names.insert(pos, param.name.clone());
            }
            if func.stores.contains(&param.name) {
                allowed.insert(pos);
            }
        }

        // Always walk: obligation #1 sinks fire only for the seeded reference
        // parameters, but the walk also discovers closure literals (checked as
        // their own entity for obligation #2), so it must run even for a
        // function with no reference parameters.
        let mut walker = RefFlow {
            ctx: self,
            carries,
            names,
            allowed,
            is_closure: false,
            out,
            seen: IndexSet::default(),
        };
        ast::walk_block(&mut walker, body);
    }

    /// Obligation #2: a closure may not store any of its reference parameters
    /// (allowance `[]`).
    fn check_closure(&self, closure: &ast::ClosureExpr, out: &mut Vec<StoresError>) {
        let Some(&type_id) = self.sem.expression_types.get(&closure.id) else {
            return;
        };
        let ResolvedType::Function { params, .. } = self.sem.types.get(type_id) else {
            return;
        };
        let mut carries: IndexMap<AstId, IndexSet<u32>> = IndexMap::default();
        let mut names: IndexMap<u32, String> = IndexMap::default();
        for (j, param) in closure.params.iter().enumerate() {
            let pos = u32::try_from(j).unwrap();
            if params.get(j).is_some_and(|&pt| {
                matches!(
                    self.sem.types.get(pt),
                    ResolvedType::Ref(_) | ResolvedType::MutRef(_)
                )
            }) {
                carries.entry(param.id).or_default().insert(pos);
                names.insert(pos, param.name.clone());
            }
        }
        if names.is_empty() {
            return;
        }
        let mut walker = RefFlow {
            ctx: self,
            carries,
            names,
            allowed: IndexSet::default(),
            is_closure: true,
            out,
            seen: IndexSet::default(),
        };
        // The closure body's value is an implicit return sink.
        walker.sink_return(&closure.body);
        walker.visit_expr(&closure.body);
    }
}

/// A resolved call's stored positions and its arguments in callee-position
/// order (position 0 is a method receiver).
struct ResolvedCall<'e> {
    stored: Vec<u32>,
    args: Vec<&'e Expr>,
}

/// Which persistence sink a carried reference reached (selects the message).
enum Sink<'a> {
    Return,
    StructField,
    Tuple,
    Global(&'a str),
    ThroughRef,
    /// Forwarded as an argument to a callee that stores that position — the
    /// reference persists inside the callee, so the caller stores it too.
    Forward,
}

/// Reference-flow escape walker. Shared by both obligations; the seed
/// (`carries` / `names`), the `allowed` allowance, and `is_closure` (wording)
/// distinguish them. Nested closures are not descended into — each is checked
/// as its own entity.
struct RefFlow<'a, 'b> {
    ctx: &'a StoresCtx<'a>,
    /// Binding [`AstId`] → the parameter positions its reference currently carries.
    carries: IndexMap<AstId, IndexSet<u32>>,
    /// Parameter position → name (for diagnostics).
    names: IndexMap<u32, String>,
    /// Positions whose escape is allowed (declared `stores`).
    allowed: IndexSet<u32>,
    is_closure: bool,
    out: &'b mut Vec<StoresError>,
    seen: IndexSet<(usize, String)>,
}

impl RefFlow<'_, '_> {
    fn expr_type(&self, expr: &Expr) -> Option<TypeId> {
        if let Expr::Ident(ident) = expr
            && let Some(def) = self.ctx.sem.references.get(&ident.id)
            && let Some(&ty) = self.ctx.sem.local_types.get(def)
        {
            return Some(ty);
        }
        self.ctx.sem.expression_types.get(&expr.id()).copied()
    }

    /// Parameter positions the reference produced by `expr` carries. A value
    /// whose type cannot hold a reference carries nothing.
    fn carries(&self, expr: &Expr) -> IndexSet<u32> {
        match self.expr_type(expr) {
            Some(ty) if self.ctx.tyctx.can_hold_ref(&self.ctx.sem.types, ty) => {}
            _ => return IndexSet::default(),
        }
        match expr {
            Expr::Ident(ident) => self
                .ctx
                .sem
                .references
                .get(&ident.id)
                .and_then(|def| self.carries.get(def))
                .cloned()
                .unwrap_or_default(),
            // Only `&place` roots at a parameter. Reading a reference-typed
            // field / element / deref (`self.w`, `xs[i]`, `*q`) yields the
            // reference stored there, which points at a *separate* object, not
            // at the container's parameter — so it carries nothing.
            Expr::Unary(u) if matches!(u.op, ast::UnaryOp::Ref | ast::UnaryOp::MutRef) => {
                self.place_roots(&u.expr)
            }
            // A cast preserves reference identity.
            Expr::Cast(c) => self.carries(&c.expr),
            Expr::Call(_) | Expr::MethodCall(_) | Expr::StaticMethodCall(_) => {
                let Some(call) = self.call_stored_args(expr) else {
                    return IndexSet::default();
                };
                let mut acc = IndexSet::default();
                for (pos, arg) in call.args.iter().enumerate() {
                    if call.stored.contains(&u32::try_from(pos).unwrap()) {
                        acc.extend(self.carries(arg));
                    }
                }
                acc
            }
            Expr::StructLiteral(lit) => {
                let mut acc = IndexSet::default();
                for field in &lit.fields {
                    acc.extend(self.carries(&field.value));
                }
                acc
            }
            Expr::TupleLiteral(t) => {
                let mut acc = IndexSet::default();
                for el in &t.elements {
                    acc.extend(self.carries(el));
                }
                acc
            }
            _ => IndexSet::default(),
        }
    }

    /// The parameter positions the *place* operand of `&` is rooted at,
    /// ignoring the place's own type (`&p.field` roots at `p`).
    fn place_roots(&self, place: &Expr) -> IndexSet<u32> {
        match place {
            Expr::Ident(ident) => self
                .ctx
                .sem
                .references
                .get(&ident.id)
                .and_then(|def| self.carries.get(def))
                .cloned()
                .unwrap_or_default(),
            Expr::Unary(u) if u.op == ast::UnaryOp::Deref => self.place_roots(&u.expr),
            Expr::FieldAccess(f) => self.place_roots(&f.expr),
            Expr::Index(i) => self.place_roots(&i.expr),
            Expr::Cast(c) => self.place_roots(&c.expr),
            _ => IndexSet::default(),
        }
    }

    /// `None` when `expr` is not a call. An unresolvable direct callee is
    /// trusted to store nothing; an unresolvable functor callee stores every
    /// position.
    fn call_stored_args<'e>(&self, expr: &'e Expr) -> Option<ResolvedCall<'e>> {
        match expr {
            Expr::Call(call) => {
                let args: Vec<&Expr> = call.args.iter().collect();
                if let Expr::Ident(ident) = &call.callee
                    && let Some(def) = self.ctx.sem.references.get(&ident.id)
                    && let Some(stored) = self.ctx.oracle.fn_stores.get(def)
                {
                    return Some(ResolvedCall {
                        stored: stored.clone(),
                        args,
                    });
                }
                if let Some(func_ref) = self
                    .ctx
                    .annotations
                    .static_method_dispatch
                    .get(&call.id)
                    .map(|d| &d.function_ref)
                {
                    return Some(ResolvedCall {
                        stored: self.mangled_stored(func_ref),
                        args,
                    });
                }
                if let Some(callee_ty) = self.indirect_callee_type(&call.callee) {
                    let stored = match self.ctx.sem.types.get(callee_ty) {
                        ResolvedType::Function { stores, .. } => stores.clone(),
                        _ => (0..u32::try_from(args.len()).unwrap()).collect(),
                    };
                    return Some(ResolvedCall { stored, args });
                }
                Some(ResolvedCall {
                    stored: Vec::new(),
                    args,
                })
            }
            Expr::MethodCall(mc) => {
                let mut args: Vec<&Expr> = vec![&mc.receiver];
                args.extend(mc.args.iter());
                let stored = self
                    .ctx
                    .sem
                    .method_dispatch
                    .get(&mc.id)
                    .map(|d| self.mangled_stored(&d.function_ref))
                    .unwrap_or_default();
                Some(ResolvedCall { stored, args })
            }
            Expr::StaticMethodCall(sc) => {
                let args: Vec<&Expr> = sc.args.iter().collect();
                let stored = self
                    .ctx
                    .annotations
                    .static_method_dispatch
                    .get(&sc.id)
                    .map(|d| self.mangled_stored(&d.function_ref))
                    .unwrap_or_default();
                Some(ResolvedCall { stored, args })
            }
            _ => None,
        }
    }

    fn mangled_stored(&self, func_ref: &FunctionRef) -> Vec<u32> {
        self.ctx
            .oracle
            .mangled_stores
            .get(&(func_ref.module_source.clone(), func_ref.name.clone()))
            .cloned()
            .unwrap_or_default()
    }

    /// Type of an indirect call's callee identifier (a functor local / param).
    fn indirect_callee_type(&self, callee: &Expr) -> Option<TypeId> {
        if let Expr::Ident(ident) = callee
            && let Some(def) = self.ctx.sem.references.get(&ident.id)
            && let Some(&ty) = self.ctx.sem.local_types.get(def)
        {
            return Some(ty);
        }
        self.ctx.sem.expression_types.get(&callee.id()).copied()
    }

    fn sink_value(&mut self, value: &Expr, sink: Sink) {
        let carried = self.carries(value);
        self.mark(&carried, value.span(), &sink);
    }

    /// Forwarding a reference parameter to a callee that stores it makes the
    /// caller store it too. Only when the result cannot hold a reference is the
    /// store provably external (a global, a write through a reference); when it
    /// can, the store may be a managed borrow into the result, already tracked by
    /// `carries`, so this defers. An unresolved (generic) result type is
    /// `can_hold_ref = true` and also defers.
    fn sink_call_args(&mut self, expr: &Expr) {
        let Some(call) = self.call_stored_args(expr) else {
            return;
        };
        if self
            .expr_type(expr)
            .is_some_and(|ty| self.ctx.tyctx.can_hold_ref(&self.ctx.sem.types, ty))
        {
            return;
        }
        for (pos, arg) in call.args.iter().enumerate() {
            if call.stored.contains(&u32::try_from(pos).unwrap()) {
                let carried = self.carries(arg);
                self.mark(&carried, arg.span(), &Sink::Forward);
            }
        }
    }

    /// A `return`/task-return sink. Returning a freshly-taken borrow (`return
    /// &self.field`, `return &xs[i]`) is a plain reference return signalled by
    /// the `&T` return type — it does not persist the parameter the way storing
    /// it in an aggregate / global does, so it needs no `stores`. Returning the
    /// parameter itself, an alias of it, or a value that folded it into an
    /// aggregate is flagged.
    fn sink_return(&mut self, value: &Expr) {
        if matches!(
            value,
            Expr::Unary(u) if matches!(u.op, ast::UnaryOp::Ref | ast::UnaryOp::MutRef)
        ) {
            return;
        }
        self.sink_value(value, Sink::Return);
    }

    fn mark(&mut self, positions: &IndexSet<u32>, span: Span, sink: &Sink) {
        for &pos in positions {
            if self.allowed.contains(&pos) {
                continue;
            }
            let name = self.names.get(&pos).map_or("?", String::as_str);
            let message = if self.is_closure {
                format!(
                    "closure may not store reference parameter '{name}' (closures cannot declare `stores`)"
                )
            } else {
                match sink {
                    Sink::Return => format!(
                        "returning reference parameter '{name}' requires `stores[{name}]` declaration"
                    ),
                    Sink::StructField => format!(
                        "storing reference parameter '{name}' in struct field requires `stores[{name}]` declaration"
                    ),
                    Sink::Tuple => format!(
                        "storing reference parameter '{name}' in a tuple requires `stores[{name}]` declaration"
                    ),
                    Sink::Global(gname) => format!(
                        "storing reference parameter '{name}' in global '{gname}' requires `stores[{name}]` declaration"
                    ),
                    Sink::ThroughRef => format!(
                        "storing reference parameter '{name}' through a reference requires `stores[{name}]` declaration"
                    ),
                    Sink::Forward => format!(
                        "passing reference parameter '{name}' to a function that stores it requires `stores[{name}]` declaration"
                    ),
                }
            };
            if !self.seen.insert((span.start, message.clone())) {
                continue;
            }
            self.out.push(StoresError {
                message,
                span,
                module: self.ctx.module.clone(),
            });
        }
    }

    /// The global's name if this identifier resolves to a module global l-value.
    fn global_name(&self, ident: &ast::IdentExpr) -> Option<String> {
        match self.ctx.annotations.assign_places.get(&ident.id) {
            Some(crate::elaborator::sem::types::AssignPlace::Global { name, .. }) => {
                Some(name.clone())
            }
            _ => None,
        }
    }

    /// Whether an identifier's binding is a reference (`&T` / `&mut T`), so a
    /// write into its projection reaches caller-visible memory.
    fn ident_is_ref(&self, ident: &ast::IdentExpr) -> bool {
        self.ctx
            .sem
            .references
            .get(&ident.id)
            .and_then(|def| self.ctx.sem.local_types.get(def))
            .is_some_and(|&ty| {
                matches!(
                    self.ctx.sem.types.get(ty),
                    ResolvedType::Ref(_) | ResolvedType::MutRef(_)
                )
            })
    }

    /// Reject a named-function reference argument whose declared `stores`
    /// exceeds the functor parameter's declared `stores`.
    fn check_functor_coercion(&mut self, call: &ast::CallExpr) {
        let Some(param_types) = self.ctx.annotations.call_param_types.get(&call.id) else {
            return;
        };
        for (arg, &param_type) in call.args.iter().zip(param_types.iter()) {
            let ResolvedType::Function {
                stores: expected, ..
            } = self.ctx.sem.types.get(param_type)
            else {
                continue;
            };
            // A closure argument is checked as its own entity; only a named
            // function reference (an identifier resolving to a function decl)
            // carries a declared `stores` to compare here.
            let Expr::Ident(ident) = arg else {
                continue;
            };
            let Some(declared) = self
                .ctx
                .sem
                .references
                .get(&ident.id)
                .and_then(|def| self.ctx.oracle.fn_stores.get(def))
            else {
                continue;
            };
            for pos in declared {
                if !expected.contains(pos) {
                    self.out.push(StoresError {
                        message: format!(
                            "function '{}' stores parameter {pos} but is passed where a functor that stores nothing at that position is expected",
                            ident.name
                        ),
                        span: arg.span(),
                        module: self.ctx.module.clone(),
                    });
                }
            }
        }
    }

    /// Deepest identifier of an assignment target place, and whether the path
    /// crossed a dereference (a write through a reference).
    fn place_root<'e>(&self, place: &'e Expr) -> (Option<&'e ast::IdentExpr>, bool) {
        match place {
            Expr::Ident(ident) => (Some(ident), false),
            Expr::Unary(u) if u.op == ast::UnaryOp::Deref => {
                let (root, _) = self.place_root(&u.expr);
                (root, true)
            }
            Expr::FieldAccess(f) => self.place_root(&f.expr),
            Expr::Index(i) => self.place_root(&i.expr),
            Expr::Cast(c) => self.place_root(&c.expr),
            _ => (None, false),
        }
    }
}

impl AstVisitor for RefFlow<'_, '_> {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(let_stmt) => {
                if let Some(value) = &let_stmt.value {
                    let carried = self.carries(value);
                    if !carried.is_empty()
                        && let Some(binding) = pattern_binding_id(&let_stmt.pattern)
                    {
                        self.carries.entry(binding).or_default().extend(carried);
                    }
                }
            }
            Stmt::Return(ret) => {
                if let Some(value) = &ret.value {
                    self.sink_return(value);
                }
            }
            Stmt::TaskReturn(task) => self.sink_return(&task.value),
            _ => {}
        }
        ast::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            // A closure is checked as its own entity (obligation #2); do not
            // descend — its `return` is not the enclosing entity's sink.
            Expr::Closure(closure) => {
                self.ctx.check_closure(closure, self.out);
                return;
            }
            Expr::StructLiteral(lit) => {
                for field in &lit.fields {
                    self.sink_value(&field.value, Sink::StructField);
                }
            }
            Expr::TupleLiteral(t) => {
                for el in &t.elements {
                    self.sink_value(el, Sink::Tuple);
                }
            }
            Expr::Assign(assign) => {
                let carried = self.carries(&assign.value);
                if !carried.is_empty() {
                    let (root, through_deref) = self.place_root(&assign.target);
                    let span = assign.value.span();
                    match root {
                        Some(ident) => {
                            if let Some(name) = self.global_name(ident) {
                                self.mark(&carried, span, &Sink::Global(&name));
                            } else if is_ident(&assign.target) && !through_deref {
                                // Whole-local rebind (`r = value`): the local now
                                // carries the reference (not a write through it).
                                if let Some(def) = self.ctx.sem.references.get(&ident.id) {
                                    self.carries.entry(*def).or_default().extend(carried);
                                }
                            } else if through_deref || self.ident_is_ref(ident) {
                                // Writing into `*ref` / `ref.field` / a `&mut self`
                                // field persists into caller-visible memory.
                                self.mark(&carried, span, &Sink::ThroughRef);
                            } else if let Some(def) = self.ctx.sem.references.get(&ident.id) {
                                // A field / index of a plain local: the local now
                                // carries the reference.
                                self.carries.entry(*def).or_default().extend(carried);
                            }
                        }
                        None => self.mark(&carried, span, &Sink::ThroughRef),
                    }
                }
            }
            // Obligation #2 for a named-function reference: passing `fn_ref` to
            // a functor parameter whose declared `stores` is narrower is unsound
            // (the optimization trusts the slot's `stores`). The function's own
            // `stores` was verified honest by obligation #1.
            Expr::Call(call) => {
                self.check_functor_coercion(call);
                self.sink_call_args(expr);
            }
            Expr::MethodCall(_) | Expr::StaticMethodCall(_) => self.sink_call_args(expr),
            _ => {}
        }
        ast::walk_expr(self, expr);
    }
}

fn is_ident(expr: &Expr) -> bool {
    matches!(expr, Expr::Ident(_))
}

/// The binding [`AstId`] of a simple `let` pattern, or `None` for a
/// destructuring pattern (reference aliasing through destructuring is not
/// tracked).
fn pattern_binding_id(pattern: &ast::Pattern) -> Option<AstId> {
    match pattern {
        ast::Pattern::Ident { id, .. } | ast::Pattern::MutIdent { id, .. } => Some(*id),
        _ => None,
    }
}


// ---------------------------------------------------------------------------
// Semantics-based default-value purity checking (Design B)
// ---------------------------------------------------------------------------

/// Default-value purity over [`Semantics`] — the Design B default-value purity
/// checker. Every `param: T = expr` and
/// `field: T = expr` default must be pure: it may not call any function that
/// declares effects, nor install an effect handler. Walks the source default
/// expressions directly. Violations are returned for the caller to route.
#[must_use]
pub fn check_default_purity_semantic(sem: &Semantics) -> Vec<DefaultPurityError> {
    let mut out = Vec::new();
    let Some(state) = sem.state.as_ref() else {
        return out;
    };
    let data = OwnedEffectData::build(sem, state);
    run_purity_checks(sem, &data.index(), &mut out);
    out
}

/// Walk every user-authored parameter / field default, appending impurity
/// violations. Shared by [`check_default_purity_semantic`] and
/// [`check_semantics`].
fn run_purity_checks(sem: &Semantics, index: &EffectIndex, out: &mut Vec<DefaultPurityError>) {
    let Some(state) = sem.state.as_ref() else {
        return;
    };
    let walk = |annotations: Option<&crate::elaborator::sem::types::TypeAnnotations>,
                module: &ModuleSource,
                params: &[crate::ast::Param],
                out: &mut Vec<DefaultPurityError>| {
        for param in params {
            if let Some(default) = &param.default {
                purity_walk_default(sem, annotations, index, module, default, out);
            }
        }
    };

    for (src, module) in &sem.modules {
        if !crate::elaborator::liveness::is_user_authored(src) {
            continue;
        }
        let annotations = state.module_semantics.get(src).map(|m| &m.types);
        for item in &module.items {
            match item {
                Item::Function(func) => walk(annotations, src, &func.params, out),
                Item::Impl(impl_block) => {
                    for method in &impl_block.methods {
                        walk(annotations, src, &method.params, out);
                    }
                }
                Item::Trait(trait_decl) => {
                    // Parity with the effect checker's trait coverage. Note the
                    // trait/effect method signature path does not yet resolve
                    // param defaults (item.rs builds them with `default_expr:
                    // None` and no expression context), so a trait-method
                    // default's calls leave no `references` edge for the walker
                    // to flag until that annotation lands.
                    for method in &trait_decl.methods {
                        walk(annotations, src, &method.params, out);
                    }
                }
                Item::Struct(struct_decl) => {
                    for field in &struct_decl.fields {
                        if let Some(default) = &field.default {
                            purity_walk_default(sem, annotations, index, src, default, out);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn purity_walk_default(
    sem: &Semantics,
    annotations: Option<&crate::elaborator::sem::types::TypeAnnotations>,
    index: &EffectIndex,
    module: &ModuleSource,
    default: &Expr,
    out: &mut Vec<DefaultPurityError>,
) {
    let mut walker = PurityWalker {
        sem,
        annotations,
        index,
        module: module.source_path(),
        out,
    };
    walker.visit_expr(default);
}

/// Walks a default expression flagging any call to an effectful function (or an
/// effect-handler install), which would make the default impure.
struct PurityWalker<'a> {
    sem: &'a Semantics,
    annotations: Option<&'a crate::elaborator::sem::types::TypeAnnotations>,
    index: &'a EffectIndex<'a>,
    module: String,
    out: &'a mut Vec<DefaultPurityError>,
}

impl PurityWalker<'_> {
    fn flag_if_effectful(&mut self, effects: &[EffectRef], callee: &str, span: Span) {
        if !effects.is_empty() {
            self.out.push(DefaultPurityError {
                callee: callee.to_string(),
                span,
                module: self.module.clone(),
            });
        }
    }
}

impl AstVisitor for PurityWalker<'_> {
    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Call(call) => {
                let free = if let Expr::Ident(ident) = &call.callee {
                    self.sem
                        .references
                        .get(&ident.id)
                        .and_then(|def| self.index.fn_effects.get(def))
                        .map(|effects| (effects.clone(), ident.name.clone()))
                } else {
                    None
                };
                if let Some((effects, name)) = free {
                    self.flag_if_effectful(&effects, &name, call.span);
                } else if let Some(func_ref) = self
                    .annotations
                    .and_then(|ann| ann.static_method_dispatch.get(&call.id))
                    .map(|dispatch| dispatch.function_ref.clone())
                {
                    let effects = self.index.method_effects(&func_ref);
                    self.flag_if_effectful(&effects, callee_name(&call.callee), call.span);
                }
            }
            Expr::MethodCall(method_call) => {
                if let Some(dispatch) = self.sem.method_dispatch.get(&method_call.id) {
                    let effects = self.index.method_effects(&dispatch.function_ref);
                    self.flag_if_effectful(&effects, &method_call.method, method_call.span);
                }
            }
            Expr::StaticMethodCall(static_call) => {
                if let Some(func_ref) = self
                    .annotations
                    .and_then(|ann| ann.static_method_dispatch.get(&static_call.id))
                    .map(|dispatch| dispatch.function_ref.clone())
                {
                    let effects = self.index.method_effects(&func_ref);
                    self.flag_if_effectful(&effects, &static_call.method, static_call.span);
                }
            }
            Expr::WithHandler(with_handler) => {
                // Installing a handler touches the dispatch global — impure.
                self.out.push(DefaultPurityError {
                    callee: "<with-handler>".to_string(),
                    span: with_handler.span,
                    module: self.module.clone(),
                });
            }
            _ => {}
        }
        ast::walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_effect_error_display() {
        let error = EffectError {
            callee: "println".to_string(),
            missing_effect: "Stdout".to_string(),
            kind: EffectKind::Effect,
            span: Span {
                start: 100,
                end: 107,
                line: 10,
                column: 5,
                end_line: 10,
                end_column: 12,
            },
            module: "example/hello.wado".to_string(),
        };
        assert_eq!(
            error.to_string(),
            "10:5: missing effect 'Stdout' required by 'println'"
        );
        let diag = crate::compiler_host::Diagnostic::from(error);
        assert_eq!(diag.span.expect("span").file, "example/hello.wado");
    }
}
