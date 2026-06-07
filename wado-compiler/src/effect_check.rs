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

use crate::ast::{self, AstVisitor, Expr, Function, Item, Stmt};
use crate::semantics::Semantics;
use crate::symbol::SymbolKey;

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
            span: Some(DiagnosticSpan::from_span(&e.span, None)),
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
            span: Some(DiagnosticSpan::from_span(&e.span, None)),
        }
    }
}

/// Error from default-value purity checking
#[derive(Debug, Clone)]
pub struct DefaultPurityError {
    pub callee: String,
    pub span: Span,
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
            span: Some(DiagnosticSpan::from_span(&e.span, None)),
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
        };
        assert_eq!(
            error.to_string(),
            "10:5: missing effect 'Stdout' required by 'println'"
        );
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
    let index = data.index();

    for (src, module) in &sem.modules {
        if !crate::elaborator::liveness::is_user_authored(src) {
            continue;
        }
        for item in &module.items {
            match item {
                Item::Function(func) => {
                    check_function_effects_sem(sem, src, func, &index, &mut out);
                }
                Item::Impl(impl_block) => {
                    for method in &impl_block.methods {
                        check_function_effects_sem(sem, src, method, &index, &mut out);
                    }
                }
                Item::Trait(trait_decl) => {
                    for method in &trait_decl.methods {
                        check_function_effects_sem(sem, src, method, &index, &mut out);
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Owns the cross-module effect maps so multiple checks (effects, default
/// purity) can borrow a single [`EffectIndex`] view over them. Assembled once
/// from [`Semantics`] + [`AnnotateState`].
struct OwnedEffectData {
    fn_effects: IndexMap<SymbolKey, Vec<EffectRef>>,
    fn_params: IndexMap<SymbolKey, Vec<TypeId>>,
    mangled_index: IndexMap<(ModuleSource, String), Vec<EffectRef>>,
    mangled_params: IndexMap<(ModuleSource, String), Vec<TypeId>>,
    resource_names: IndexSet<(ModuleSource, String)>,
    struct_fields: IndexMap<(ModuleSource, String), Vec<TypeId>>,
    variant_payloads: IndexMap<(ModuleSource, String), Vec<TypeId>>,
    closure: IndexMap<EffectRef, IndexSet<EffectRef>>,
    effect_by_name: IndexMap<String, EffectRef>,
}

impl OwnedEffectData {
    fn build(sem: &Semantics, state: &crate::elaborator::orchestration::AnnotateState) -> Self {
        // Resolved effect lists, indexed two ways: by the function's
        // declaration key (free calls resolve through `references`) and by
        // `(module, mangled name)` (method dispatch carries a `FunctionRef`).
        let mut fn_effects: IndexMap<SymbolKey, Vec<EffectRef>> = IndexMap::default();
        let mut fn_params: IndexMap<SymbolKey, Vec<TypeId>> = IndexMap::default();
        let mut mangled_index: IndexMap<(ModuleSource, String), Vec<EffectRef>> =
            IndexMap::default();
        let mut mangled_params: IndexMap<(ModuleSource, String), Vec<TypeId>> = IndexMap::default();
        for (src, module_sem) in &state.module_semantics {
            let types = &module_sem.types;
            for (key, effects) in &types.function_effects {
                fn_effects.insert(key.clone(), effects.clone());
            }
            for (key, params) in &types.fn_param_types {
                fn_params.insert(key.clone(), params.clone());
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
                        if let Some(field_types) = annotations.and_then(|ann| {
                            ann.struct_field_types
                                .get(&SymbolKey::new(src.clone(), struct_decl.id))
                        }) {
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
        }
    }
}

/// The cross-module effect data the body walk consults, assembled once.
struct EffectIndex<'a> {
    /// Declaration key → resolved effects (free calls resolve via `references`).
    fn_effects: &'a IndexMap<SymbolKey, Vec<EffectRef>>,
    /// Declaration key → parameter type ids (for effect-parameter resolution).
    fn_params: &'a IndexMap<SymbolKey, Vec<TypeId>>,
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
    let caller_key = SymbolKey::new(module.clone(), func.id);

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
            &caller_key,
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
        module,
        sem,
        annotations,
        index,
        current,
        param_types,
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
            let Some(ops) = annotations
                .effect_ops
                .get(&SymbolKey::new(src.clone(), decl_id))
            else {
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
    fn_key: &SymbolKey,
    type_table: &TypeTable,
    struct_fields: &IndexMap<(ModuleSource, String), Vec<TypeId>>,
    variant_payloads: &IndexMap<(ModuleSource, String), Vec<TypeId>>,
    out: &mut IndexSet<EffectRef>,
) {
    let mut visited = TypeSet::default();
    for &type_id in annotations.fn_param_types.get(fn_key).into_iter().flatten() {
        collect_resource_refs(
            type_id,
            type_table,
            struct_fields,
            variant_payloads,
            out,
            &mut visited,
        );
    }
    if let Some(&return_type) = annotations.fn_return_types.get(fn_key) {
        collect_resource_refs(
            return_type,
            type_table,
            struct_fields,
            variant_payloads,
            out,
            &mut visited,
        );
    }
    if let Some(&task_return) = annotations.function_task_returns.get(fn_key) {
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
    module: &'a ModuleSource,
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
            let Some(arg_type) = self
                .sem
                .expression_types
                .get(&SymbolKey::new(self.module.clone(), arg.id()))
                .copied()
            else {
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
            });
        }
    }
}

impl AstVisitor for SemEffectWalker<'_> {
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
                    self.sem
                        .references
                        .get(&SymbolKey::new(self.module.clone(), ident.id))
                        .and_then(|def| {
                            self.index
                                .fn_effects
                                .get(def)
                                .map(|effects| (def.clone(), effects.clone(), ident.name.clone()))
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
                    .and_then(|ann| {
                        ann.static_method_dispatch
                            .get(&SymbolKey::new(self.module.clone(), call.id))
                    })
                    .map(|dispatch| dispatch.function_ref.clone())
                {
                    let effects = self.method_effects(&func_ref);
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
                let call_key = SymbolKey::new(self.module.clone(), method_call.id);
                if let Some(dispatch) = self.sem.method_dispatch.get(&call_key) {
                    let func_ref = dispatch.function_ref.clone();
                    let effects = self.method_effects(&func_ref);
                    let params = self.method_param_types(&func_ref);
                    let resolved =
                        self.resolve_effect_params(&effects, &params, true, &method_call.args);
                    self.report_missing(&resolved, &method_call.method, method_call.span);
                }
            }
            Expr::StaticMethodCall(static_call) => {
                let call_key = SymbolKey::new(self.module.clone(), static_call.id);
                if let Some(func_ref) = self
                    .annotations
                    .and_then(|ann| ann.static_method_dispatch.get(&call_key))
                    .map(|dispatch| dispatch.function_ref.clone())
                {
                    let effects = self.method_effects(&func_ref);
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
                    .filter_map(|b| b.effect.as_ref())
                    .filter_map(|ty| match ty {
                        crate::ast::Type::Named(named) => {
                            self.index.effect_by_name.get(&named.name).cloned()
                        }
                        _ => None,
                    })
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
                .get(&SymbolKey::new(self.module.clone(), ident.id))
                .and_then(|def| self.sem.local_types.get(def))
            {
                return Some(*type_id);
            }
        }
        self.sem
            .expression_types
            .get(&SymbolKey::new(self.module.clone(), call.callee.id()))
            .copied()
    }
}

// ---------------------------------------------------------------------------
// Semantics-based stores checking (Design B)
// ---------------------------------------------------------------------------

/// Stores checking over [`Semantics`] — the Design B stores checker. A
/// function that lets a reference parameter
/// escape — by returning it, storing it in a struct field, or assigning it to
/// a global — must declare `stores[param]`. Walks the source AST, so it sees
/// every function regardless of what reify emits and is immune to dead-code
/// gating. Violations are returned for the caller to route.
#[must_use]
pub fn check_stores_semantic(sem: &Semantics) -> Vec<StoresError> {
    let mut out = Vec::new();
    let Some(state) = sem.state.as_ref() else {
        return out;
    };

    for (src, module) in &sem.modules {
        if !crate::elaborator::liveness::is_user_authored(src) {
            continue;
        }
        let annotations = state.module_semantics.get(src).map(|m| &m.types);
        for item in &module.items {
            match item {
                Item::Function(func) => {
                    check_function_stores_sem(sem, src, func, annotations, &mut out);
                }
                Item::Impl(impl_block) => {
                    for method in &impl_block.methods {
                        check_function_stores_sem(sem, src, method, annotations, &mut out);
                    }
                }
                _ => {}
            }
        }
    }
    out
}

fn check_function_stores_sem(
    sem: &Semantics,
    module: &ModuleSource,
    func: &Function,
    annotations: Option<&crate::elaborator::sem::types::TypeAnnotations>,
    out: &mut Vec<StoresError>,
) {
    let Some(body) = &func.body else {
        return;
    };
    // `#[ambient]` bypasses the reference discipline; test helpers are exempt.
    if func.attrs.iter().any(|attr| attr.name == "ambient") || func.name.starts_with("__test_") {
        return;
    }
    if func.params.is_empty() {
        return;
    }
    let Some(annotations) = annotations else {
        return;
    };
    let key = SymbolKey::new(module.clone(), func.id);
    let Some(param_types) = annotations.fn_param_types.get(&key) else {
        return;
    };

    // Reference parameters: only `&T` / `&mut T` parameters can be stored, so
    // only they can produce a stores violation.
    let type_table = &sem.types;
    let mut ref_params: IndexSet<String> = IndexSet::default();
    for (param, &type_id) in func.params.iter().zip(param_types.iter()) {
        if matches!(
            type_table.get(type_id),
            ResolvedType::Ref(_) | ResolvedType::MutRef(_)
        ) {
            ref_params.insert(param.name.clone());
        }
    }
    if ref_params.is_empty() {
        return;
    }

    let stores: IndexSet<String> = func.stores.iter().cloned().collect();
    let mut walker = StoresWalker {
        module,
        annotations,
        ref_params,
        stores,
        out,
    };
    ast::walk_block(&mut walker, body);
}

/// Walks a function body flagging reference parameters that escape without a
/// matching `stores[param]` declaration.
struct StoresWalker<'a> {
    module: &'a ModuleSource,
    annotations: &'a crate::elaborator::sem::types::TypeAnnotations,
    /// Reference (`&T` / `&mut T`) parameter names of the enclosing function.
    ref_params: IndexSet<String>,
    /// `stores[...]`-declared parameter names — escapes of these are allowed.
    stores: IndexSet<String>,
    out: &'a mut Vec<StoresError>,
}

impl StoresWalker<'_> {
    /// If `expr` is a bare reference to a reference parameter that is *not*
    /// declared in `stores[...]`, return its name. Only a direct identifier
    /// counts — `&x.field` and the like do not store the parameter itself.
    fn unstored_ref_param<'e>(&self, expr: &'e Expr) -> Option<&'e str> {
        if let Expr::Ident(ident) = expr
            && self.ref_params.contains(&ident.name)
            && !self.stores.contains(&ident.name)
        {
            return Some(&ident.name);
        }
        None
    }
}

impl AstVisitor for StoresWalker<'_> {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        if let Stmt::Return(ret) = stmt
            && let Some(value) = &ret.value
            && let Some(param) = self.unstored_ref_param(value)
        {
            self.out.push(StoresError {
                message: format!(
                    "returning reference parameter '{param}' requires `stores[{param}]` declaration"
                ),
                span: value.span(),
            });
        }
        ast::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::StructLiteral(lit) => {
                for field in &lit.fields {
                    if let Some(param) = self.unstored_ref_param(&field.value) {
                        self.out.push(StoresError {
                            message: format!(
                                "storing reference parameter '{param}' in struct field requires `stores[{param}]` declaration"
                            ),
                            span: field.value.span(),
                        });
                    }
                }
            }
            Expr::Assign(assign) => {
                // A reference parameter assigned to a module global escapes; the
                // assign place recorded by the elaborator (the same fact reify
                // reads to build `GlobalVarSet`) identifies the global by name.
                if let Some(param) = self.unstored_ref_param(&assign.value)
                    && let Some(place) = self
                        .annotations
                        .assign_places
                        .get(&SymbolKey::new(self.module.clone(), assign.target.id()))
                    && let crate::elaborator::sem::types::AssignPlace::Global { name, .. } = place
                {
                    self.out.push(StoresError {
                        message: format!(
                            "storing reference parameter '{param}' in global '{name}' requires `stores[{param}]` declaration"
                        ),
                        span: assign.value.span(),
                    });
                }
            }
            _ => {}
        }
        ast::walk_expr(self, expr);
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
    let index = data.index();

    for (src, module) in &sem.modules {
        if !crate::elaborator::liveness::is_user_authored(src) {
            continue;
        }
        let annotations = state.module_semantics.get(src).map(|m| &m.types);
        for item in &module.items {
            match item {
                Item::Function(func) => {
                    for param in &func.params {
                        if let Some(default) = &param.default {
                            purity_walk_default(sem, src, annotations, &index, default, &mut out);
                        }
                    }
                }
                Item::Impl(impl_block) => {
                    for method in &impl_block.methods {
                        for param in &method.params {
                            if let Some(default) = &param.default {
                                purity_walk_default(
                                    sem, src, annotations, &index, default, &mut out,
                                );
                            }
                        }
                    }
                }
                Item::Struct(struct_decl) => {
                    for field in &struct_decl.fields {
                        if let Some(default) = &field.default {
                            purity_walk_default(sem, src, annotations, &index, default, &mut out);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

fn purity_walk_default(
    sem: &Semantics,
    module: &ModuleSource,
    annotations: Option<&crate::elaborator::sem::types::TypeAnnotations>,
    index: &EffectIndex,
    default: &Expr,
    out: &mut Vec<DefaultPurityError>,
) {
    let mut walker = PurityWalker {
        module,
        sem,
        annotations,
        index,
        out,
    };
    walker.visit_expr(default);
}

/// Walks a default expression flagging any call to an effectful function (or an
/// effect-handler install), which would make the default impure.
struct PurityWalker<'a> {
    module: &'a ModuleSource,
    sem: &'a Semantics,
    annotations: Option<&'a crate::elaborator::sem::types::TypeAnnotations>,
    index: &'a EffectIndex<'a>,
    out: &'a mut Vec<DefaultPurityError>,
}

impl PurityWalker<'_> {
    fn flag_if_effectful(&mut self, effects: &[EffectRef], callee: &str, span: Span) {
        if !effects.is_empty() {
            self.out.push(DefaultPurityError {
                callee: callee.to_string(),
                span,
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
                        .get(&SymbolKey::new(self.module.clone(), ident.id))
                        .and_then(|def| self.index.fn_effects.get(def))
                        .map(|effects| (effects.clone(), ident.name.clone()))
                } else {
                    None
                };
                if let Some((effects, name)) = free {
                    self.flag_if_effectful(&effects, &name, call.span);
                } else if let Some(func_ref) = self
                    .annotations
                    .and_then(|ann| {
                        ann.static_method_dispatch
                            .get(&SymbolKey::new(self.module.clone(), call.id))
                    })
                    .map(|dispatch| dispatch.function_ref.clone())
                {
                    let effects = self.index.method_effects(&func_ref);
                    self.flag_if_effectful(&effects, callee_name(&call.callee), call.span);
                }
            }
            Expr::MethodCall(method_call) => {
                if let Some(dispatch) = self
                    .sem
                    .method_dispatch
                    .get(&SymbolKey::new(self.module.clone(), method_call.id))
                {
                    let effects = self.index.method_effects(&dispatch.function_ref);
                    self.flag_if_effectful(&effects, &method_call.method, method_call.span);
                }
            }
            Expr::StaticMethodCall(static_call) => {
                if let Some(func_ref) = self
                    .annotations
                    .and_then(|ann| {
                        ann.static_method_dispatch
                            .get(&SymbolKey::new(self.module.clone(), static_call.id))
                    })
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
                });
            }
            _ => {}
        }
        ast::walk_expr(self, expr);
    }
}
