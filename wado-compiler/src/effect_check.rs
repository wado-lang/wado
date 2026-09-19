//! Effect and purity checking for Wado (Design B): that every call holds the
//! effects its callee requires, and that defaults and global initializers are
//! pure. Both read [`Semantics`] rather than the emitted TIR, so they see every
//! source function and run on the LSP path. Violations are returned.

use crate::hashmap::{IndexMap, IndexSet};

use crate::module_source::ModuleSource;
use crate::name::{FqTraitName, FqTypeName, is_test_function};
use crate::tir::{EffectRef, FunctionRef, ResolvedType, TypeId, TypeSet, TypeTable};
use crate::token::Span;

use crate::ast::{
    self, AstId, AstVisitor, AttrArg, Attribute, CmImport, EffectHandlerBinding, Expr, Function,
    ImplBlock, Item, Stmt, TraitDecl, cm_import_of,
};
use crate::compiler_host::Diagnostic;
use crate::defs::DefId;
use crate::elaborator::liveness::is_user_authored;
use crate::elaborator::orchestration::AnnotateState;
use crate::elaborator::sem::types::{ForOfIteratorInfo, ImplFacts, TypeAnnotations};
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

/// What the effect checker found at the reported position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectFault {
    /// The caller does not hold what its callee requires.
    Missing(EffectKind),
    /// An `impl` method declares an effect its trait method leaves out.
    UndeclaredByTrait,
    /// The callee's effects are left open, and the caller forwards none.
    MissingOpen,
}

/// Error from effect checking
#[derive(Debug, Clone)]
pub struct EffectError {
    /// The function being called, or the trait method being implemented
    pub callee: String,
    /// The effect the caller lacks, or the impl adds
    pub missing_effect: String,
    /// Which violation this is, and how it words itself
    pub fault: EffectFault,
    /// Source location of the call
    pub span: Span,
    pub module: String,
}

impl From<EffectError> for Diagnostic {
    fn from(e: EffectError) -> Self {
        use crate::compiler_host::{Code, DiagnosticSpan, Severity};
        let message = match e.fault {
            EffectFault::Missing(kind) => format!(
                "missing {} '{}' required by '{}'",
                kind.noun(),
                e.missing_effect,
                e.callee
            ),
            EffectFault::UndeclaredByTrait => format!(
                "effect '{}' is not declared by trait method '{}'",
                e.missing_effect, e.callee
            ),
            EffectFault::MissingOpen => format!(
                "missing effects required by '{}': its trait leaves them to the impl, so declare `with _`",
                e.callee
            ),
        };
        Diagnostic {
            severity: Severity::Error,
            code: Code::TypeMismatch,
            message,
            span: Some(DiagnosticSpan::from_span(&e.span, Some(&e.module))),
        }
    }
}

/// A position whose expression must be pure. The diagnostic names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PureContext {
    DefaultValue,
    GlobalInitializer,
}

impl PureContext {
    fn noun(self) -> &'static str {
        match self {
            Self::DefaultValue => "default value expression",
            Self::GlobalInitializer => "global initializer",
        }
    }
}

/// Why an expression that must be pure is rejected, as the diagnostic words it.
#[derive(Debug, Clone)]
pub enum Impurity {
    /// The named callee declares an effect.
    Call(String),
    /// The named operation is backed by the host, so dispatching it demands a
    /// capability. A user-defined effect's operation demands none: it traps
    /// where no handler answers, which is a runtime outcome, not an impurity.
    Dispatch(String),
}

/// One impurity, at the position that must not hold it.
#[derive(Debug, Clone)]
pub struct PurityError {
    pub context: PureContext,
    pub impurity: Impurity,
    pub span: Span,
    pub module: String,
}

impl From<PurityError> for Diagnostic {
    fn from(e: PurityError) -> Self {
        use crate::compiler_host::{Code, DiagnosticSpan, Severity};
        let noun = e.context.noun();
        let message = match &e.impurity {
            Impurity::Call(callee) => {
                format!("{noun} must be pure (no effects), but calls effectful function '{callee}'")
            }
            Impurity::Dispatch(op) => format!(
                "{noun} must be pure (no effects), but dispatches '{op}', which needs a \
                 capability the position does not hold"
            ),
        };
        Diagnostic {
            severity: Severity::Error,
            code: Code::TypeMismatch,
            message,
            span: Some(DiagnosticSpan::from_span(&e.span, Some(&e.module))),
        }
    }
}

/// The members of a nominal type, by the declaration's module and name.
type MemberTable = IndexMap<(ModuleSource, String), Vec<TypeId>>;

/// Every declaration's members: a struct's fields, a variant's case payloads.
#[derive(Default)]
struct MemberTables {
    struct_fields: MemberTable,
    variant_payloads: MemberTable,
}

impl MemberTables {
    fn collect(sem: &Semantics, state: &AnnotateState) -> Self {
        let mut tables = Self::default();
        for (src, module) in &sem.modules {
            let annotations = state.module_semantics.get(src).map(|m| &m.types);
            for item in &module.items {
                if let Item::Struct(struct_decl) = item
                    && let Some(field_types) =
                        annotations.and_then(|ann| ann.struct_field_types.get(&struct_decl.id))
                {
                    tables
                        .struct_fields
                        .insert((src.clone(), struct_decl.name.clone()), field_types.clone());
                }
            }
        }
        for info in state.tysys.all_variant_cases.values() {
            tables.variant_payloads.insert(
                (info.module_source.clone(), info.name.clone()),
                info.cases.iter().map(|case| case.payload).collect(),
            );
        }
        tables
    }

    /// The members `type_id` declares, an instance's answered from its own type
    /// arguments.
    ///
    /// Only a member the head spells as a bare slot is answered. One that buries
    /// a slot (`&Array<T>`) would have to be substituted into, which interns a
    /// type, and this phase holds the table shared.
    fn of<'a>(&'a self, type_id: TypeId, tt: &'a TypeTable) -> impl Iterator<Item = TypeId> + 'a {
        let key = tt
            .nominal_head(type_id)
            .map(|(name, module)| (module, name));
        let fields = key.as_ref().and_then(|k| self.struct_fields.get(k));
        let payloads = key.as_ref().and_then(|k| self.variant_payloads.get(k));
        let type_args = tt.nominal_type_args(type_id).unwrap_or_default();
        fields
            .into_iter()
            .flatten()
            .chain(payloads.into_iter().flatten())
            .map(move |&member| match tt.get(member) {
                ResolvedType::TypeParam { index, .. } => {
                    type_args.get(*index as usize).copied().unwrap_or(member)
                }
                _ => member,
            })
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
    members: &MemberTables,
    out: &mut IndexSet<EffectRef>,
    visited: &mut TypeSet,
) {
    if !visited.insert(type_id) {
        return;
    }
    let ty = tt.get(type_id);
    match ty {
        ResolvedType::Resource { def } | ResolvedType::GenericResource { def, .. } => {
            // A `resource Child extends Parent` value is usable wherever the
            // parent is, so holding it holds every ancestor too.
            for ancestor in tt.resource_chain(*def) {
                out.insert(EffectRef::Concrete {
                    name: tt.def_name(ancestor).to_string(),
                    module_source: tt.def_module(ancestor).clone(),
                });
            }
            if let ResolvedType::GenericResource { type_args, .. } = ty {
                for ta in type_args {
                    collect_resource_refs(*ta, tt, members, out, visited);
                }
            }
        }
        ResolvedType::GenericInstance { type_args, .. } => {
            for ta in type_args {
                collect_resource_refs(*ta, tt, members, out, visited);
            }
            for member in members.of(type_id, tt) {
                collect_resource_refs(member, tt, members, out, visited);
            }
        }
        ResolvedType::Ref(t)
        | ResolvedType::MutRef(t)
        | ResolvedType::Reactive(t)
        | ResolvedType::BuiltinArray(t) => {
            collect_resource_refs(*t, tt, members, out, visited);
        }
        ResolvedType::Function {
            params,
            return_type,
            ..
        } => {
            for p in params {
                collect_resource_refs(*p, tt, members, out, visited);
            }
            collect_resource_refs(*return_type, tt, members, out, visited);
        }
        ResolvedType::Newtype { base_type, .. } => {
            collect_resource_refs(*base_type, tt, members, out, visited);
        }
        ResolvedType::Struct { .. } | ResolvedType::Variant { .. } => {
            for member in members.of(type_id, tt) {
                collect_resource_refs(member, tt, members, out, visited);
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

/// Effect checking over [`Semantics`], run after `annotate_bodies` so it sees
/// every function, dead or live. Covers free-function, method, and static
/// dispatch with resource injection, the effect / resource propagation closure,
/// signature-resource inference, effect-parameter resolution, `#[benign]`,
/// handler-scope grants, and closure calls, over user-authored modules.
#[must_use]
pub fn check_effects_semantic(sem: &Semantics) -> Vec<EffectError> {
    let mut out = Vec::new();
    let Some(state) = sem.state.as_ref() else {
        return out;
    };
    let data = OwnedEffectData::build(sem, state, IndexSet::default());
    run_effect_checks(sem, &data.index(), &mut out);
    out
}

/// Both Design-B semantic diagnostics, computed in one pass that builds the
/// shared `OwnedEffectData` once. Used by the batch driver and the LSP so
/// effect and purity stay in lockstep across both.
#[must_use]
pub fn check_semantics(
    sem: &Semantics,
    provided_import_fqs: IndexSet<String>,
) -> SemanticDiagnostics {
    let mut diags = SemanticDiagnostics::default();
    let Some(state) = sem.state.as_ref() else {
        return diags;
    };
    let data = OwnedEffectData::build(sem, state, provided_import_fqs);
    let index = data.index();
    run_effect_checks(sem, &index, &mut diags.effects);
    run_purity_checks(sem, &index, &mut diags.purity);
    diags
}

/// Bundle of the Design-B semantic diagnostics returned by [`check_semantics`].
#[derive(Default)]
pub struct SemanticDiagnostics {
    pub effects: Vec<EffectError>,
    pub purity: Vec<PurityError>,
}

impl SemanticDiagnostics {
    /// Whether any check produced a violation.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.effects.is_empty() && self.purity.is_empty()
    }
}

/// Walk every user-authored function / method / trait method, appending effect
/// violations. Shared by [`check_effects_semantic`] and [`check_semantics`].
fn run_effect_checks(sem: &Semantics, index: &EffectIndex, out: &mut Vec<EffectError>) {
    for (src, module) in &sem.modules {
        if !is_user_authored(src) {
            continue;
        }
        for item in &module.items {
            match item {
                Item::Function(func) => {
                    check_function_effects_sem(sem, src, func, index, None, out);
                }
                Item::Impl(impl_block) => {
                    let handled = handled_effect(sem, src, impl_block, index);
                    check_impl_effect_conformance(sem, src, impl_block, index, out);
                    for method in &impl_block.methods {
                        check_function_effects_sem(sem, src, method, index, handled.as_ref(), out);
                    }
                }
                Item::Trait(trait_decl) => {
                    for method in &trait_decl.methods {
                        check_function_effects_sem(sem, src, method, index, None, out);
                    }
                }
                // An operation's default body is ordinary code, so what it
                // performs is checked like any other function's.
                Item::Interface(interface_decl) => {
                    for method in &interface_decl.methods {
                        check_function_effects_sem(sem, src, method, index, None, out);
                    }
                }
                _ => {}
            }
        }
    }
}

/// A trait method, by the trait's declaring module, the trait name, and the
/// method name.
type TraitMethodKey = (ModuleSource, String, String);

/// A trait, by its declaring module and name.
type TraitKey = (ModuleSource, String);

/// One trait impl, by the head of the type it targets and the trait it
/// implements.
type ImplKey = (FqTypeName, TraitKey);

/// The key one `impl Trait for Type` is recorded under. `None` for a trait
/// whose name carries no declaring module.
fn impl_key(struct_name: &FqTypeName, trait_name: &FqTraitName) -> Option<ImplKey> {
    Some((
        struct_name.head_only(),
        (
            trait_name.module()?.clone(),
            trait_name.base_name().to_string(),
        ),
    ))
}

/// Whether `name` is an effect parameter of the trait or of the method, rather
/// than an effect declaration the module can resolve.
fn declares_effect_param(trait_decl: &TraitDecl, method: &Function, name: &str) -> bool {
    let mut params = trait_decl.type_params.iter().chain(&method.type_params);
    params.any(|p| p.is_effect && p.name == name)
}

/// The traits bounding each type parameter, by the slot
/// `Scope::register_generic_params` gives it.
fn bound_traits_per_slot(
    type_params: &[ast::GenericParam],
    sem: &Semantics,
    trait_by_def: &IndexMap<DefId, TraitKey>,
) -> Vec<Vec<TraitKey>> {
    let Some(resolutions) = sem.resolutions() else {
        return Vec::new();
    };
    type_params
        .iter()
        .filter(|p| p.is_real_type_param())
        .map(|p| {
            p.bounds
                .iter()
                .filter_map(|bound| resolutions.declared(bound.id))
                .filter_map(|def| trait_by_def.get(&def).cloned())
                .collect()
        })
        .collect()
}

/// Owns the cross-module effect maps so multiple checks (effects, default
/// purity) can borrow a single [`EffectIndex`] view over them. Assembled once
/// from [`Semantics`] + [`AnnotateState`].
struct OwnedEffectData {
    fn_effects: IndexMap<AstId, Vec<EffectRef>>,
    fn_params: IndexMap<AstId, Vec<TypeId>>,
    mangled_index: IndexMap<(ModuleSource, String), Vec<EffectRef>>,
    mangled_params: IndexMap<(ModuleSource, String), Vec<TypeId>>,
    trait_method_effects: IndexMap<TraitMethodKey, Vec<EffectRef>>,
    /// The traits that leave their effects to the impl — a `with _` head, or a
    /// bare one, which reads as the same.
    open_traits: IndexSet<TraitKey>,
    /// Per type-parameter slot of a function declaration, the traits bounding
    /// it, so a call site can read what its type arguments implement.
    fn_bound_traits: IndexMap<AstId, Vec<Vec<TraitKey>>>,
    /// Every effect an impl's methods declare, for resolving a trait head's
    /// effect hole against the type a call instantiates it with.
    impl_effects: IndexMap<ImplKey, Vec<EffectRef>>,
    resource_names: IndexSet<(ModuleSource, String)>,
    members: MemberTables,
    closure: IndexMap<EffectRef, IndexSet<EffectRef>>,
    effect_by_name: IndexMap<String, EffectRef>,
    /// `#[cm]` FQ per interface declaration.
    interface_cm_fq: IndexMap<(ModuleSource, String), Option<String>>,
    effect_by_cm_fq: IndexMap<String, EffectRef>,
    /// CM interface FQs the consumer satisfies with a provider component; a
    /// reconstructed host-leaf import in this set is discharged (composition-
    /// relative — it bottoms out at a fused sibling, not the host).
    provided_import_fqs: IndexSet<String>,
}

impl OwnedEffectData {
    fn build(
        sem: &Semantics,
        state: &AnnotateState,
        provided_import_fqs: IndexSet<String>,
    ) -> Self {
        // Resolved effect lists, indexed two ways: by the function's
        // declaration key (free calls resolve through `references`) and by
        // `(module, mangled name)` (method dispatch carries a `FunctionRef`).
        let mut fn_effects: IndexMap<AstId, Vec<EffectRef>> = IndexMap::default();
        let mut fn_params: IndexMap<AstId, Vec<TypeId>> = IndexMap::default();
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
        for (src, module) in &sem.modules {
            for item in &module.items {
                if let Item::Resource(resource) = item {
                    resource_names.insert((src.clone(), resource.name.clone()));
                }
            }
        }

        let members = MemberTables::collect(sem, state);

        // Effect / resource propagation closure: holding effect `E` admits the
        // resources `E`'s operations reference (e.g. `Stdout` → `Stream`).
        let closure = build_propagation_closure_sem(sem, state, &members);

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

        // An empty entry is meaningful: the method exists and grants nothing.
        // A declaration has no body, so `fn_effects` holds nothing for it.
        let mut trait_method_effects: IndexMap<TraitMethodKey, Vec<EffectRef>> =
            IndexMap::default();
        let mut trait_by_def: IndexMap<DefId, TraitKey> = IndexMap::default();
        let mut open_traits: IndexSet<TraitKey> = IndexSet::default();
        for (src, module) in &sem.modules {
            for item in &module.items {
                let Item::Trait(trait_decl) = item else {
                    continue;
                };
                if let Some(def) = sem.resolutions().and_then(|r| r.declared(trait_decl.id)) {
                    trait_by_def.insert(def, (src.clone(), trait_decl.name.clone()));
                }
                if trait_decl.head.is_open() {
                    open_traits.insert((src.clone(), trait_decl.name.clone()));
                }
                for method in &trait_decl.methods {
                    let effects = method
                        .effects
                        .iter()
                        .map(|name| {
                            // A name the trait or the method declares as an
                            // effect parameter stands for whatever the impl
                            // brings, so it never resolves to a declaration.
                            if declares_effect_param(trait_decl, method, name) {
                                return EffectRef::Param { name: name.clone() };
                            }
                            effect_named_in(name, src, sem, &closure, &effect_by_name).unwrap_or(
                                EffectRef::Concrete {
                                    name: name.clone(),
                                    module_source: src.clone(),
                                },
                            )
                        })
                        .collect();
                    trait_method_effects.insert(
                        (src.clone(), trait_decl.name.clone(), method.name.clone()),
                        effects,
                    );
                }
            }
        }

        let mut fn_bound_traits: IndexMap<AstId, Vec<Vec<TraitKey>>> = IndexMap::default();
        let mut impl_effects: IndexMap<ImplKey, Vec<EffectRef>> = IndexMap::default();
        for (src, module) in &sem.modules {
            let annotations = state.module_semantics.get(src).map(|m| &m.types);
            for item in &module.items {
                match item {
                    Item::Function(func) if !func.type_params.is_empty() => {
                        fn_bound_traits.insert(
                            func.id,
                            bound_traits_per_slot(&func.type_params, sem, &trait_by_def),
                        );
                    }
                    Item::Impl(block) => {
                        let Some(facts) = annotations.and_then(|ann| ann.impl_facts.get(&block.id))
                        else {
                            continue;
                        };
                        let Some(key) = facts
                            .trait_name
                            .as_ref()
                            .and_then(|trait_name| impl_key(&facts.struct_name, trait_name))
                        else {
                            continue;
                        };
                        let entry: &mut Vec<EffectRef> = impl_effects.entry(key).or_default();
                        for effect in block
                            .methods
                            .iter()
                            .filter_map(|method| fn_effects.get(&method.id))
                            .flatten()
                        {
                            if !entry.contains(effect) {
                                entry.push(effect.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        let mut interface_cm_fq: IndexMap<(ModuleSource, String), Option<String>> =
            IndexMap::default();
        // Restricted to closure keys, so a host-leaf import resolves to an
        // effect while a type-only interface (`wasi:cli/types`) resolves to
        // nothing.
        let mut effect_by_cm_fq: IndexMap<String, EffectRef> = IndexMap::default();
        for (src, module) in &sem.modules {
            for item in &module.items {
                let Item::Interface(decl) = item else {
                    continue;
                };
                let cm_fq = cm_import_of(&decl.attrs).map(CmImport::interface_path);
                interface_cm_fq.insert((src.clone(), decl.name.clone()), cm_fq.clone());
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
            trait_method_effects,
            open_traits,
            fn_bound_traits,
            impl_effects,
            resource_names,
            members,
            closure,
            effect_by_name,
            interface_cm_fq,
            effect_by_cm_fq,
            provided_import_fqs,
        }
    }

    fn index(&self) -> EffectIndex<'_> {
        EffectIndex {
            fn_effects: &self.fn_effects,
            fn_params: &self.fn_params,
            mangled_index: &self.mangled_index,
            mangled_params: &self.mangled_params,
            trait_method_effects: &self.trait_method_effects,
            open_traits: &self.open_traits,
            fn_bound_traits: &self.fn_bound_traits,
            impl_effects: &self.impl_effects,
            resource_names: &self.resource_names,
            members: &self.members,
            closure: &self.closure,
            effect_by_name: &self.effect_by_name,
            interface_cm_fq: &self.interface_cm_fq,
            effect_by_cm_fq: &self.effect_by_cm_fq,
            provided_import_fqs: &self.provided_import_fqs,
        }
    }
}

/// The cross-module effect data the body walk consults, assembled once.
struct EffectIndex<'a> {
    /// Declaration key → resolved effects (free calls resolve via `references`).
    fn_effects: &'a IndexMap<AstId, Vec<EffectRef>>,
    /// Declaration key → parameter type ids (for effect-parameter resolution).
    fn_params: &'a IndexMap<AstId, Vec<TypeId>>,
    /// `(module, mangled name)` → effects (method / static dispatch).
    mangled_index: &'a IndexMap<(ModuleSource, String), Vec<EffectRef>>,
    /// `(module, mangled name)` → parameter type ids.
    mangled_params: &'a IndexMap<(ModuleSource, String), Vec<TypeId>>,
    /// Trait method → the effects it declares. A call through a type
    /// parameter's bound selects no impl, so this is what it can demand.
    trait_method_effects: &'a IndexMap<TraitMethodKey, Vec<EffectRef>>,
    /// The traits that leave their effects to the impl.
    open_traits: &'a IndexSet<TraitKey>,
    /// Per type-parameter slot of a function declaration, the traits bounding it.
    fn_bound_traits: &'a IndexMap<AstId, Vec<Vec<TraitKey>>>,
    /// Every effect one impl's methods declare.
    impl_effects: &'a IndexMap<ImplKey, Vec<EffectRef>>,
    /// Declared resources, for resource injection and effect classification.
    resource_names: &'a IndexSet<(ModuleSource, String)>,
    /// Declared members, for nested-resource detection.
    members: &'a MemberTables,
    /// Effect → implied resources propagation closure.
    closure: &'a IndexMap<EffectRef, IndexSet<EffectRef>>,
    /// Declared effect / resource name → resolved `EffectRef` (`#[benign]`).
    effect_by_name: &'a IndexMap<String, EffectRef>,
    /// Interface declaration → its `#[cm]` FQ, for resolving a direct `E::op()`
    /// callee to its effect and FQ.
    interface_cm_fq: &'a IndexMap<(ModuleSource, String), Option<String>>,
    /// CM interface FQ → the effect it declares, for reconstructing a
    /// component's host-leaf imports into effects.
    effect_by_cm_fq: &'a IndexMap<String, EffectRef>,
    /// CM interface FQs the consumer provides (discharged in reconstruction).
    provided_import_fqs: &'a IndexSet<String>,
}

/// The segment naming the interface in a dispatch path `[ns::]*E::op`: the one
/// before the operation, so a namespace qualifier ahead of `E` does not stand in
/// for it.
fn interface_segment(callee: &Expr) -> Option<&ast::PathSegment> {
    let Expr::Ident(ident) = callee else {
        return None;
    };
    ident.owner_segment()
}

/// The `interface` the name at `site` declares, as its declaring module, its
/// name, and its `#[cm]` FQ. `None` when the name declares anything else.
fn interface_at<'a>(
    sem: &Semantics,
    index: &EffectIndex<'a>,
    site: Option<AstId>,
) -> Option<(ModuleSource, String, &'a Option<String>)> {
    let resolutions = sem.resolutions()?;
    let def = resolutions.declared(site?)?;
    let defs = resolutions.defs();
    let key = (defs.module(def).clone(), defs.name(def).to_string());
    let cm_fq = index.interface_cm_fq.get(&key)?;
    Some((key.0, key.1, cm_fq))
}

/// The effects `with E => h do` grants to its body.
fn binding_granted_effects(
    sem: &Semantics,
    annotations: Option<&TypeAnnotations>,
    index: &EffectIndex,
    module_source: &ModuleSource,
    binding: &EffectHandlerBinding,
) -> Vec<EffectRef> {
    // One fact per walk that reached the binding. A handler installed in a
    // tuple `for-of` body is bound once per element, and an effect only some
    // elements grant does not cover the body — so the grant is what every walk
    // agrees on.
    let mut granted: Option<Vec<EffectRef>> = None;
    for facts in annotations
        .into_iter()
        .flat_map(|a| a.all(|f| &f.handler_bindings, binding.id))
    {
        let walk: Vec<EffectRef> = facts
            .effects
            .iter()
            .map(|entry| EffectRef::Concrete {
                name: entry.name.clone(),
                module_source: entry.module_source.clone(),
            })
            .filter(|effect| index.closure.contains_key(effect))
            .collect();
        granted = Some(match granted {
            None => walk,
            Some(prev) => prev.into_iter().filter(|e| walk.contains(e)).collect(),
        });
    }
    if let Some(granted) = granted {
        return granted;
    }
    binding
        .effect
        .as_ref()
        .and_then(|ty| match ty {
            ast::Type::Named(named) => effect_named_in(
                &named.name,
                module_source,
                sem,
                index.closure,
                index.effect_by_name,
            ),
            _ => None,
        })
        .into_iter()
        .collect()
}

/// What a direct `E::op()` call at `site` demands of its caller.
///
/// Empty where it demands nothing: the site names no interface, or `E` is a
/// user-defined effect, whose operation an installed handler answers and whose
/// dispatch with none traps — a runtime outcome, not a demand on the position.
/// A purely computational component's operation demands nothing either.
fn operation_requirements(
    sem: &Semantics,
    index: &EffectIndex,
    site: Option<AstId>,
) -> Vec<EffectRef> {
    // The callee names its interface's declaration; the site says which one
    // that is, so a same-named local `interface` cannot stand in for it.
    let Some((decl_module, name, cm_fq)) = interface_at(sem, index, site) else {
        return Vec::new();
    };
    let Some(fq) = cm_fq else {
        return Vec::new();
    };
    if let Some(registry) = sem.cm_interface_registry()
        && registry.is_component_interface(fq)
    {
        // Composition-relative: the imported interface is composed away, so its
        // operations demand the dependency's own host-leaf capabilities.
        return registry
            .host_leaf_imports_for(fq)
            .iter()
            .filter(|leaf| !index.provided_import_fqs.contains(leaf.as_str()))
            .filter_map(|leaf| index.effect_by_cm_fq.get(leaf).cloned())
            .collect();
    }
    vec![EffectRef::Concrete {
        name,
        module_source: decl_module,
    }]
}

/// What the elaborator recorded about one `impl` block.
fn impl_facts<'a>(
    sem: &'a Semantics,
    module: &ModuleSource,
    impl_block: &ImplBlock,
) -> Option<&'a ImplFacts> {
    sem.state
        .as_ref()?
        .module_semantics
        .get(module)?
        .types
        .impl_facts
        .get(&impl_block.id)
}

/// The effect an `impl E for T` block handles, when `E` is one. Read off the
/// impl facts, which name the trait by its declaring module: a plain trait
/// spelled like an effect is a different declaration and grants nothing.
fn handled_effect(
    sem: &Semantics,
    module: &ModuleSource,
    impl_block: &ImplBlock,
    index: &EffectIndex,
) -> Option<EffectRef> {
    let facts = impl_facts(sem, module, impl_block)?;
    if !facts.is_handler_method {
        return None;
    }
    let trait_name = facts.trait_name.as_ref()?;
    let effect = EffectRef::Concrete {
        name: trait_name.base_name().to_string(),
        module_source: trait_name.module()?.clone(),
    };
    index.closure.contains_key(&effect).then_some(effect)
}

/// Reports an impl method declaring an effect its trait method leaves out.
/// An `interface` handler and a `resource` impl declare none, so both pass.
fn check_impl_effect_conformance(
    sem: &Semantics,
    module: &ModuleSource,
    impl_block: &ImplBlock,
    index: &EffectIndex,
    out: &mut Vec<EffectError>,
) {
    let Some(trait_name) = impl_facts(sem, module, impl_block).and_then(|f| f.trait_name.as_ref())
    else {
        return;
    };
    for method in &impl_block.methods {
        let Some(declared_by_trait) = index.effects_declared_by(trait_name, &method.name) else {
            continue;
        };
        let Some(declared) = index.fn_effects.get(&method.id) else {
            continue;
        };
        // An open head stands for whatever the impl brings, so it allows
        // everything rather than one set.
        if declared_by_trait.iter().any(EffectRef::is_param) {
            continue;
        }
        let allowed: IndexSet<EffectRef> = declared_by_trait
            .iter()
            .map(|effect| canonicalize_effect(effect, index.closure, index.effect_by_name))
            .collect();
        for effect in declared {
            let effect = &canonicalize_effect(effect, index.closure, index.effect_by_name);
            if effect.is_param() || allowed.contains(effect) {
                continue;
            }
            out.push(EffectError {
                callee: format!("{}::{}", trait_name.base_name(), method.name),
                missing_effect: effect.name().to_string(),
                fault: EffectFault::UndeclaredByTrait,
                span: method.span,
                module: module.to_string(),
            });
        }
    }
}

/// `handled` is the effect a method of `impl E for T` handles.
fn check_function_effects_sem(
    sem: &Semantics,
    module: &ModuleSource,
    func: &Function,
    index: &EffectIndex,
    handled: Option<&EffectRef>,
    out: &mut Vec<EffectError>,
) {
    let Some(body) = &func.body else {
        return;
    };
    // `#[ambient]` bypasses the effect system; test helpers implicitly hold
    // every effect.
    if func.attrs.iter().any(|attr| attr.name == "ambient") || is_test_function(&func.name) {
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
        add_signature_resources(ann, caller_key, &sem.types, index.members, &mut current);
    }
    // A handler method holds the effect it handles: `E::op()` from inside
    // `impl E for T` delegates to the outer handler, what `..forward` desugars to.
    if let Some(effect) = handled {
        current.insert(effect.clone());
    }
    // `#[benign(E)]` admits `E` in the body without a `with E` clause.
    for name in benign_effect_names(&func.attrs) {
        if let Some(effect) =
            effect_named_in(&name, module, sem, index.closure, index.effect_by_name)
        {
            current.insert(effect);
        }
    }
    // Canonicalise before expanding so the closure keys (built from the
    // declarations, i.e. canonical) match, then expand: a function holding
    // `Stdout` may call operations that internally need `Stream`, etc.
    let current: IndexSet<EffectRef> = current
        .iter()
        .map(|effect| canonicalize_effect(effect, index.closure, index.effect_by_name))
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
        module_source: module.clone(),
        out,
    };
    ast::walk_block(&mut walker, body);
}

/// Build the effect / resource propagation closure from `Semantics`: for each
/// effect or resource declaration, the resources its operations' parameter and
/// return types reference, transitively closed. Reads the resolved operation
/// signatures from the `effect_ops` facts; `members` lets resource detection
/// descend into an operation type's own members.
fn build_propagation_closure_sem(
    sem: &Semantics,
    state: &AnnotateState,
    members: &MemberTables,
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
                        members,
                        &mut refs,
                        &mut TypeSet::default(),
                    );
                }
                collect_resource_refs(
                    op.return_type,
                    type_table,
                    members,
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

/// The effect a name written in `module` refers to. An attribute argument and a
/// `with` clause type carry a spelling, so the declaration it reaches decides —
/// the module's own first, then what it imported, aliases and all.
fn effect_named_in(
    name: &str,
    module: &ModuleSource,
    sem: &Semantics,
    closure: &IndexMap<EffectRef, IndexSet<EffectRef>>,
    effect_by_name: &IndexMap<String, EffectRef>,
) -> Option<EffectRef> {
    let local = EffectRef::Concrete {
        name: name.to_string(),
        module_source: module.clone(),
    };
    if closure.contains_key(&local) {
        return Some(local);
    }
    if let Some(resolutions) = sem.resolutions()
        && let Some(def) = resolutions.imported_as(module, name)
    {
        let defs = resolutions.defs();
        let imported = EffectRef::Concrete {
            name: defs.name(def).to_string(),
            module_source: defs.module(def).clone(),
        };
        if closure.contains_key(&imported) {
            return Some(imported);
        }
    }
    effect_by_name.get(name).cloned()
}

/// The declaration an effect reference names. One the closure knows already is;
/// a spelling it does not — a re-export seen from the recording module —
/// resolves through the by-name index. Effect parameters pass through.
fn canonicalize_effect(
    effect: &EffectRef,
    closure: &IndexMap<EffectRef, IndexSet<EffectRef>>,
    effect_by_name: &IndexMap<String, EffectRef>,
) -> EffectRef {
    if closure.contains_key(effect) {
        return effect.clone();
    }
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
/// Resources nested inside a type's own members are followed via `members`;
/// direct and container-nested ones (`Option<R>`, `List<R>`, `&R`,
/// `fn() -> R`) are too.
fn add_signature_resources(
    annotations: &TypeAnnotations,
    fn_key: AstId,
    type_table: &TypeTable,
    members: &MemberTables,
    out: &mut IndexSet<EffectRef>,
) {
    let mut visited = TypeSet::default();
    for &type_id in annotations
        .fn_param_types
        .get(&fn_key)
        .into_iter()
        .flatten()
    {
        collect_resource_refs(type_id, type_table, members, out, &mut visited);
    }
    if let Some(&return_type) = annotations.fn_return_types.get(&fn_key) {
        collect_resource_refs(return_type, type_table, members, out, &mut visited);
    }
    if let Some(&task_return) = annotations.function_task_returns.get(&fn_key) {
        collect_resource_refs(task_return, type_table, members, out, &mut visited);
    }
}

/// `#[benign(E, F)]` effect names declared on a function.
fn benign_effect_names(attrs: &[Attribute]) -> Vec<String> {
    attrs
        .iter()
        .filter(|attr| attr.name == "benign")
        .flat_map(|attr| attr.args.iter().map(AttrArg::as_str))
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

/// What a diagnostic calls a callee no name reaches.
pub const INDIRECT_CALLEE: &str = "(indirect call)";

/// Type of an indirect call's callee, preferring the enclosing function's
/// parameter types: a function-typed parameter callee leaves no `references`
/// edge or recorded expression type at the call, so nothing else names it.
fn indirect_callee_type(
    sem: &Semantics,
    param_types: &IndexMap<String, TypeId>,
    callee: &Expr,
) -> Option<TypeId> {
    if let Expr::Ident(ident) = callee
        && let Some(type_id) = param_types.get(&ident.name)
    {
        return Some(*type_id);
    }
    expr_type_of(callee, sem)
}

/// The static dispatches recorded at a call, with whether each spells its
/// receiver as the first argument.
fn dispatches_at(annotations: Option<&TypeAnnotations>, id: AstId) -> Vec<(FunctionRef, bool)> {
    annotations
        .into_iter()
        .flat_map(|ann| ann.static_dispatches(id))
        .map(|dispatch| (dispatch.function_ref.clone(), dispatch.self_in_args))
        .collect()
}

/// What invoking one callee at a call site performs.
struct CalleeEffects {
    /// The name a diagnostic gives the callee.
    name: String,
    /// Effects the callee's signature declares.
    declared: Vec<EffectRef>,
    /// The capability dispatching a host-backed operation demands. It belongs
    /// to the path that names the interface, not to the callee's signature, so a
    /// walk may word the two apart.
    dispatched: Vec<EffectRef>,
}

/// Every callee a call at `id` resolves to: a free function, each static
/// dispatch, or the function type of a callee no name reaches.
///
/// Both walks read a call through this, so a spelling either one answers for is
/// a spelling both answer for. A tag call is a call: annotate records its callee
/// under the template's own id, so the same lookup answers for it.
fn call_site_effects(
    sem: &Semantics,
    index: &EffectIndex<'_>,
    annotations: Option<&TypeAnnotations>,
    param_types: &IndexMap<String, TypeId>,
    callee: &Expr,
    id: AstId,
    args: &[Expr],
) -> Vec<CalleeEffects> {
    let bare = |name: String, declared: Vec<EffectRef>| CalleeEffects {
        name,
        declared,
        dispatched: Vec::new(),
    };
    if let Expr::Ident(ident) = callee
        && let Some(def) = sem.referenced_symbol(ident.id)
        && let Some(effects) = index.fn_effects.get(&def)
    {
        let params = index.fn_params.get(&def).cloned().unwrap_or_default();
        let resolved = resolve_effect_params(sem, index, effects, &params, false, args);
        let resolved = resolve_bound_effect_params(sem, index, annotations, def, id, resolved);
        return vec![bare(ident.name.clone(), resolved)];
    }
    let dispatches = dispatches_at(annotations, id);
    if dispatches.is_empty() {
        // The callee is a function-typed value (a closure or `fn(...)`
        // parameter). Its type carries the effects it performs when invoked.
        let Some(callee_type) = indirect_callee_type(sem, param_types, callee) else {
            return Vec::new();
        };
        let ResolvedType::Function { effects, .. } = sem.types.get(callee_type) else {
            return Vec::new();
        };
        return vec![bare(INDIRECT_CALLEE.to_string(), effects.clone())];
    }
    // Only a free function in this program dispatches through the path: a
    // method names its receiver, and a host binding is already the import.
    let path_site = interface_segment(callee).map(|seg| seg.id);
    dispatches
        .into_iter()
        .map(|(func_ref, self_in_args)| {
            let effects = index.method_effects(&func_ref);
            let params = index.method_param_types(&func_ref);
            // A qualified (UFCS) call spells the receiver as its first
            // argument, so the args already align with the callee's full
            // parameter list — no self skip.
            let is_method = func_ref.method_info.is_some() && !self_in_args;
            let dispatches_through_path = func_ref.method_info.is_none()
                && matches!(func_ref.module_source, ModuleSource::Local { .. });
            CalleeEffects {
                name: callee_name(callee).to_string(),
                declared: resolve_effect_params(sem, index, &effects, &params, is_method, args),
                dispatched: if dispatches_through_path {
                    operation_requirements(sem, index, path_site)
                } else {
                    Vec::new()
                },
            }
        })
        .collect()
}

/// Walks a function body, checking that each call's required effects are held.
struct SemEffectWalker<'a> {
    sem: &'a Semantics,
    annotations: Option<&'a TypeAnnotations>,
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
    module_source: ModuleSource,
    out: &'a mut Vec<EffectError>,
}

impl EffectIndex<'_> {
    /// Effects a method dispatch requires: the callee's declared effects plus,
    /// for a direct (non-trait) method on a `resource`, the resource effect.
    fn method_effects(&self, func_ref: &FunctionRef) -> Vec<EffectRef> {
        // A trait method's `with` clause bounds every impl, and a bound
        // dispatch has no impl to read: the declaration is what a call requires.
        let mut effects = match self.declared_by_trait(func_ref) {
            Some(declared) => declared.to_vec(),
            None => self
                .mangled_index
                .get(&(func_ref.module_source.clone(), func_ref.name.clone()))
                .cloned()
                .unwrap_or_default(),
        };
        effects = self.resolve_open_head(func_ref, effects);
        if let Some(method_info) = &func_ref.method_info
            && method_info.trait_name.is_none()
        {
            // `resource_names` is keyed by the declaration name, as the
            // `resource` item writes it — a mangled head would carry the
            // declaring module and match nothing, silently dropping the
            // resource requirement instead of reporting it.
            let decl_name = method_info.receiver_decl_name();
            let decl_name = decl_name.into_string();
            let resource_key = (func_ref.module_source.clone(), decl_name.clone());
            if self.resource_names.contains(&resource_key) {
                let resource_effect = EffectRef::Concrete {
                    name: decl_name,
                    module_source: func_ref.module_source.clone(),
                };
                if !effects.contains(&resource_effect) {
                    effects.push(resource_effect);
                }
            }
        }
        effects
    }

    /// An open head's effect parameter, resolved against the receiver the
    /// dispatch names. A receiver that is a type parameter names no impl.
    fn resolve_open_head(&self, func_ref: &FunctionRef, effects: Vec<EffectRef>) -> Vec<EffectRef> {
        if !effects.iter().any(EffectRef::is_param) {
            return effects;
        }
        let Some(method_info) = func_ref.method_info.as_ref() else {
            return effects;
        };
        let Some(key) = method_info
            .trait_name
            .as_ref()
            .and_then(|trait_name| impl_key(&method_info.fq_base_struct_name(), trait_name))
        else {
            return effects;
        };
        let Some(declared) = self.impl_effects.get(&key) else {
            return effects;
        };
        let brought = self.close_over_args(declared, &key.1, &method_info.struct_type_args, 0);
        substitute_effect_param(effects, &brought)
    }

    /// What an impl brings once its own effect parameter is filled from the
    /// receiver's type arguments. An argument implementing nothing leaves it.
    fn close_over_args(
        &self,
        declared: &[EffectRef],
        trait_key: &TraitKey,
        args: &[FqTypeName],
        depth: u32,
    ) -> IndexSet<EffectRef> {
        /// What stops a cyclic instantiation. Deeper than real nesting goes.
        const MAX_DEPTH: u32 = 8;

        let mut out: IndexSet<EffectRef> = IndexSet::default();
        for effect in declared {
            if !effect.is_param() || depth == MAX_DEPTH {
                out.insert(effect.clone());
                continue;
            }
            let mut filled = false;
            for arg in args {
                let Some(inner) = self.impl_effects.get(&(arg.head_only(), trait_key.clone()))
                else {
                    continue;
                };
                filled = true;
                out.extend(self.close_over_args(inner, trait_key, arg.args(), depth + 1));
            }
            if !filled {
                out.insert(effect.clone());
            }
        }
        out
    }

    /// The effects the trait method behind a dispatch declares. `None` where
    /// the dispatch names no trait, or names an `interface` or a `resource`.
    fn declared_by_trait(&self, func_ref: &FunctionRef) -> Option<&[EffectRef]> {
        let method_info = func_ref.method_info.as_ref()?;
        self.effects_declared_by(method_info.trait_name.as_ref()?, &method_info.method_name)
    }

    /// The effects one trait method declares. `None` for a name no trait
    /// declares, an `interface` operation, or a `resource` method.
    fn effects_declared_by(&self, trait_name: &FqTraitName, method: &str) -> Option<&[EffectRef]> {
        let module = trait_name.module()?;
        self.trait_method_effects
            .get(&(
                module.clone(),
                trait_name.base_name().to_string(),
                method.to_string(),
            ))
            .map(Vec::as_slice)
    }

    /// Parameter type ids for a method / static dispatch target.
    fn method_param_types(&self, func_ref: &FunctionRef) -> Vec<TypeId> {
        self.mangled_params
            .get(&(func_ref.module_source.clone(), func_ref.name.clone()))
            .cloned()
            .unwrap_or_default()
    }
}

/// Resolve `EffectRef::Param` effects to concrete effects by matching the
/// callee's function-typed parameters against the actual argument types.
/// `is_method` drops the leading `self` parameter so params line up with
/// `args`.
///
/// Both walks resolve before they read a callee's effects, so an `effect E`
/// bound to a concrete effect at the call site is seen by each of them.
fn resolve_effect_params(
    sem: &Semantics,
    index: &EffectIndex<'_>,
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
    // `None` until an argument determines the parameter. An argument that
    // determines it to be pure leaves an empty set, which is not the same
    // answer as never having been determined.
    let mut concrete: IndexMap<String, Option<IndexSet<EffectRef>>> =
        param_names.iter().map(|n| (n.clone(), None)).collect();
    let type_table = &sem.types;
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
        let Some(arg_type) = sem.expression_type(arg.id()) else {
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
                && let Some(slot) = concrete.get_mut(name)
            {
                let set = slot.get_or_insert_with(IndexSet::default);
                for a in actual {
                    set.insert(a.clone());
                }
            }
        }
    }
    let mut resolved = Vec::new();
    for effect in callee_effects {
        match effect {
            // A parameter no argument determined — a trait bound's, say —
            // stays the requirement, so only a caller holding it satisfies it.
            EffectRef::Param { name } => match concrete.get(name).and_then(Option::as_ref) {
                Some(set) => resolved.extend(expand_through_closure(set, index.closure)),
                None => resolved.push(effect.clone()),
            },
            EffectRef::Concrete { .. } => resolved.push(effect.clone()),
        }
    }
    resolved
}

/// An effect parameter the callee's trait bounds leave open, resolved against
/// the types the call instantiates them with. An unreached impl leaves it.
fn resolve_bound_effect_params(
    sem: &Semantics,
    index: &EffectIndex<'_>,
    annotations: Option<&TypeAnnotations>,
    callee: AstId,
    site: AstId,
    effects: Vec<EffectRef>,
) -> Vec<EffectRef> {
    if !effects.iter().any(EffectRef::is_param) {
        return effects;
    }
    let Some(slots) = index.fn_bound_traits.get(&callee) else {
        return effects;
    };
    let mut brought: IndexSet<EffectRef> = IndexSet::default();
    let mut resolved_any = false;
    let instantiations = annotations
        .into_iter()
        .flat_map(|ann| ann.all(|facts| &facts.generic_instantiations, site));
    for instantiation in instantiations {
        for (slot, traits) in slots.iter().enumerate() {
            let Some(&type_arg) = instantiation.type_args.get(slot) else {
                continue;
            };
            let head = sem.types.fq_base_type_name(type_arg).head_only();
            for key in traits.iter().filter(|key| index.open_traits.contains(*key)) {
                let Some(declared) = index.impl_effects.get(&(head.clone(), key.clone())) else {
                    continue;
                };
                resolved_any = true;
                brought.extend(declared.iter().cloned());
            }
        }
    }
    if !resolved_any {
        return effects;
    }
    substitute_effect_param(effects, &brought)
}

/// Replace every effect parameter with what the impl behind it brings.
fn substitute_effect_param<'a>(
    effects: Vec<EffectRef>,
    brought: impl IntoIterator<Item = &'a EffectRef> + Copy,
) -> Vec<EffectRef> {
    let mut out: IndexSet<EffectRef> = IndexSet::default();
    for effect in effects {
        if effect.is_param() {
            out.extend(brought.into_iter().cloned());
        } else {
            out.insert(effect);
        }
    }
    out.into_iter().collect()
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
    fn binding_granted_effects(&self, binding: &EffectHandlerBinding) -> Vec<EffectRef> {
        binding_granted_effects(
            self.sem,
            self.annotations,
            self.index,
            &self.module_source,
            binding,
        )
    }

    fn report_missing(&mut self, effects: &[EffectRef], callee: &str, span: Span) {
        for effect in effects {
            // Canonicalise: `EffectRef::Concrete.module_source` reflects the
            // recording module's import perspective (a user `with Stdout`
            // records `Stdout` against the entry module, while stdlib records
            // it against `wasi:cli`), so compare through the declaration's
            // canonical form rather than by raw `module_source`.
            let effect = canonicalize_effect(effect, self.index.closure, self.index.effect_by_name);
            if effect.is_param() {
                // An undetermined parameter stands for whatever the callee
                // brings, so only a caller with one of its own forwards it.
                if !self.current.iter().any(EffectRef::is_param) {
                    self.out.push(EffectError {
                        callee: callee.to_string(),
                        missing_effect: effect.name().to_string(),
                        fault: EffectFault::MissingOpen,
                        span,
                        module: self.module.clone(),
                    });
                }
                continue;
            }
            if self.current.contains(&effect) {
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
                fault: EffectFault::Missing(kind),
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
        // One fact per walk that reached the loop: an inner `for-of` inside a
        // tuple `for-of` body is walked once per outer element.
        if let Stmt::ForOf(for_of) = stmt {
            let iterators: Vec<ForOfIteratorInfo> = self
                .annotations
                .into_iter()
                .flat_map(|ann| ann.all(|facts| &facts.for_of_iterator, for_of.id))
                .cloned()
                .collect();
            for info in &iterators {
                for func_ref in [&info.into_iter, &info.next] {
                    let effects = self.index.method_effects(func_ref);
                    let callee = func_ref
                        .method_info
                        .as_ref()
                        .map_or(func_ref.name.as_str(), |m| m.method_name.as_str());
                    self.report_missing(&effects, callee, for_of.span);
                }
            }
        }
        ast::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Call(call) => {
                self.check_call_effects(&call.callee, call.id, &call.args, call.span);
            }
            Expr::TaggedTemplate(tagged) => {
                self.check_call_effects(&tagged.tag, tagged.id, &[], tagged.span);
            }
            Expr::MethodCall(method_call) => {
                let sem = self.sem;
                for dispatch in sem.method_dispatches_at(method_call.id) {
                    let func_ref = dispatch.function_ref.clone();
                    let effects = self.method_effects(&func_ref);
                    let params = self.method_param_types(&func_ref);
                    let resolved = resolve_effect_params(
                        self.sem,
                        self.index,
                        &effects,
                        &params,
                        true,
                        &method_call.args,
                    );
                    self.report_missing(&resolved, &method_call.method, method_call.span);
                }
            }
            Expr::StaticMethodCall(static_call) => {
                for (func_ref, self_in_args) in dispatches_at(self.annotations, static_call.id) {
                    let effects = self.method_effects(&func_ref);
                    let params = self.method_param_types(&func_ref);
                    // See the `Call` arm: a trait-turbofish qualified call
                    // carries its receiver in the argument list.
                    let is_method = func_ref.method_info.is_some() && !self_in_args;
                    let resolved = resolve_effect_params(
                        self.sem,
                        self.index,
                        &effects,
                        &params,
                        is_method,
                        &static_call.args,
                    );
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

    /// Report the effects every callee the call at `id` resolves to performs,
    /// and the capability its path demands where it dispatches an operation.
    fn check_call_effects(&mut self, callee: &Expr, id: AstId, args: &[Expr], span: Span) {
        let sites = call_site_effects(
            self.sem,
            self.index,
            self.annotations,
            &self.param_types,
            callee,
            id,
            args,
        );
        for site in sites {
            self.report_missing(&site.declared, &site.name, span);
            self.report_missing(&site.dispatched, &site.name, span);
        }
    }
}

/// Type of `expr`, preferring the type of the binding an identifier names —
/// where a parameter or a function-typed local has one and the use site does not.
fn expr_type_of(expr: &Expr, sem: &Semantics) -> Option<TypeId> {
    if let Expr::Ident(ident) = expr
        && let Some(def) = sem.referenced_symbol(ident.id)
        && let Some(ty) = sem.local_type(def)
    {
        return Some(ty);
    }
    sem.expression_type(expr.id())
}

// ---------------------------------------------------------------------------
// Semantics-based purity checking
// ---------------------------------------------------------------------------

/// Purity over [`Semantics`]: no default expression or global initializer
/// performs an effect its own handlers do not answer.
#[must_use]
pub fn check_purity_semantic(sem: &Semantics) -> Vec<PurityError> {
    let mut out = Vec::new();
    let Some(state) = sem.state.as_ref() else {
        return out;
    };
    let data = OwnedEffectData::build(sem, state, IndexSet::default());
    run_purity_checks(sem, &data.index(), &mut out);
    out
}

/// Walk every user-authored expression that must be pure, appending violations.
/// Shared by [`check_purity_semantic`] and [`check_semantics`].
fn run_purity_checks(sem: &Semantics, index: &EffectIndex, out: &mut Vec<PurityError>) {
    let Some(state) = sem.state.as_ref() else {
        return;
    };
    for (src, module) in &sem.modules {
        if !is_user_authored(src) {
            continue;
        }
        let mut walker = PurityWalker {
            sem,
            annotations: state.module_semantics.get(src).map(|m| &m.types),
            index,
            module_source: src,
            context: PureContext::DefaultValue,
            granted: IndexSet::default(),
            param_types: IndexMap::default(),
            out: &mut *out,
        };
        for item in &module.items {
            match item {
                Item::Function(func) => walker.check_defaults(&func.params),
                Item::Impl(impl_block) => {
                    for method in &impl_block.methods {
                        walker.check_defaults(&method.params);
                    }
                }
                Item::Trait(trait_decl) => {
                    // The trait method signature path does not yet resolve param
                    // defaults, so their calls leave no `references` edge to flag
                    // until that annotation lands.
                    for method in &trait_decl.methods {
                        walker.check_defaults(&method.params);
                    }
                }
                Item::Interface(interface_decl) => {
                    for method in &interface_decl.methods {
                        walker.check_defaults(&method.params);
                    }
                }
                Item::Resource(resource_decl) => {
                    for method in &resource_decl.methods {
                        walker.check_defaults(&method.params);
                    }
                }
                Item::Struct(struct_decl) => {
                    for field in &struct_decl.fields {
                        if let Some(default) = &field.default {
                            walker.check(PureContext::DefaultValue, default);
                        }
                    }
                }
                Item::Global(global) => {
                    walker.check(PureContext::GlobalInitializer, &global.initializer);
                }
                // No expression a position requires to be pure.
                Item::Use(_)
                | Item::Enum(_)
                | Item::Variant(_)
                | Item::Flags(_)
                | Item::Newtype(_)
                | Item::TupleTypeDecl(_)
                | Item::BuiltinTypeDecl(_)
                | Item::World(_)
                | Item::Test(_)
                | Item::Error(_) => {}
            }
        }
    }
}

/// Walks an expression that must be pure, flagging every effect it performs
/// that no enclosing `with … do` answers.
struct PurityWalker<'a> {
    sem: &'a Semantics,
    annotations: Option<&'a TypeAnnotations>,
    index: &'a EffectIndex<'a>,
    module_source: &'a ModuleSource,
    context: PureContext,
    /// Effects the enclosing `with … do` installs, which a callee declaring
    /// one may demand of the position.
    granted: IndexSet<EffectRef>,
    /// Names the callee of an indirect call through a function-typed parameter.
    /// Always empty: a global initializer has no enclosing function, and a
    /// default expression cannot name a parameter — the call site evaluates it
    /// before any is bound.
    param_types: IndexMap<String, TypeId>,
    out: &'a mut Vec<PurityError>,
}

impl PurityWalker<'_> {
    fn check(&mut self, context: PureContext, expr: &Expr) {
        self.context = context;
        self.visit_expr(expr);
    }

    fn check_defaults(&mut self, params: &[ast::Param]) {
        for param in params {
            if let Some(default) = &param.default {
                self.check(PureContext::DefaultValue, default);
            }
        }
    }

    fn flag(&mut self, impurity: Impurity, span: Span) {
        self.out.push(PurityError {
            context: self.context,
            impurity,
            span,
            module: self.module_source.source_path(),
        });
    }

    /// Whether any of `effects` is one no enclosing `with … do` installs.
    fn unanswered(&self, effects: &[EffectRef]) -> bool {
        effects.iter().any(|effect| {
            let effect = canonicalize_effect(effect, self.index.closure, self.index.effect_by_name);
            // A `Param` left after resolution stands for effects no handler
            // here can have installed, so it is unanswered like any other.
            effect.is_param() || !self.granted.contains(&effect)
        })
    }

    fn flag_if_effectful(
        &mut self,
        effects: &[EffectRef],
        params: &[TypeId],
        is_method: bool,
        args: &[Expr],
        callee: &str,
        span: Span,
    ) {
        let effects = resolve_effect_params(self.sem, self.index, effects, params, is_method, args);
        if self.unanswered(&effects) {
            self.flag(Impurity::Call(callee.to_string()), span);
        }
    }

    /// Flags `Site::op(…)` when the dispatch demands a capability the position
    /// does not hold. An operation declares no `with` clause of its own, so
    /// nothing but the site says so.
    fn flag_if_operation(&mut self, site: AstId, op: &str, span: Span) {
        // An operation declares no effect parameters, so there is nothing for
        // the arguments to resolve.
        let required = operation_requirements(self.sem, self.index, Some(site));
        if self.unanswered(&required) {
            self.flag(Impurity::Dispatch(op.to_string()), span);
        }
    }

    /// Flags every callee the call at `id` resolves to whose effects the
    /// position does not hold.
    fn flag_call(&mut self, callee: &Expr, id: AstId, args: &[Expr], span: Span) {
        let sites = call_site_effects(
            self.sem,
            self.index,
            self.annotations,
            &self.param_types,
            callee,
            id,
            args,
        );
        // `dispatched` is left to `flag_if_operation`, which asks the path
        // rather than each dispatch and so answers once per site.
        for site in sites {
            if self.unanswered(&site.declared) {
                self.flag(Impurity::Call(site.name), span);
            }
        }
    }
}

impl AstVisitor for PurityWalker<'_> {
    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Call(call) => {
                if let Some(interface) = interface_segment(&call.callee)
                    && let Expr::Ident(ident) = &call.callee
                    && let Some(op) = ident.segments.last()
                {
                    self.flag_if_operation(interface.id, &op.name, call.span);
                }
                self.flag_call(&call.callee, call.id, &call.args, call.span);
            }
            Expr::TaggedTemplate(tagged) => {
                self.flag_call(&tagged.tag, tagged.id, &[], tagged.span);
            }
            Expr::MethodCall(method_call) => {
                let sem = self.sem;
                for dispatch in sem.method_dispatches_at(method_call.id) {
                    let effects = self.index.method_effects(&dispatch.function_ref);
                    let params = self.index.method_param_types(&dispatch.function_ref);
                    self.flag_if_effectful(
                        &effects,
                        &params,
                        true,
                        &method_call.args,
                        &method_call.method,
                        method_call.span,
                    );
                }
            }
            Expr::StaticMethodCall(static_call) => {
                if let ast::Type::Named(named) = &static_call.target_type {
                    self.flag_if_operation(named.id, &static_call.method, static_call.span);
                }
                for (func_ref, self_in_args) in dispatches_at(self.annotations, static_call.id) {
                    let effects = self.index.method_effects(&func_ref);
                    let params = self.index.method_param_types(&func_ref);
                    let is_method = func_ref.method_info.is_some() && !self_in_args;
                    self.flag_if_effectful(
                        &effects,
                        &params,
                        is_method,
                        &static_call.args,
                        &static_call.method,
                        static_call.span,
                    );
                }
            }
            Expr::WithHandler(with_handler) => {
                // The install discharges what its body dispatches, so the body
                // walks under the grant. The handler expressions run outside it.
                for binding in &with_handler.handlers {
                    ast::walk_expr(self, &binding.handler);
                }
                let added: Vec<EffectRef> = with_handler
                    .handlers
                    .iter()
                    .flat_map(|binding| {
                        binding_granted_effects(
                            self.sem,
                            self.annotations,
                            self.index,
                            self.module_source,
                            binding,
                        )
                    })
                    .filter(|effect| self.granted.insert(effect.clone()))
                    .collect();
                ast::walk_block(self, &with_handler.body);
                for effect in added {
                    self.granted.shift_remove(&effect);
                }
                return;
            }
            _ => {}
        }
        ast::walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler_host::Diagnostic;

    #[test]
    fn test_effect_error_display() {
        let error = EffectError {
            callee: "println".to_string(),
            missing_effect: "Stdout".to_string(),
            fault: EffectFault::Missing(EffectKind::Effect),
            span: Span {
                start: 100,
                end: 107,
                line: 10,
                column: 5,
                end_line: 10,
                end_column: 12,
                ..Span::default()
            },
            module: "example/hello.wado".to_string(),
        };
        let diag = Diagnostic::from(error);
        assert_eq!(
            diag.message,
            "missing effect 'Stdout' required by 'println'"
        );
        assert_eq!(diag.span.expect("span").file, "example/hello.wado");
    }
}
