//! Effect and purity checking for Wado (Design B): that every call holds the
//! effects its callee requires, that defaults, global initializers and
//! `contract` clauses are pure, and that a clause writes only the values it
//! creates.
//! Both read [`Semantics`] rather than the emitted TIR, so they see every
//! source function and run on the LSP path. Violations are returned.

use std::cell::RefCell;
use std::rc::Rc;

use crate::attribute::AMBIENT;
use crate::hashmap::{IndexMap, IndexSet};
use crate::{Lints, demanded_imports};

use crate::component_model::Demand;
use crate::module_source::ModuleSource;
use crate::name::{
    DeclaredHead, FqTraitName, FqTypeName, TypeHead, is_wasi_fq, mangle_local_method,
};
use crate::resolve::{Resolution, Resolutions};
use crate::tir::{
    EffectRef, EffectRows, FunctionRef, ResolvedType, RowFlow, TemplateId, TypeId, solve_row,
};
use crate::token::Span;
use crate::type_table::{TypeSet, TypeTable};

use crate::ast::{
    self, AstId, AstVisitor, Attribute, Block, CmImport, EffectHandlerBinding, Expr, Function,
    ImplBlock, Item, Pattern, RestClause, Stmt, TestDecl, cm_import_of,
};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::compiler_item::CompilerItem;
use crate::defs::{DefId, DefKind};
use crate::elaborator::ImplParamSlots;
use crate::elaborator::orchestration::AnnotateState;
use crate::elaborator::sem::types::{ForOfIteratorInfo, ImplFacts, TypeAnnotations};
use crate::semantics::Semantics;
use crate::unparse::unparse_expr_source;
use crate::write_sites::{WriteSite, for_each_write_site};

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
    /// The caller does not hold an import of the component its callee is in,
    /// which demands every import for recording none of what it demands.
    MissingUnrecorded(EffectKind),
    /// A `with` installs a handler whose methods perform what the installing
    /// function does not hold. `callee` names the effect handled.
    MissingForHandler(EffectKind),
    /// An `impl` method declares an effect its trait method leaves out.
    UndeclaredByTrait,
    /// The callee's effects are left open, and the caller forwards none.
    MissingOpen,
    /// An `#[ambient]` argument names no effect in the function's module.
    UnknownAmbient,
    /// A function value performs an effect the `fn` type it is moved into
    /// does not declare.
    BeyondExpectedType(EffectKind),
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
        let (code, message) = match e.fault {
            EffectFault::Missing(kind) => (
                Code::MissingEffect,
                format!(
                    "missing {} '{}' required by '{}'",
                    kind.noun(),
                    e.missing_effect,
                    e.callee
                ),
            ),
            EffectFault::MissingUnrecorded(kind) => (
                Code::MissingEffect,
                format!(
                    "missing {} '{}' required by '{}': its component records none of what its \
                     exports demand, so each of its `wasi:*` imports is demanded (a Wado \
                     library records it in a `{}` section)",
                    kind.noun(),
                    e.missing_effect,
                    e.callee,
                    demanded_imports::SECTION_NAME,
                ),
            ),
            EffectFault::MissingForHandler(kind) => (
                Code::MissingEffect,
                format!(
                    "missing {} '{}' required by the handler installed for '{}'",
                    kind.noun(),
                    e.missing_effect,
                    e.callee
                ),
            ),
            EffectFault::UndeclaredByTrait => (
                Code::EffectNotInTrait,
                format!(
                    "effect '{}' is not declared by trait method '{}'",
                    e.missing_effect, e.callee
                ),
            ),
            EffectFault::MissingOpen => (
                Code::MissingEffect,
                format!(
                    "missing effects required by '{}': its trait leaves them to the impl, so declare `with _`",
                    e.callee
                ),
            ),
            EffectFault::UnknownAmbient => (
                Code::UnknownType,
                format!(
                    "`#[ambient({})]` on '{}' names no effect in scope",
                    e.missing_effect, e.callee
                ),
            ),
            EffectFault::BeyondExpectedType(kind) => (
                Code::MissingEffect,
                format!(
                    "this function value performs {} '{}', which the function type it is expected to be does not declare",
                    kind.noun(),
                    e.missing_effect,
                ),
            ),
        };
        Diagnostic {
            severity: Severity::Error,
            code,
            message,
            span: Some(DiagnosticSpan::from_span(&e.span, Some(&e.module))),
        }
    }
}

/// A handler scope that leaves out operations its body reaches through the
/// `extends` family of a resource it handles. Those reach the next handler out
/// or the host, which a test double installing the scope rarely means.
#[derive(Debug, Clone)]
pub struct UnhandledOperation {
    /// The resources the scope handles whose families the body reaches into.
    pub handled: Vec<String>,
    /// The resources declaring the operations no handler in scope answers.
    pub missing: Vec<String>,
    pub span: Span,
    pub module: String,
}

impl From<UnhandledOperation> for Diagnostic {
    fn from(w: UnhandledOperation) -> Self {
        Diagnostic {
            severity: Severity::Warning,
            code: Code::UnhandledOperation,
            message: format!(
                "the handlers installed here leave out `{}`, whose operations the body reaches \
                 through the `extends` family of `{}`, so those calls reach the next handler out \
                 or the host; install a handler for each, or write `#[allow({})]` \
                 if that is deliberate",
                w.missing.join("`, `"),
                w.handled.join("`, `"),
                ast::lint::UNHANDLED_OPERATION,
            ),
            span: Some(DiagnosticSpan::from_span(&w.span, Some(&w.module))),
        }
    }
}

/// One call a body makes, as what it reaches: the operations it dispatches
/// itself and the function whose body it runs.
struct Reach {
    /// The effects whose operations the call dispatches, or, for a function
    /// value, the effects its type says it performs.
    owners: Vec<EffectRef>,
    /// The function called, whose own reach the call's includes.
    callee: Option<AstId>,
    /// What the callee's type declares, standing for its reach where no body
    /// was walked: a function value, or a host binding.
    declared: Vec<EffectRef>,
    /// What a handler installed between the call and the place it is counted
    /// at answers, so it reaches no further.
    answered: IndexSet<EffectRef>,
}

impl Reach {
    /// The effects whose operations the call reaches, given what each walked
    /// body reaches.
    fn reaches<'a>(
        &'a self,
        summaries: &'a IndexMap<AstId, IndexSet<EffectRef>>,
    ) -> impl Iterator<Item = &'a EffectRef> {
        let body = self.callee.and_then(|callee| summaries.get(&callee));
        let declared = if body.is_some() {
            &[][..]
        } else {
            &self.declared[..]
        };
        self.owners
            .iter()
            .chain(declared)
            .chain(body.into_iter().flatten())
            .filter(|effect| !self.answered.contains(*effect))
    }
}

/// A `with … => … do` block, with the calls its body makes.
struct HandlerScope {
    installed: Vec<EffectRef>,
    /// What scopes enclosing this one in the same body install.
    around: IndexSet<EffectRef>,
    calls: Vec<Reach>,
    span: Span,
}

/// The calls one body makes, and its handler scopes, open and closed.
#[derive(Default)]
struct BodyReach {
    calls: Vec<Reach>,
    open: Vec<HandlerScope>,
    closed: Vec<HandlerScope>,
}

impl BodyReach {
    fn note(&mut self, owners: Vec<EffectRef>, callee: Option<AstId>, declared: &[EffectRef]) {
        if owners.is_empty() && callee.is_none() && declared.is_empty() {
            return;
        }
        // Each scope counts the call past what the scopes nested inside it
        // answer; the body counts it past every scope open around it.
        let mut deeper: IndexSet<EffectRef> = IndexSet::default();
        let reach = |answered| Reach {
            owners: owners.clone(),
            callee,
            declared: declared.to_vec(),
            answered,
        };
        for scope in self.open.iter_mut().rev() {
            scope.calls.push(reach(deeper.clone()));
            deeper.extend(scope.installed.iter().cloned());
        }
        self.calls.push(reach(deeper));
    }
}

/// What every body reaches, gathered across the walks, and the handler scopes
/// to check against it once every body is known.
#[derive(Default)]
struct ReachLog {
    bodies: IndexMap<AstId, Vec<Reach>>,
    /// Each closed scope no `allow` waives, with its module.
    scopes: Vec<(HandlerScope, String)>,
}

/// A position whose expression must be pure. The diagnostic names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PureContext {
    DefaultValue,
    GlobalInitializer,
    /// A build may skip it, so whether its effects happen would depend on the
    /// build.
    Contract,
}

impl PureContext {
    fn noun(self) -> &'static str {
        match self {
            Self::DefaultValue => "default value expression",
            Self::GlobalInitializer => "global initializer",
            Self::Contract => "contract clause",
        }
    }
}

/// Why an expression that must be pure is rejected, as the diagnostic words it.
#[derive(Debug, Clone)]
pub enum Impurity {
    /// The named callee declares an effect.
    Call(String),
    /// The named operation demands its interface, which the position does not
    /// hold.
    Dispatch(String),
    /// The position installs a handler for the named effect whose methods
    /// declare an effect it does not hold.
    Install(String),
    /// A `contract` clause writes, or lends a callee the means to write, state
    /// it did not create, reached from the named binding.
    Mutate(String),
    /// A `contract` clause calls the named function value, which may write
    /// through a `&mut` it captured, `fn` and `fn mut` alike.
    CallValue(String),
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
        let noun = e.context.noun();
        let (code, message) = match &e.impurity {
            Impurity::Call(callee) => (
                Code::EffectNotAllowed,
                format!("{noun} cannot perform effects, but calls effectful function '{callee}'"),
            ),
            Impurity::Dispatch(op) => (
                Code::EffectNotAllowed,
                format!(
                    "{noun} cannot perform effects, but dispatches '{op}', which needs a \
                     capability the position does not hold"
                ),
            ),
            Impurity::Install(effect) => (
                Code::EffectNotAllowed,
                format!(
                    "{noun} cannot perform effects, but installs a handler for '{effect}' \
                     whose methods need a capability the position does not hold"
                ),
            ),
            Impurity::Mutate(name) => (
                Code::ContractWrite,
                format!("{noun} cannot mutate '{name}', which reaches state it did not create"),
            ),
            Impurity::CallValue(name) => (
                Code::ContractWrite,
                format!("{noun} cannot call '{name}', which may write what it captured"),
            ),
        };
        Diagnostic {
            severity: Severity::Error,
            code,
            message,
            span: Some(DiagnosticSpan::from_span(&e.span, Some(&e.module))),
        }
    }
}

/// Every declaration's members, by the declaration: a struct's fields, a
/// variant's case payloads.
#[derive(Default)]
struct MemberTables(IndexMap<DefId, Vec<TypeId>>);

impl MemberTables {
    fn collect(sem: &Semantics, state: &AnnotateState) -> Self {
        let defs = state.tysys.resolutions.defs();
        let mut tables = Self::default();
        for (src, module) in &sem.modules {
            let annotations = state.module_semantics.get(src).map(|m| &m.types);
            for item in &module.items {
                if let Item::Struct(struct_decl) = item
                    && let Some(field_types) =
                        annotations.and_then(|ann| ann.struct_field_types.get(&struct_decl.id))
                {
                    tables
                        .0
                        .insert(defs.def_at(struct_decl.id), field_types.clone());
                }
            }
        }
        for (&def, info) in &state.tysys.data.variant_cases {
            tables
                .0
                .insert(def, info.cases.iter().map(|case| case.payload).collect());
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
        let type_args = tt.nominal_type_args(type_id).unwrap_or_default();
        let decl = tt.nominal_def(type_id);
        decl.and_then(|def| self.0.get(&def))
            .into_iter()
            .flatten()
            .map(move |&member| match tt.get(member) {
                // The declaration's own type binds none of its parameters.
                ResolvedType::TypeParam { id, .. }
                    if Some(id.owner) == decl && !type_args.is_empty() =>
                {
                    type_args[id.index as usize]
                }
                _ => member,
            })
    }
}

/// What collecting the resources a type references reads.
struct ResourceScan<'a> {
    tt: &'a TypeTable,
    resolutions: &'a Resolutions,
    members: &'a MemberTables,
}

impl ResourceScan<'_> {
    /// Collect into `out` the effect of every resource `type_id` references,
    /// through containers, signatures and members. `visited` stops at a cycle.
    fn collect(&self, type_id: TypeId, out: &mut IndexSet<EffectRef>, visited: &mut TypeSet) {
        if !visited.insert(type_id) {
            return;
        }
        if let Some(def) = self.tt.resource_def_of(type_id) {
            out.insert(resource_effect(self.resolutions, def));
        }
        match self.tt.get(type_id) {
            ResolvedType::Struct { type_args, .. } | ResolvedType::Variant { type_args, .. } => {
                for ta in type_args {
                    self.collect(*ta, out, visited);
                }
                for member in self.members.of(type_id, self.tt) {
                    self.collect(member, out, visited);
                }
            }
            ResolvedType::Resource {
                type_args: elems, ..
            }
            | ResolvedType::Tuple(elems) => {
                for elem in elems {
                    self.collect(*elem, out, visited);
                }
            }
            ResolvedType::Ref(t) | ResolvedType::MutRef(t) | ResolvedType::BuiltinArray(t) => {
                self.collect(*t, out, visited);
            }
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                for p in params {
                    self.collect(*p, out, visited);
                }
                self.collect(*return_type, out, visited);
            }
            ResolvedType::Newtype { base_type, .. } => {
                self.collect(*base_type, out, visited);
            }
            // Primitives, Unit, Never, Enum, Flags, TypeParam, TypePack,
            // AssocTypeProjection, Unknown, Error — no resource refs.
            _ => {}
        }
    }
}

fn resource_effect(resolutions: &Resolutions, def: DefId) -> EffectRef {
    resolutions
        .effect_decl(def)
        .expect("a resource is an effect")
}

// ---------------------------------------------------------------------------
// Semantics-based effect checking (Design B, Phase 1b)
// ---------------------------------------------------------------------------

/// Effect checking over [`Semantics`], run after `annotate_bodies` so it sees
/// every function, dead or live. Covers free-function, method, and static
/// dispatch with resource injection, the effect / resource propagation closure,
/// effect-parameter resolution, `#[ambient]`,
/// handler-scope grants, and closure calls, over user-authored modules.
#[must_use]
pub fn check_effects_semantic(sem: &Semantics) -> Vec<EffectError> {
    let mut out = Vec::new();
    let Some(state) = sem.state.as_ref() else {
        return out;
    };
    let data = OwnedEffectData::build(sem, state, IndexSet::default());
    run_effect_checks(
        sem,
        &data.index(&sem.types),
        &mut out,
        &mut EffectRows::default(),
        Lints::Off,
        &mut Vec::new(),
    );
    out
}

/// Both Design-B semantic diagnostics, computed in one pass that builds the
/// shared `OwnedEffectData` once. Used by the batch driver and the LSP so
/// effect and purity stay in lockstep across both. `library_exports` are the
/// functions a library build exports, each with its module.
#[must_use]
pub fn check_semantics(
    sem: &Semantics,
    provided_import_fqs: IndexSet<String>,
    library_exports: Option<&[(&ModuleSource, &Function)]>,
    lints: Lints,
) -> SemanticDiagnostics {
    let mut diags = SemanticDiagnostics::default();
    let Some(state) = sem.state.as_ref() else {
        return diags;
    };
    let data = OwnedEffectData::build(sem, state, provided_import_fqs);
    let index = data.index(&sem.types);
    run_effect_checks(
        sem,
        &index,
        &mut diags.effects,
        &mut diags.rows,
        lints,
        &mut diags.unhandled,
    );
    run_purity_checks(sem, &index, &mut diags.purity);
    diags.exported_host_interfaces =
        library_exports.map(|exports| exported_host_interfaces(sem, &index, exports));
    diags
}

/// Bundle of the Design-B semantic diagnostics returned by [`check_semantics`].
#[derive(Default)]
pub struct SemanticDiagnostics {
    pub effects: Vec<EffectError>,
    pub purity: Vec<PurityError>,
    /// Warnings: handler scopes leaving out part of a family they handle.
    pub unhandled: Vec<UnhandledOperation>,
    /// What each effect variable the bodies' calls minted was solved to.
    pub rows: EffectRows,
    /// The `wasi:*` interfaces a library's exports hold, for a library build.
    pub exported_host_interfaces: Option<IndexSet<String>>,
}

impl SemanticDiagnostics {
    /// Whether any check produced a violation.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.effects.is_empty() && self.purity.is_empty()
    }
}

/// The standard library is held to the rules it states, so every module is
/// checked but the bindings, whose declarations have no body to read.
fn is_effect_checked(src: &ModuleSource) -> bool {
    !matches!(
        src,
        ModuleSource::Binding { .. } | ModuleSource::Wasm { .. }
    )
}

/// Walk every function / method / trait method with a body, appending effect
/// violations. Shared by [`check_effects_semantic`] and [`check_semantics`].
fn run_effect_checks(
    sem: &Semantics,
    index: &EffectIndex,
    out: &mut Vec<EffectError>,
    solved: &mut EffectRows,
    lints: Lints,
    warnings: &mut Vec<UnhandledOperation>,
) {
    let mut log = ReachLog::default();
    let log = &mut log;
    for (src, module) in &sem.modules {
        if !is_effect_checked(src) {
            continue;
        }
        let module_waives = !lints.reports_in(src, module)
            || ast::inner_attrs_allow(&module.inner_attributes, ast::lint::UNHANDLED_OPERATION);
        let allows = |attrs: &[ast::Attribute]| {
            module_waives || ast::attrs_allow(attrs, ast::lint::UNHANDLED_OPERATION)
        };
        for item in &module.items {
            match item {
                Item::Function(func) => {
                    let held = Held::Only(&[]);
                    check_function_effects_sem(
                        sem,
                        src,
                        func,
                        index,
                        held,
                        module_waives,
                        out,
                        solved,
                        log,
                    );
                }
                Item::Impl(impl_block) => {
                    let enclosing = allows(&impl_block.attrs);
                    check_impl_effect_conformance(sem, src, impl_block, index, out);
                    let facts = impl_facts(sem, src, impl_block);
                    let trait_key = facts.and_then(|facts| facts.trait_name.as_ref()?.canonical());
                    // A method of a resource's own `impl` is one of its
                    // operations, so its body holds the resource. A handler
                    // builds what its operations hand back, so it holds what
                    // the effect implies; delegating to the effect itself
                    // still declares it.
                    let own_resource: Vec<EffectRef> = facts
                        .filter(|facts| facts.trait_name.is_none())
                        .and_then(|facts| facts.struct_name.head().def())
                        .filter(|&def| index.resolutions.defs().kind(def) == DefKind::Resource)
                        .map(|def| resource_effect(index.resolutions, def))
                        .into_iter()
                        .collect();
                    let mentioned: Option<Vec<EffectRef>> = trait_key
                        .and_then(|key| index.interfaces.get(&key))
                        .map(|(effect, _)| {
                            index
                                .mentions
                                .get(effect)
                                .into_iter()
                                .flatten()
                                .cloned()
                                .collect()
                        });
                    for method in &impl_block.methods {
                        let inherits_ambient = trait_key.is_some_and(|key| {
                            index
                                .ambient_trait_methods
                                .contains(&(key, method.name.clone()))
                        });
                        let held = match &mentioned {
                            _ if inherits_ambient => Held::Every,
                            Some(mentioned) => Held::Mentioned(mentioned),
                            None => Held::Only(&own_resource),
                        };
                        check_function_effects_sem(
                            sem, src, method, index, held, enclosing, out, solved, log,
                        );
                    }
                }
                // A default body holds what the trait declares for it.
                Item::Trait(trait_decl) => {
                    let def = index.resolutions.defs().def_at(trait_decl.id);
                    let enclosing = allows(&trait_decl.attrs);
                    for method in &trait_decl.methods {
                        let declared = &index.trait_method_effects[&(def, method.name.clone())];
                        let held = Held::Only(declared);
                        check_function_effects_sem(
                            sem, src, method, index, held, enclosing, out, solved, log,
                        );
                    }
                }
                // An operation's default body is ordinary code, so what it
                // performs is checked like any other function's. It runs where
                // the operation was dispatched, which held the interface.
                Item::Interface(interface_decl) => {
                    let def = index.resolutions.defs().def_at(interface_decl.id);
                    let own = index.interfaces.get(&def).map(|(effect, _)| effect);
                    let enclosing = allows(&interface_decl.attrs);
                    for method in &interface_decl.methods {
                        let held = Held::Only(own.map_or(&[][..], std::slice::from_ref));
                        check_function_effects_sem(
                            sem, src, method, index, held, enclosing, out, solved, log,
                        );
                    }
                }
                // What the initializer performs is the purity check's; only an
                // unknown name is reported here, as on a function.
                Item::Global(global) => {
                    admitted_effects(&global.name, &global.attributes, src, index, out);
                }
                Item::Test(test) => {
                    check_test_effects_sem(sem, src, test, index, module_waives, out, solved, log);
                }
                _ => {}
            }
        }
    }
    report_unhandled_operations(index, log, warnings);
}

/// Warn at each handler scope whose body reaches an operation of a resource
/// sharing an `extends` family with one the scope handles, where no handler in
/// scope answers it. The family is one effect, so holding it says nothing of
/// which members a test double must stand in for; the calls the double leaves
/// out pass it by.
fn report_unhandled_operations(
    index: &EffectIndex,
    log: &ReachLog,
    warnings: &mut Vec<UnhandledOperation>,
) {
    let family_root: IndexMap<&EffectRef, DefId> = index
        .interfaces
        .iter()
        .filter(|(def, _)| index.resolutions.defs().kind(**def) == DefKind::Resource)
        .map(|(def, (effect, _))| {
            let root = index
                .types
                .resource_chain(*def)
                .last()
                .expect("the chain starts at def");
            (effect, root)
        })
        .collect();
    let summaries = reach_summaries(log);
    for (scope, module) in &log.scopes {
        let answered =
            |effect: &EffectRef| scope.installed.contains(effect) || scope.around.contains(effect);
        let mut handled: IndexSet<String> = IndexSet::default();
        let mut missing: IndexSet<String> = IndexSet::default();
        let reached: IndexSet<&EffectRef> = scope
            .calls
            .iter()
            .flat_map(|call| call.reaches(&summaries))
            .collect();
        for effect in reached.into_iter().filter(|effect| !answered(effect)) {
            let Some(root) = family_root.get(effect) else {
                continue;
            };
            let kin: Vec<&EffectRef> = scope
                .installed
                .iter()
                .filter(|installed| family_root.get(installed) == Some(root))
                .collect();
            if kin.is_empty() {
                continue;
            }
            missing.insert(effect.name().to_string());
            handled.extend(
                kin.into_iter()
                    .map(|installed| installed.name().to_string()),
            );
        }
        if !missing.is_empty() {
            warnings.push(UnhandledOperation {
                handled: handled.into_iter().collect(),
                missing: missing.into_iter().collect(),
                span: scope.span,
                module: module.clone(),
            });
        }
    }
}

/// What each walked body reaches through its calls, transitively, past the
/// handlers it installs itself.
fn reach_summaries(log: &ReachLog) -> IndexMap<AstId, IndexSet<EffectRef>> {
    let mut summaries: IndexMap<AstId, IndexSet<EffectRef>> = log
        .bodies
        .keys()
        .map(|key| (*key, IndexSet::default()))
        .collect();
    loop {
        let mut grew = false;
        for (key, calls) in &log.bodies {
            let reached: IndexSet<EffectRef> = summaries[key]
                .iter()
                .chain(calls.iter().flat_map(|call| call.reaches(&summaries)))
                .cloned()
                .collect();
            if reached.len() != summaries[key].len() {
                summaries[key] = reached;
                grew = true;
            }
        }
        if !grew {
            return summaries;
        }
    }
}

/// A trait method, by the trait's declaration and the method name.
type TraitMethodKey = (DefId, String);

/// One trait impl, by the head of the type it targets and the trait it
/// implements.
type ImplKey = (FqTypeName, DefId);

/// What one `impl Trait for Type`'s methods declare, and its target's
/// arguments as written, rows included: an effect parameter of the impl is
/// the row the receiver's argument has where the target writes it.
struct ImplEffects {
    declared: Vec<EffectRef>,
    target_args: Vec<FqTypeName>,
    /// The impl's bounded parameters, whose impls fill its effect parameter.
    bounds: Rc<[ReceiverBound]>,
}

/// Impl block `def`'s target arguments as written, rows included.
fn spelled_target_args(types: &TypeTable, def: DefId) -> Vec<FqTypeName> {
    types
        .impl_target_args(def)
        .iter()
        .map(|&arg| types.fq_type_name_with_rows(arg))
        .collect()
}

/// The key one `impl Trait for Type` is recorded under. `None` for a trait
/// reaching no declaration.
fn impl_key(struct_name: &FqTypeName, trait_name: &FqTraitName) -> Option<ImplKey> {
    Some((struct_name.head_only(), trait_name.canonical()?))
}

/// The traits bounding each type parameter, by the slot
/// `Scope::register_generic_params` gives it.
fn bound_traits_per_slot(
    type_params: &[ast::GenericParam],
    resolutions: &Resolutions,
) -> Vec<Vec<DefId>> {
    let defs = resolutions.defs();
    type_params
        .iter()
        .filter(|p| p.is_real_type_param())
        .map(|p| {
            p.bounds
                .iter()
                .filter_map(|bound| resolutions.bound_decl(bound))
                .filter(|def| defs.kind(*def) == DefKind::Trait)
                .collect()
        })
        .collect()
}

/// Where a bounded impl parameter sits in the impl's target, and the traits
/// bounding it. The path indexes the receiver's type arguments, then theirs:
/// `T` in `impl<T: Tick> W<List<T>>` is at `[0, 0]`, and `T` in
/// `impl<T: Tick> Show for T` at `[]`, the receiver itself. `None` where no
/// path through type arguments reaches it, as inside a tuple or a reference.
struct ReceiverBound {
    path: Option<Vec<usize>>,
    /// The parameter's position in an instantiation's impl type arguments.
    slot: u32,
    traits: Vec<DefId>,
}

/// An impl whose target is its own bounded parameter, `impl<T: B> Trait for T`:
/// what it brings to a type that has no impl of the trait of its own.
struct BlanketImpl {
    /// What the impl's methods declare.
    declared: Vec<EffectRef>,
    /// The traits bounding the target parameter, each of which brings what the
    /// type's impl of it does.
    bounds: Vec<DefId>,
}

/// Every bounded impl parameter the impl's target names.
fn receiver_bounds(block: &ImplBlock, resolutions: &Resolutions) -> Vec<ReceiverBound> {
    let real: Vec<&ast::GenericParam> = block
        .type_params
        .iter()
        .filter(|p| p.is_real_type_param())
        .collect();
    let slots = ImplParamSlots::of(&block.ty, &block.type_params);
    let walk = ReceiverWalk {
        params: real.iter().map(|p| p.id).collect(),
        slots: real
            .iter()
            .map(|p| {
                slots
                    .of_name(&p.name)
                    .expect("every type parameter of an impl fills a slot")
            })
            .collect(),
        bounds: bound_traits_per_slot(&block.type_params, resolutions),
        resolutions,
    };
    let mut out = Vec::new();
    walk.collect(&block.ty, Some(&mut Vec::new()), &mut out);
    out
}

/// Every declared method that takes `self`, wherever methods are declared.
fn receiver_methods(sem: &Semantics) -> IndexSet<AstId> {
    let mut out = IndexSet::default();
    for module in sem.modules.values() {
        for item in &module.items {
            let methods: &[Function] = match item {
                Item::Impl(block) => &block.methods,
                Item::Trait(decl) => &decl.methods,
                Item::Resource(decl) => &decl.methods,
                Item::Interface(decl) => &decl.methods,
                _ => continue,
            };
            out.extend(
                methods
                    .iter()
                    .filter(|m| {
                        m.params
                            .first()
                            .is_some_and(|p| p.self_kind != ast::SelfKind::None)
                    })
                    .map(|m| m.id),
            );
        }
    }
    out
}

/// The impl's type parameters, by binder, and the traits bounding each.
struct ReceiverWalk<'a> {
    params: Vec<AstId>,
    slots: Vec<u32>,
    bounds: Vec<Vec<DefId>>,
    resolutions: &'a Resolutions,
}

impl ReceiverWalk<'_> {
    /// Records each bounded parameter `ty` names, `path` reaching `ty` itself.
    fn collect(&self, ty: &ast::Type, path: Option<&mut Vec<usize>>, out: &mut Vec<ReceiverBound>) {
        match ty {
            ast::Type::Named(named) => {
                if let Resolution::Binder(binder) = self.resolutions.get(named.id)
                    && let Some(slot) = self.params.iter().position(|p| *p == binder)
                    && !self.bounds[slot].is_empty()
                {
                    out.push(ReceiverBound {
                        path: path.map(|p| p.clone()),
                        slot: self.slots[slot],
                        traits: self.bounds[slot].clone(),
                    });
                }
            }
            ast::Type::Generic(ast::GenericType { args, .. }) => self.collect_args(args, path, out),
            ast::Type::NamespacedGeneric(generic) => self.collect_args(&generic.args, path, out),
            ast::Type::Tuple(elements) => {
                for element in elements {
                    self.collect(element, None, out);
                }
            }
            ast::Type::Reference(inner) | ast::Type::MutReference(inner) => {
                self.collect(inner, None, out);
            }
            ast::Type::Function(function) => {
                for param in &function.params {
                    self.collect(param, None, out);
                }
                self.collect(&function.return_type, None, out);
            }
            ast::Type::TypePackSpread(..) | ast::Type::Infer(_) | ast::Type::Error(_) => {}
        }
    }

    fn collect_args(
        &self,
        args: &[ast::Type],
        mut path: Option<&mut Vec<usize>>,
        out: &mut Vec<ReceiverBound>,
    ) {
        for (i, arg) in args.iter().enumerate() {
            match path.as_deref_mut() {
                Some(path) => {
                    path.push(i);
                    self.collect(arg, Some(path), out);
                    path.pop();
                }
                None => self.collect(arg, None, out),
            }
        }
    }
}

/// What a call's bounded type arguments bring to the callee's open effect
/// parameter.
#[derive(Default)]
struct Brought {
    /// The effects the impls the arguments select perform.
    effects: IndexSet<EffectRef>,
    /// Whether some argument is bounded by a trait leaving its effects open.
    determined: bool,
    /// Whether some such argument names no impl to read (a type parameter of
    /// the caller's, say), which leaves the parameter a requirement.
    open: bool,
}

/// Owns the cross-module effect maps so multiple checks (effects, default
/// purity) can borrow a single [`EffectIndex`] view over them. Assembled once
/// from [`Semantics`] + [`AnnotateState`].
struct OwnedEffectData {
    flows_by_var: IndexMap<String, Vec<usize>>,
    fn_effects: IndexMap<AstId, Vec<EffectRef>>,
    fn_params: IndexMap<AstId, Vec<TypeId>>,
    /// Each function's declared return type, by declaration: where a call's
    /// type shows which variable each of the callee's effect parameters became.
    fn_returns: IndexMap<AstId, TypeId>,
    mangled_index: IndexMap<(ModuleSource, String), Vec<EffectRef>>,
    mangled_params: IndexMap<(ModuleSource, String), Vec<TypeId>>,
    trait_method_effects: IndexMap<TraitMethodKey, Vec<EffectRef>>,
    /// The trait methods whose `#[ambient(_)]` every impl inherits.
    ambient_trait_methods: IndexSet<TraitMethodKey>,
    /// The traits that leave their effects to the impl — a `with _` head, or a
    /// bare one, which reads as the same.
    open_traits: IndexSet<DefId>,
    /// Each trait's transitive supertraits, whose methods a bound on it reaches.
    supertraits: IndexMap<DefId, Vec<DefId>>,
    /// Per type-parameter slot of a function declaration, the traits bounding
    /// it, so a call site can read what its type arguments implement.
    fn_bound_traits: IndexMap<AstId, Vec<Vec<DefId>>>,
    /// The bounded parameters a generic impl's target names, by each method of
    /// the impl, which share one list.
    receiver_bounds: IndexMap<AstId, Rc<[ReceiverBound]>>,
    /// The methods declaring `self`, of an `impl`, a `trait`, a `resource` or
    /// an `interface`. A call's arguments line up with another's parameters
    /// from the first.
    receiver_methods: IndexSet<AstId>,
    /// Every effect an impl's methods declare, for resolving a trait head's
    /// effect hole against the type a call instantiates it with.
    impl_effects: IndexMap<ImplKey, ImplEffects>,
    /// The blanket impls of each trait, `impl<T: B> Trait for T`.
    blanket_impls: IndexMap<DefId, Vec<BlanketImpl>>,
    /// Every effect one `impl` block's methods declare, by the block. What a
    /// handler binding performs when it installs the block.
    block_effects: IndexMap<DefId, IndexSet<EffectRef>>,
    /// The `impl` blocks ending in `..forward`, whose left-out operations reach
    /// the next handler out.
    forwarding_blocks: IndexSet<DefId>,
    resources: IndexSet<EffectRef>,
    drop_is_an_operation: IndexSet<DefId>,
    closure: IndexMap<EffectRef, IndexSet<EffectRef>>,
    mentions: IndexMap<EffectRef, IndexSet<EffectRef>>,
    waiting: IndexSet<EffectRef>,
    /// Each interface and resource declaration's effect and `#[cm]` FQ.
    interfaces: IndexMap<DefId, (EffectRef, Option<String>)>,
    effect_by_cm_fq: IndexMap<String, EffectRef>,
    /// CM interface FQs the consumer satisfies with a provider component, which
    /// discharge a reconstructed host-leaf import.
    provided_import_fqs: IndexSet<String>,
    resolutions: Rc<Resolutions>,
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
        let mut fn_returns: IndexMap<AstId, TypeId> = IndexMap::default();
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
            fn_returns.extend(types.fn_return_types.iter().map(|(&k, &t)| (k, t)));
            // A trait's default method is walked once per impl inheriting it.
            // Its function-typed parameters carry the same rows in each, which
            // is all a call reads off them.
            for facts in module_sem.default_method_facts.values() {
                for (key, params) in &facts.types.fn_param_types {
                    fn_params.entry(*key).or_insert_with(|| params.clone());
                }
                for (&key, &ret) in &facts.types.fn_return_types {
                    fn_returns.entry(key).or_insert(ret);
                }
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

        let resolutions = &*state.tysys.resolutions;
        let defs = resolutions.defs();
        let resource_decls: Vec<_> = sem
            .modules
            .values()
            .flat_map(|module| &module.items)
            .filter_map(|item| match item {
                Item::Resource(resource) => Some(resource),
                _ => None,
            })
            .collect();
        let resources: IndexSet<EffectRef> = resource_decls
            .iter()
            .map(|resource| resource_effect(resolutions, defs.def_at(resource.id)))
            .collect();
        let drop_is_an_operation: IndexSet<DefId> = resource_decls
            .iter()
            .filter(|resource| {
                resource.methods.iter().any(|method| {
                    method.name == RESOURCE_DROP
                        && !method
                            .params
                            .first()
                            .is_some_and(|p| matches!(p.self_kind, ast::SelfKind::Value))
                })
            })
            .map(|resource| defs.def_at(resource.id))
            .collect();

        // Effect / resource propagation closure: holding effect `E` admits the
        // resources `E`'s operations reference (e.g. `Stdout` → `Stream`).
        let members = MemberTables::collect(sem, state);
        let closure = build_propagation_closure_sem(sem, state, &members, Families::Joined);
        let mentions = build_propagation_closure_sem(sem, state, &members, Families::Apart);

        // An empty entry is meaningful: the method exists and grants nothing.
        // A declaration has no body, so `fn_effects` holds nothing for it.
        let mut trait_method_effects: IndexMap<TraitMethodKey, Vec<EffectRef>> =
            IndexMap::default();
        let mut open_traits: IndexSet<DefId> = IndexSet::default();
        let mut supertraits: IndexMap<DefId, Vec<DefId>> = IndexMap::default();
        // The trait methods that write no clause of their own, so the head's
        // effects are theirs. An impl of one of these alone brings what the
        // head resolves to; a method's own clause overrides the head.
        let mut head_methods: IndexSet<TraitMethodKey> = IndexSet::default();
        let mut ambient_trait_methods: IndexSet<TraitMethodKey> = IndexSet::default();
        for (src, module) in &sem.modules {
            for item in &module.items {
                let Item::Trait(trait_decl) = item else {
                    continue;
                };
                let def = defs.def_at(trait_decl.id);
                if trait_decl.head.is_open() {
                    open_traits.insert(def);
                }
                let supers: Vec<DefId> = state.tysys.trait_env.supertrait_decls(&def).collect();
                if !supers.is_empty() {
                    supertraits.insert(def, supers);
                }
                for method in &trait_decl.methods {
                    if ambient_admits_every(&method.attrs) {
                        ambient_trait_methods.insert((def, method.name.clone()));
                    }
                    let effects = method
                        .effects
                        .iter()
                        .map(|effect| resolutions.effect_named(effect, src))
                        .collect();
                    trait_method_effects.insert((def, method.name.clone()), effects);
                    if method.effects_source == ast::EffectsSource::Inherited {
                        head_methods.insert((def, method.name.clone()));
                    }
                }
            }
        }

        let mut fn_bound_traits: IndexMap<AstId, Vec<Vec<DefId>>> = IndexMap::default();
        let mut receiver_bounds_by_method: IndexMap<AstId, Rc<[ReceiverBound]>> =
            IndexMap::default();
        let receiver_methods = receiver_methods(sem);
        let mut impl_effects: IndexMap<ImplKey, ImplEffects> = IndexMap::default();
        let mut blanket_impls: IndexMap<DefId, Vec<BlanketImpl>> = IndexMap::default();
        let mut block_effects: IndexMap<DefId, IndexSet<EffectRef>> = IndexMap::default();
        let mut forwarding_blocks: IndexSet<DefId> = IndexSet::default();
        let mut waiting: IndexSet<EffectRef> = IndexSet::default();
        let async_call = sem.types.compiler_item_def(CompilerItem::AsyncCall);
        for (src, module) in &sem.modules {
            let annotations = state.module_semantics.get(src).map(|m| &m.types);
            for item in &module.items {
                match item {
                    Item::Function(func) if !func.type_params.is_empty() => {
                        fn_bound_traits.insert(
                            func.id,
                            bound_traits_per_slot(&func.type_params, resolutions),
                        );
                    }
                    Item::Impl(block) => {
                        let bounds: Rc<[ReceiverBound]> =
                            receiver_bounds(block, resolutions).into();
                        if !bounds.is_empty() {
                            for method in &block.methods {
                                receiver_bounds_by_method.insert(method.id, Rc::clone(&bounds));
                            }
                        }
                        for method in &block.methods {
                            if !method.type_params.is_empty() {
                                fn_bound_traits.insert(
                                    method.id,
                                    bound_traits_per_slot(&method.type_params, resolutions),
                                );
                            }
                        }
                        let declared: IndexSet<EffectRef> = block
                            .methods
                            .iter()
                            .filter_map(|method| fn_effects.get(&method.id))
                            .flatten()
                            .cloned()
                            .collect();
                        let facts = annotations.and_then(|ann| ann.impl_facts.get(&block.id));
                        let brought_to_head = |trait_key: DefId| -> Vec<EffectRef> {
                            let mut brought: Vec<EffectRef> = Vec::new();
                            for method in &block.methods {
                                if !head_methods.contains(&(trait_key, method.name.clone())) {
                                    continue;
                                }
                                for effect in fn_effects.get(&method.id).into_iter().flatten() {
                                    if !brought.contains(effect) {
                                        brought.push(effect.clone());
                                    }
                                }
                            }
                            brought
                        };
                        // Waiting on an `AsyncCall` demands what its methods declare.
                        if facts.is_some_and(|facts| {
                            facts.trait_name.is_none()
                                && async_call
                                    .is_some_and(|def| facts.struct_name.head().def() == Some(def))
                        }) {
                            waiting.extend(declared.iter().cloned());
                        }
                        if let Some(facts) = facts
                            && let Some(trait_name) = &facts.trait_name
                            && matches!(facts.struct_name.head(), TypeHead::Binder { .. })
                            && let Some((_, trait_key)) = impl_key(&facts.struct_name, trait_name)
                        {
                            let bounds = bounds
                                .iter()
                                .filter(|bound| bound.path.as_deref() == Some(&[]))
                                .flat_map(|bound| bound.traits.iter().copied())
                                .filter(|bound| *bound != trait_key)
                                .collect();
                            blanket_impls
                                .entry(trait_key)
                                .or_default()
                                .push(BlanketImpl {
                                    declared: brought_to_head(trait_key),
                                    bounds,
                                });
                        }
                        if let Some(key) = facts.and_then(|facts| {
                            let trait_name = facts.trait_name.as_ref()?;
                            impl_key(&facts.struct_name, trait_name)
                        }) {
                            let brought = brought_to_head(key.1);
                            let entry = impl_effects.entry(key).or_insert_with(|| ImplEffects {
                                declared: Vec::new(),
                                target_args: spelled_target_args(&sem.types, defs.def_at(block.id)),
                                bounds: Rc::clone(&bounds),
                            });
                            for effect in brought {
                                if !entry.declared.contains(&effect) {
                                    entry.declared.push(effect);
                                }
                            }
                        }
                        let def = defs.def_at(block.id);
                        if block
                            .rest
                            .is_some_and(|rest| rest.kind == RestClause::Forward)
                        {
                            forwarding_blocks.insert(def);
                        }
                        block_effects.insert(def, declared);
                    }
                    _ => {}
                }
            }
        }

        let mut interfaces: IndexMap<DefId, (EffectRef, Option<String>)> = IndexMap::default();
        // Restricted to closure keys, so a host-leaf import resolves to an
        // effect while a type-only interface (`wasi:cli/types`) resolves to
        // nothing.
        let mut effect_by_cm_fq: IndexMap<String, EffectRef> = IndexMap::default();
        for module in sem.modules.values() {
            for item in &module.items {
                // A resource's `#[cm]` FQ names the interface declaring it, so
                // only the interface's effect answers for that FQ.
                let (id, attrs, answers_cm_fq) = match item {
                    Item::Interface(decl) => (decl.id, &decl.attrs, true),
                    Item::Resource(decl) => (decl.id, &decl.attrs, false),
                    _ => continue,
                };
                let def = defs.def_at(id);
                let cm_fq = cm_import_of(attrs).map(CmImport::interface_path);
                let key = resolutions
                    .effect_decl(def)
                    .expect("an interface or resource is an effect");
                if answers_cm_fq
                    && closure.contains_key(&key)
                    && let Some(fq) = &cm_fq
                {
                    effect_by_cm_fq.entry(fq.clone()).or_insert(key.clone());
                }
                interfaces.insert(def, (key, cm_fq));
            }
        }

        let mut flows_by_var: IndexMap<String, Vec<usize>> = IndexMap::default();
        for (at, flow) in sem.types.row_flows().iter().enumerate() {
            for var in flow.from.iter().chain(&flow.into).filter(|e| e.is_var()) {
                flows_by_var
                    .entry(var.name().to_string())
                    .or_default()
                    .push(at);
            }
        }

        Self {
            flows_by_var,
            fn_effects,
            fn_params,
            fn_returns,
            mangled_index,
            mangled_params,
            trait_method_effects,
            ambient_trait_methods,
            open_traits,
            supertraits,
            fn_bound_traits,
            receiver_bounds: receiver_bounds_by_method,
            receiver_methods,
            impl_effects,
            blanket_impls,
            block_effects,
            forwarding_blocks,
            resources,
            drop_is_an_operation,
            closure,
            mentions,
            waiting,
            interfaces,
            effect_by_cm_fq,
            provided_import_fqs,
            resolutions: Rc::clone(&state.tysys.resolutions),
        }
    }

    fn index<'a>(&'a self, types: &'a TypeTable) -> EffectIndex<'a> {
        EffectIndex {
            types,
            flows_by_var: &self.flows_by_var,
            fn_effects: &self.fn_effects,
            fn_params: &self.fn_params,
            fn_returns: &self.fn_returns,
            mangled_index: &self.mangled_index,
            mangled_params: &self.mangled_params,
            trait_method_effects: &self.trait_method_effects,
            ambient_trait_methods: &self.ambient_trait_methods,
            open_traits: &self.open_traits,
            supertraits: &self.supertraits,
            fn_bound_traits: &self.fn_bound_traits,
            receiver_bounds: &self.receiver_bounds,
            receiver_methods: &self.receiver_methods,
            impl_effects: &self.impl_effects,
            blanket_impls: &self.blanket_impls,
            block_effects: &self.block_effects,
            forwarding_blocks: &self.forwarding_blocks,
            resources: &self.resources,
            drop_is_an_operation: &self.drop_is_an_operation,
            closure: &self.closure,
            mentions: &self.mentions,
            waiting: &self.waiting,
            interfaces: &self.interfaces,
            effect_by_cm_fq: &self.effect_by_cm_fq,
            provided_import_fqs: &self.provided_import_fqs,
            resolutions: &self.resolutions,
        }
    }
}

/// The cross-module effect data the body walk consults, assembled once.
struct EffectIndex<'a> {
    /// The program's types, which a static call's impl type arguments name.
    types: &'a TypeTable,
    /// Each effect variable → the [`TypeTable::row_flows`] whose value's row or
    /// slot's row holds it.
    flows_by_var: &'a IndexMap<String, Vec<usize>>,
    /// Declaration key → resolved effects (free calls resolve via `references`).
    fn_effects: &'a IndexMap<AstId, Vec<EffectRef>>,
    /// Declaration key → parameter type ids (for effect-parameter resolution).
    fn_params: &'a IndexMap<AstId, Vec<TypeId>>,
    fn_returns: &'a IndexMap<AstId, TypeId>,
    /// `(module, mangled name)` → effects (method / static dispatch).
    mangled_index: &'a IndexMap<(ModuleSource, String), Vec<EffectRef>>,
    /// `(module, mangled name)` → parameter type ids.
    mangled_params: &'a IndexMap<(ModuleSource, String), Vec<TypeId>>,
    /// Trait method → the effects it declares. A call through a type
    /// parameter's bound selects no impl, so this is what it can demand.
    trait_method_effects: &'a IndexMap<TraitMethodKey, Vec<EffectRef>>,
    ambient_trait_methods: &'a IndexSet<TraitMethodKey>,
    /// The traits that leave their effects to the impl.
    open_traits: &'a IndexSet<DefId>,
    /// Each trait's transitive supertraits.
    supertraits: &'a IndexMap<DefId, Vec<DefId>>,
    /// Per type-parameter slot of a function or method declaration, the traits
    /// bounding it.
    fn_bound_traits: &'a IndexMap<AstId, Vec<Vec<DefId>>>,
    /// The bounded parameters a generic impl's target names, by method.
    receiver_bounds: &'a IndexMap<AstId, Rc<[ReceiverBound]>>,
    /// The methods declaring `self`.
    receiver_methods: &'a IndexSet<AstId>,
    /// Every effect one impl's methods declare.
    impl_effects: &'a IndexMap<ImplKey, ImplEffects>,
    blanket_impls: &'a IndexMap<DefId, Vec<BlanketImpl>>,
    /// Every effect one `impl` block's methods declare, by the block.
    block_effects: &'a IndexMap<DefId, IndexSet<EffectRef>>,
    /// The `impl` blocks ending in `..forward`.
    forwarding_blocks: &'a IndexSet<DefId>,
    /// Declared resources, for classifying a missing effect.
    resources: &'a IndexSet<EffectRef>,
    /// The resources declaring a `drop` that keeps the handle, an ordinary
    /// operation rather than the release.
    drop_is_an_operation: &'a IndexSet<DefId>,
    /// Effect → implied resources propagation closure.
    closure: &'a IndexMap<EffectRef, IndexSet<EffectRef>>,
    /// The same closure through what operations mention alone, leaving out
    /// the `extends` families: what a handler holds.
    mentions: &'a IndexMap<EffectRef, IndexSet<EffectRef>>,
    /// What waiting on an `AsyncCall` demands, held where the call starts.
    waiting: &'a IndexSet<EffectRef>,
    /// Interface or resource declaration → its effect and `#[cm]` FQ.
    interfaces: &'a IndexMap<DefId, (EffectRef, Option<String>)>,
    /// CM interface FQ → the effect it declares, for reconstructing a
    /// component's host-leaf imports into effects.
    effect_by_cm_fq: &'a IndexMap<String, EffectRef>,
    /// CM interface FQs the consumer provides (discharged in reconstruction).
    provided_import_fqs: &'a IndexSet<String>,
    resolutions: &'a Resolutions,
}

/// Per effect `binding` installs a handler for, what that handler performs:
/// what its methods declare, and the effect itself where `..forward` passes
/// operations to the next handler out. The installing function must hold it,
/// since the handler runs on its behalf.
fn binding_performed_effects(
    annotations: Option<&TypeAnnotations>,
    index: &EffectIndex,
    binding: &EffectHandlerBinding,
    rows: &Rows,
) -> IndexMap<DeclaredHead, Vec<EffectRef>> {
    let mut performed: IndexMap<DeclaredHead, Vec<EffectRef>> = IndexMap::default();
    for facts in annotations
        .into_iter()
        .flat_map(|a| a.all(|f| &f.handler_bindings, binding.id))
    {
        for entry in &facts.effects {
            let Some(def) = entry.impl_def else {
                continue;
            };
            let forwarded = index
                .forwarding_blocks
                .contains(&def)
                .then(|| EffectRef::Concrete(entry.effect.clone()));
            let installed = index.installed_effects(def, facts.handler_type, rows);
            let effects = performed.entry(entry.effect.clone()).or_default();
            for effect in installed.into_iter().chain(forwarded) {
                if !effects.contains(&effect) {
                    effects.push(effect);
                }
            }
        }
    }
    performed
}

/// The effects `with E => h do` grants to its body.
fn binding_granted_effects(
    annotations: Option<&TypeAnnotations>,
    index: &EffectIndex,
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
            .map(|entry| EffectRef::Concrete(entry.effect.clone()))
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
            ast::Type::Named(named) => index.resolutions.effect_at(named.id, &named.name),
            _ => None,
        })
        .into_iter()
        .collect()
}

/// What a direct call of an operation of the interface or resource `owner`
/// demands of its caller: the owner itself, whether a handler or the host
/// answers it, unless a composed component implements it.
fn operation_requirements(
    sem: &Semantics,
    index: &EffectIndex,
    owner: Option<DefId>,
) -> Vec<EffectRef> {
    operation_demands(sem, index, owner).all()
}

/// [`operation_requirements`], split by why each is demanded.
#[derive(Default)]
struct OperationDemands {
    certain: Vec<EffectRef>,
    /// A `wasi:*` import of a component that records nothing of what its
    /// exports demand, so every one is.
    unrecorded: Vec<EffectRef>,
}

impl OperationDemands {
    fn all(&self) -> Vec<EffectRef> {
        self.certain
            .iter()
            .chain(&self.unrecorded)
            .cloned()
            .collect()
    }
}

fn operation_demands(
    sem: &Semantics,
    index: &EffectIndex,
    owner: Option<DefId>,
) -> OperationDemands {
    let mut demands = OperationDemands::default();
    let Some((effect, cm_fq)) = owner.and_then(|def| index.interfaces.get(&def)) else {
        return demands;
    };
    if let Some(fq) = cm_fq
        && let Some(registry) = sem.cm_interface_registry()
        && registry.is_component_interface(fq)
    {
        // Composition-relative: the imported interface is composed away, so its
        // operations demand the dependency's own host-leaf capabilities. A
        // types-only leaf has no operation to perform.
        for (leaf, demand) in registry
            .demanded_imports_for(fq)
            .filter(|(leaf, _)| registry.declares_functions(leaf))
            .filter(|(leaf, _)| !index.provided_import_fqs.contains(leaf.as_str()))
        {
            let Some(effect) = index.effect_by_cm_fq.get(leaf) else {
                continue;
            };
            match demand {
                Demand::Certain => demands.certain.push(effect.clone()),
                Demand::Unrecorded => demands.unrecorded.push(effect.clone()),
            }
        }
        return demands;
    }
    demands.certain.push(effect.clone());
    demands
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
        let allowed: IndexSet<&EffectRef> = declared_by_trait.iter().collect();
        for effect in declared {
            if effect.is_param() || allowed.contains(effect) {
                continue;
            }
            out.push(EffectError {
                callee: mangle_local_method(trait_name.base_name(), &method.name),
                missing_effect: effect.name().to_string(),
                fault: EffectFault::UndeclaredByTrait,
                span: method.span,
                module: module.to_string(),
            });
        }
    }
}

/// What a body holds beyond its own declaration: the effects listed, or every
/// effect, for one an `#[ambient(_)]` on it or on its trait method waives.
/// `Mentioned` is a handler's: what the handled effect's operations mention,
/// held as listed rather than through their `extends` families, so a handler
/// reaches no sibling of what it handles.
#[derive(Clone, Copy)]
enum Held<'a> {
    Only(&'a [EffectRef]),
    Mentioned(&'a [EffectRef]),
    Every,
}

fn check_function_effects_sem(
    sem: &Semantics,
    module: &ModuleSource,
    func: &Function,
    index: &EffectIndex,
    held: Held<'_>,
    enclosing_allows_unhandled: bool,
    out: &mut Vec<EffectError>,
    solved: &mut EffectRows,
    log: &mut ReachLog,
) {
    let admitted = admitted_effects(&func.name, &func.attrs, module, index, out);
    let Some(body) = &func.body else {
        return;
    };
    let held = if ambient_admits_every(&func.attrs) {
        Held::Every
    } else {
        held
    };
    let caller_key = func.id;
    let annotations = module_annotations(sem, module);
    let (listed, mentioned) = match held {
        Held::Only(listed) => (listed, &[][..]),
        Held::Mentioned(mentioned) => (&[][..], mentioned),
        Held::Every => (&[][..], &[][..]),
    };
    let mut current = held_by(
        sem,
        annotations,
        index,
        func,
        body,
        admitted.into_iter().chain(listed.iter().cloned()),
    );
    current.extend(mentioned.iter().cloned());

    // Parameter name → type id (aligned with the recorded signature types),
    // for resolving indirect calls through function-typed parameters.
    let mut param_types: IndexMap<String, TypeId> = IndexMap::default();
    if let Some(type_ids) = annotations.and_then(|ann| ann.fn_param_types.get(&caller_key)) {
        for (param, type_id) in func.params.iter().zip(type_ids.iter()) {
            param_types.insert(param.name.clone(), *type_id);
        }
    }

    let walk = BodyWalk {
        name: &func.name,
        key: Some(caller_key),
        current,
        param_types,
        body,
        holds_every_effect: matches!(held, Held::Every),
        allows_unhandled: enclosing_allows_unhandled
            || ast::attrs_allow(&func.attrs, ast::lint::UNHANDLED_OPERATION),
    };
    walk_body_effects(sem, module, annotations, index, walk, out, solved, log);
}

/// What `func`'s body holds: its declared effects and `granted`, expanded
/// through the propagation closure. A resource its signature names is not
/// among them, so a body without `with` calls no host.
fn held_by(
    sem: &Semantics,
    annotations: Option<&TypeAnnotations>,
    index: &EffectIndex,
    func: &Function,
    body: &Block,
    granted: impl IntoIterator<Item = EffectRef>,
) -> IndexSet<EffectRef> {
    let mut current: IndexSet<EffectRef> = index
        .fn_effects
        .get(&func.id)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect();
    current.extend(granted);
    held_in_body(annotations, index, &sem.types, current, Body::Block(body))
}

/// The `wasi:*` interfaces some function of `exports` holds by what it
/// declares, which a caller across the component boundary must hold too. A
/// host import outside them is one only an `#[ambient]` body reaches.
fn exported_host_interfaces(
    sem: &Semantics,
    index: &EffectIndex,
    exports: &[(&ModuleSource, &Function)],
) -> IndexSet<String> {
    let interface_of: IndexMap<&EffectRef, &str> = index
        .interfaces
        .values()
        .filter_map(|(effect, fq)| Some((effect, fq.as_deref()?)))
        .collect();
    let mut out = IndexSet::default();
    for (src, func) in exports {
        // The parser admits a bodyless function, which runs nothing.
        let Some(body) = &func.body else {
            continue;
        };
        let held = held_by(sem, module_annotations(sem, src), index, func, body, []);
        out.extend(
            held.iter()
                .filter_map(|effect| interface_of.get(effect))
                .filter(|fq| is_wasi_fq(fq))
                .map(|fq| (*fq).to_string()),
        );
    }
    out
}

/// A test holds every effect, so nothing it performs is missing. A function
/// value it moves into a `fn` type is still bound by that type's row: the
/// callee holding the value performs it without holding what the test holds.
fn check_test_effects_sem(
    sem: &Semantics,
    module: &ModuleSource,
    test: &TestDecl,
    index: &EffectIndex,
    enclosing_allows_unhandled: bool,
    out: &mut Vec<EffectError>,
    solved: &mut EffectRows,
    log: &mut ReachLog,
) {
    let walk = BodyWalk {
        name: test.name.as_deref().unwrap_or("test"),
        key: None,
        current: IndexSet::default(),
        param_types: IndexMap::default(),
        body: &test.body,
        holds_every_effect: true,
        allows_unhandled: enclosing_allows_unhandled
            || ast::attrs_allow(&test.attributes, ast::lint::UNHANDLED_OPERATION),
    };
    walk_body_effects(
        sem,
        module,
        module_annotations(sem, module),
        index,
        walk,
        out,
        solved,
        log,
    );
}

/// Per-module annotations carry the dispatch facts and signature types that
/// have no flattened `Semantics` mirror (static-method dispatch, param /
/// return type ids).
fn module_annotations<'a>(
    sem: &'a Semantics,
    module: &ModuleSource,
) -> Option<&'a TypeAnnotations> {
    sem.state
        .as_ref()
        .and_then(|state| state.module_semantics.get(module))
        .map(|module_sem| &module_sem.types)
}

/// One body to walk, with what it holds on entry.
struct BodyWalk<'a> {
    name: &'a str,
    /// The function the body is, which callers reach; a test has none.
    key: Option<AstId>,
    current: IndexSet<EffectRef>,
    param_types: IndexMap<String, TypeId>,
    body: &'a Block,
    holds_every_effect: bool,
    /// `#[allow(unhandled_operation)]` covers the body.
    allows_unhandled: bool,
}

fn walk_body_effects(
    sem: &Semantics,
    module: &ModuleSource,
    annotations: Option<&TypeAnnotations>,
    index: &EffectIndex,
    walk: BodyWalk,
    out: &mut Vec<EffectError>,
    solved: &mut EffectRows,
    log: &mut ReachLog,
) {
    // A variable is solved at the call that minted it, and read wherever the
    // call's value flows, which may come before the walk reaches the call. Walk
    // until no solution read changed after it was read, keeping only the last
    // pass's findings. A walk that is not final grew a solution, and a solution
    // only grows within the effects the program names, so this ends.
    let mut rows = EffectRows::default();
    loop {
        let mut found = Vec::new();
        let mut walker = SemEffectWalker {
            sem,
            annotations,
            index,
            current: walk.current.clone(),
            holds_every_effect: walk.holds_every_effect,
            param_types: walk.param_types.clone(),
            module: module.source_path(),
            out: &mut found,
            closures: ClosureEffects::default(),
            collecting: None,
            rows: Rows::starting_from(rows),
            reach: BodyReach::default(),
        };
        ast::walk_block(&mut walker, walk.body);
        let SemEffectWalker {
            rows: mut walked,
            reach,
            ..
        } = walker;
        walked.propagate_flows(index);
        if walked.read_final() {
            walked.assert_solved(walk.name);
            walked.bound_flows(index, &mut found);
            out.extend(found);
            solved.extend(walked.solved);
            assert!(reach.open.is_empty(), "every handler scope closes");
            if let Some(key) = walk.key {
                log.bodies.insert(key, reach.calls);
            }
            if !walk.allows_unhandled {
                let module = module.source_path();
                log.scopes.extend(
                    reach
                        .closed
                        .into_iter()
                        .map(|scope| (scope, module.clone())),
                );
            }
            return;
        }
        rows = walked.solved;
    }
}

/// Whether a propagation closure treats each `extends` family as one effect.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Families {
    Joined,
    Apart,
}

/// Build the effect / resource propagation closure from `Semantics`: for each
/// effect or resource declaration, the resources its operations' parameter and
/// return types reference, and for a resource its family under `extends`
/// where `families` joins them, transitively closed. Reads the resolved
/// operation signatures from the `effect_ops` facts; `members` lets resource
/// detection descend into an operation type's own members.
fn build_propagation_closure_sem(
    sem: &Semantics,
    state: &AnnotateState,
    members: &MemberTables,
    families: Families,
) -> IndexMap<EffectRef, IndexSet<EffectRef>> {
    let resolutions = &*state.tysys.resolutions;
    let scan = ResourceScan {
        tt: &sem.types,
        resolutions,
        members,
    };
    let mut direct: IndexMap<EffectRef, IndexSet<EffectRef>> = IndexMap::default();

    for (src, module) in &sem.modules {
        let Some(annotations) = state.module_semantics.get(src).map(|m| &m.types) else {
            continue;
        };
        for item in &module.items {
            let (decl_id, is_resource) = match item {
                Item::Interface(decl) => (decl.id, false),
                Item::Resource(decl) => (decl.id, true),
                _ => continue,
            };
            let def = resolutions.defs().def_at(decl_id);
            let key = resolutions
                .effect_decl(def)
                .expect("an interface or resource is an effect");
            if is_resource && families == Families::Joined {
                // A family under `extends` is one effect: each member implies
                // its parent and its parent implies it, so the closure reaches
                // every member.
                if let Some(parent) = scan.tt.resource_chain(def).nth(1) {
                    let parent = resource_effect(resolutions, parent);
                    direct
                        .entry(parent.clone())
                        .or_default()
                        .insert(key.clone());
                    direct.entry(key.clone()).or_default().insert(parent);
                }
            }
            let Some(ops) = annotations.effect_ops.get(&decl_id) else {
                continue;
            };
            let mut refs: IndexSet<EffectRef> = IndexSet::default();
            for op in ops {
                for param in &op.params {
                    scan.collect(param.type_id, &mut refs, &mut TypeSet::default());
                }
                scan.collect(op.return_type, &mut refs, &mut TypeSet::default());
            }
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
                if matches!(eff, EffectRef::Concrete(_))
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
    // A family's cycle leads each member back to itself, and what an effect
    // implies is what it is not.
    for (key, implied) in &mut direct {
        implied.shift_remove(key);
    }
    direct
}

/// The effect an attribute argument written in `module` refers to. It has no
/// reference site, so the module's scope decides.
#[expect(
    clippy::disallowed_methods,
    reason = "an attribute argument is a spelling no walk visits"
)]
fn effect_named_in(name: &str, module: &ModuleSource, index: &EffectIndex) -> Option<EffectRef> {
    let resolutions = index.resolutions;
    resolutions.effect_decl(resolutions.resolve_in(module, name)?)
}

/// Expand an effect set through the propagation closure.
fn expand_through_closure(
    effects: &IndexSet<EffectRef>,
    closure: &IndexMap<EffectRef, IndexSet<EffectRef>>,
) -> IndexSet<EffectRef> {
    let mut out: IndexSet<EffectRef> = IndexSet::default();
    for effect in effects {
        out.insert(effect.clone());
        if matches!(effect, EffectRef::Concrete(_))
            && let Some(extra) = closure.get(effect)
        {
            for e in extra {
                out.insert(e.clone());
            }
        }
    }
    out
}

/// Finds whether a body starts an async call, so it may wait on the call there.
/// A closure is a body of its own, so the search stops at one.
struct StartsAsyncCall<'a> {
    annotations: &'a TypeAnnotations,
    types: &'a TypeTable,
    found: bool,
}

impl AstVisitor<'_> for StartsAsyncCall<'_> {
    fn visit_expr(&mut self, expr: &Expr) {
        if matches!(expr, Expr::Closure(_)) {
            return;
        }
        if matches!(expr, Expr::Call(_) | Expr::MethodCall(_))
            && self
                .annotations
                .all(|facts| &facts.expression_types, expr.id())
                .any(|&ty| {
                    self.types
                        .is_compiler_item_type(ty, CompilerItem::AsyncCall)
                })
        {
            self.found = true;
        }
        ast::walk_expr(self, expr);
    }
}

/// A function's or a closure's body, walked alike.
#[derive(Clone, Copy)]
enum Body<'a> {
    Block(&'a ast::Block),
    Expr(&'a Expr),
}

impl Body<'_> {
    fn walk<V: for<'ast> AstVisitor<'ast>>(self, v: &mut V) {
        match self {
            Body::Block(block) => ast::walk_block(v, block),
            Body::Expr(expr) => v.visit_expr(expr),
        }
    }
}

/// What `body` holds, given `declared` from its signature: the waits on an
/// async call it starts, what a held effect needs internally (a function
/// holding `Stdout` may call operations that need `Stream`).
fn held_in_body(
    annotations: Option<&TypeAnnotations>,
    index: &EffectIndex<'_>,
    tt: &TypeTable,
    mut declared: IndexSet<EffectRef>,
    body: Body<'_>,
) -> IndexSet<EffectRef> {
    if starts_async_call(annotations, tt, |v| body.walk(v)) {
        declared.extend(index.waiting.iter().cloned());
    }
    expand_through_closure(&declared, index.closure)
}

/// Whether the body `walk` visits starts an async call.
fn starts_async_call(
    annotations: Option<&TypeAnnotations>,
    types: &TypeTable,
    walk: impl FnOnce(&mut StartsAsyncCall),
) -> bool {
    let Some(annotations) = annotations else {
        return false;
    };
    let mut starts = StartsAsyncCall {
        annotations,
        types,
        found: false,
    };
    walk(&mut starts);
    starts.found
}

/// The effects an `#[ambient(E, F)]` on `owner`, a function or a global,
/// admits, reporting each name that reaches no effect in `module`.
fn admitted_effects(
    owner: &str,
    attrs: &[Attribute],
    module: &ModuleSource,
    index: &EffectIndex,
    out: &mut Vec<EffectError>,
) -> Vec<EffectRef> {
    let mut admitted = Vec::new();
    for (name, span) in ambient_effect_names(attrs).filter(|(name, _)| *name != AMBIENT_EVERY) {
        match effect_named_in(name, module, index) {
            Some(effect) => admitted.push(effect),
            None => out.push(EffectError {
                callee: owner.to_string(),
                missing_effect: name.to_string(),
                fault: EffectFault::UnknownAmbient,
                span,
                module: module.source_path(),
            }),
        }
    }
    admitted
}

/// The effect names `#[ambient(E, F)]` lists on an item, each with the
/// attribute's span.
fn ambient_effect_names(attrs: &[Attribute]) -> impl Iterator<Item = (&str, Span)> {
    attrs
        .iter()
        .filter(|attr| attr.name == AMBIENT)
        .flat_map(|attr| attr.args.iter().map(|arg| (arg.as_str(), attr.span)))
}

/// The `#[ambient]` argument admitting every effect.
const AMBIENT_EVERY: &str = "_";

/// The operation releasing a resource's handle.
const RESOURCE_DROP: &str = "drop";

/// Whether `#[ambient(_)]` waives every effect the item performs.
fn ambient_admits_every(attrs: &[Attribute]) -> bool {
    ambient_effect_names(attrs).any(|(name, _)| name == AMBIENT_EVERY)
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

/// Types of an indirect call's callee, preferring the enclosing function's
/// parameter types: a function-typed parameter callee leaves no `references`
/// edge or recorded expression type at the call, so nothing else names it.
fn indirect_callee_types(
    sem: &Semantics,
    param_types: &IndexMap<String, TypeId>,
    callee: &Expr,
) -> Vec<TypeId> {
    if let Expr::Ident(ident) = callee
        && let Some(type_id) = param_types.get(&ident.name)
    {
        return vec![*type_id];
    }
    sem.expr_types(callee)
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
    dispatched: OperationDemands,
    /// What a method of a resource demands besides its declaration.
    resource: OperationDemands,
    /// What each of the callee's effect parameters stood for at the call.
    params: IndexMap<String, IndexSet<EffectRef>>,
    /// The callee's declared return type, which the call's type instantiates.
    returns: Option<TypeId>,
    /// The function whose body the call runs, where a declaration names it.
    body: Option<AstId>,
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
    rows: &Rows,
) -> Vec<CalleeEffects> {
    let bare = |name: String, declared: Vec<EffectRef>| CalleeEffects {
        name,
        declared,
        dispatched: OperationDemands::default(),
        resource: OperationDemands::default(),
        params: IndexMap::default(),
        returns: None,
        body: None,
    };
    if let Expr::Ident(ident) = callee
        && let Some(def) = sem.referenced_symbol(ident.id)
        && let Some(effects) = index.fn_effects.get(&def)
    {
        let params = index.fn_params.get(&def).cloned().unwrap_or_default();
        let brought = if effects.iter().any(EffectRef::is_param) {
            brought_by_call(index, annotations, def, id, rows)
        } else {
            Brought::default()
        };
        let resolved = resolve_effect_params(sem, effects, &params, false, args, &brought, rows);
        return vec![CalleeEffects {
            params: resolved.params,
            returns: index.fn_returns.get(&def).copied(),
            body: Some(def),
            ..bare(ident.name.clone(), resolved.required)
        }];
    }
    let dispatches = dispatches_at(annotations, id);
    if dispatches.is_empty() {
        // The callee is a function-typed value (a closure or `fn(...)`
        // parameter). Its type carries the effects it performs when invoked.
        let rows_of = fn_rows(&sem.types, indirect_callee_types(sem, param_types, callee));
        if rows_of.is_empty() {
            return Vec::new();
        }
        let performed: Vec<EffectRef> = rows.row(&rows_of.concat());
        return vec![bare(INDIRECT_CALLEE.to_string(), performed)];
    }
    // Only a free function outside the bindings dispatches through the path: a
    // method names its receiver, and a host binding is already the import.
    let owner = match callee {
        Expr::Ident(ident) => index.resolutions.operation_owner(ident),
        _ => None,
    };
    dispatches
        .into_iter()
        .map(|(func_ref, self_in_args)| {
            let dispatches_through_path = func_ref.method_info.is_none()
                && !matches!(func_ref.module_source, ModuleSource::Binding { .. });
            let (resolved, resource) =
                dispatch_effects(sem, index, &func_ref, self_in_args, args, rows);
            CalleeEffects {
                name: callee_name(callee).to_string(),
                declared: resolved.required,
                dispatched: if dispatches_through_path {
                    operation_demands(sem, index, owner)
                } else {
                    OperationDemands::default()
                },
                resource,
                params: resolved.params,
                returns: index.callee_return(&func_ref),
                body: index.template_body(&func_ref),
            }
        })
        .collect()
}

/// What a dispatch to `func_ref` with `args` requires: what the callee
/// declares, and what a method of a resource demands besides
/// ([`EffectIndex::resource_demands`]). `self_in_args` says a qualified (UFCS)
/// call spells the receiver as its first argument, so the arguments align with
/// the callee's full parameter list.
fn dispatch_effects(
    sem: &Semantics,
    index: &EffectIndex<'_>,
    func_ref: &FunctionRef,
    self_in_args: bool,
    args: &[Expr],
    rows: &Rows,
) -> (Resolved, OperationDemands) {
    // A synthesized template (a derived method) is a method of the type it is
    // derived for, so it takes `self`.
    let has_receiver = match func_ref.template.as_ref().and_then(TemplateId::def) {
        Some(def) => index
            .receiver_methods
            .contains(&index.resolutions.defs().ast_id(def)),
        None => func_ref.method_info.is_some(),
    };
    let effects = index.method_effects(func_ref);
    let brought = if effects.iter().any(EffectRef::is_param) {
        index.brought_by_dispatch(func_ref, rows)
    } else {
        Brought::default()
    };
    let resolved = resolve_effect_params(
        sem,
        &effects,
        &index.method_param_types(func_ref),
        has_receiver && !self_in_args,
        args,
        &brought,
        rows,
    );
    (resolved, index.resource_demands(sem, func_ref))
}

/// The effects each closure literal's body performs, by the literal's node,
/// worked out once per walk.
type ClosureEffects = IndexMap<AstId, Vec<EffectRef>>;

/// Walks a function body, checking that each call's required effects are held.
struct SemEffectWalker<'a> {
    sem: &'a Semantics,
    annotations: Option<&'a TypeAnnotations>,
    index: &'a EffectIndex<'a>,
    /// Effects available at the current point: the function's declared +
    /// signature + ambient + propagated set, plus any effects granted
    /// by an enclosing `with H => … do { … }` handler scope (pushed / popped as
    /// the walk enters / leaves the do-block body).
    current: IndexSet<EffectRef>,
    /// A `test` body, which holds every effect: nothing it performs is
    /// missing, whatever `current` lists.
    holds_every_effect: bool,
    /// This function's parameter name → type id, for resolving the callee of an
    /// indirect call through a function-typed parameter (which leaves no
    /// `references` edge or recorded expression type at the call site).
    param_types: IndexMap<String, TypeId>,
    module: String,
    out: &'a mut Vec<EffectError>,
    /// The closure literals of this body whose effects are worked out so far.
    closures: ClosureEffects,
    /// Where the walk is working out a closure body's effects: what it found
    /// the body to perform beyond `current`, reported as nothing missing.
    collecting: Option<IndexSet<EffectRef>>,
    /// What each effect variable this body's calls instantiated stands for.
    rows: Rows,
    /// The calls this body makes and its handler scopes, for what each scope
    /// leaves out.
    reach: BodyReach,
}

impl EffectIndex<'_> {
    fn kind_of(&self, effect: &EffectRef) -> EffectKind {
        if self.resources.contains(effect) {
            EffectKind::Resource
        } else {
            EffectKind::Effect
        }
    }

    /// The flows whose value's or slot's row holds a variable `rows` has
    /// solved.
    fn flows_of(&self, rows: &Rows) -> Vec<&RowFlow> {
        let at: IndexSet<usize> = rows
            .solved
            .keys()
            .filter_map(|var| self.flows_by_var.get(var))
            .flatten()
            .copied()
            .collect();
        let flows = self.types.row_flows();
        at.into_iter()
            .map(|at| flows.get_index(at).expect("an index into the flows"))
            .collect()
    }

    /// Effects a method dispatch's callee declares.
    fn method_effects(&self, func_ref: &FunctionRef) -> Vec<EffectRef> {
        // A trait method's `with` clause bounds every impl, and a bound
        // dispatch has no impl to read: the declaration is what a call requires.
        match self.declared_by_trait(func_ref) {
            Some(declared) => declared.to_vec(),
            None => self.callee_entry(func_ref, self.fn_effects, self.mangled_index),
        }
    }

    /// What dispatching `func_ref` demands besides its declaration: for a
    /// direct (non-trait) method on a `resource`, what an operation of the
    /// resource demands ([`operation_demands`]). An inherited method demands
    /// the resource declaring it, whatever the receiver extends, since that
    /// resource's handler is the one the call dispatches to. The release is
    /// ambient, as an implicit drop is: whoever holds a handle may release it.
    fn resource_demands(&self, sem: &Semantics, func_ref: &FunctionRef) -> OperationDemands {
        if let Some(method_info) = &func_ref.method_info
            && method_info.trait_name.is_none()
            && let Some(declaring) = method_info.receiver().def()
            && self.resolutions.defs().kind(declaring) == DefKind::Resource
            && (method_info.method_name != RESOURCE_DROP
                || self.drop_is_an_operation.contains(&declaring))
        {
            assert!(
                self.interfaces.contains_key(&declaring),
                "a resource is an operation owner"
            );
            return operation_demands(sem, self, Some(declaring));
        }
        OperationDemands::default()
    }

    /// What a dispatch's type arguments bring to the callee's open effect
    /// parameter: the impl of a trait method's receiver, and each bounded
    /// parameter of the method's impl and of the method itself.
    fn brought_by_dispatch(&self, func_ref: &FunctionRef, rows: &Rows) -> Brought {
        let mut brought = Brought::default();
        let Some(method_info) = func_ref.method_info.as_ref() else {
            return brought;
        };
        // The method the dispatch selected, and the impl block declaring it.
        let defs = self.resolutions.defs();
        let impl_method = func_ref
            .template
            .as_ref()
            .and_then(TemplateId::def)
            .and_then(|def| {
                defs.parent(def)
                    .filter(|&owner| defs.kind(owner) == DefKind::Impl)
                    .map(|owner| (defs.ast_id(def), owner))
            });
        if let Some(trait_name) = &method_info.trait_name
            && let Some((receiver, key)) = impl_key(&method_info.fq_base_struct_name(), trait_name)
            && self.open_traits.contains(&key)
        {
            brought.determined = true;
            // The dispatch selected the impl's method, so what it declares is
            // what the impl brings, an effect parameter of the impl's closed
            // over the receiver's arguments: a function-typed one carries the
            // row an adapter's `F: fn() with E` binds. One the arguments leave
            // open is resolved from the method's bounds below. A trait's
            // default method has only the receiver's impl to read. The
            // receiver's name spells no rows, so its arguments are read off the
            // instantiation where there is one.
            let receiver_args: Vec<FqTypeName> = match func_ref.monomorph_info.as_ref() {
                Some(info) if !info.impl_type_args.is_empty() => info
                    .impl_type_args
                    .iter()
                    .map(|&arg| self.spelled_solved(arg, rows))
                    .collect(),
                _ => method_info.struct_type_args.clone(),
            };
            if let Some((method, block)) = impl_method {
                let declared = self
                    .fn_effects
                    .get(&method)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let target_args = spelled_target_args(self.types, block);
                let bounds = self
                    .receiver_bounds
                    .get(&method)
                    .map(|b| &**b)
                    .unwrap_or_default();
                brought.effects.extend(
                    self.close_over_args(declared, bounds, &target_args, &receiver_args, None)
                        .into_iter()
                        .filter(|e| !e.is_param()),
                );
            } else {
                match self.impl_effects.get(&(receiver, key)) {
                    Some(entry) => brought.effects.extend(self.close_over_args(
                        &entry.declared,
                        &entry.bounds,
                        &entry.target_args,
                        &receiver_args,
                        None,
                    )),
                    None => brought.open = true,
                }
            }
        }
        let Some(def) = func_ref.template.as_ref().and_then(TemplateId::def) else {
            return brought;
        };
        let method = self.resolutions.defs().ast_id(def);
        let spelled = |args: &[TypeId]| -> Vec<Vec<FqTypeName>> {
            args.iter().map(|&arg| self.spellings(arg, rows)).collect()
        };
        let info = func_ref.monomorph_info.as_ref();
        let impl_args = info
            .map(|info| spelled(&info.impl_type_args))
            .unwrap_or_default();
        // A static call leaves the impl's type arguments, inferred, to the
        // instantiation rather than the receiver's name.
        let receiver_args = match info {
            Some(info) if method_info.struct_type_args.is_empty() => info
                .impl_type_args
                .iter()
                .map(|&arg| self.types.fq_base_type_name(arg))
                .collect(),
            _ => method_info.struct_type_args.clone(),
        };
        let receiver = method_info.fq_base_struct_name().head_only();
        for bound in self
            .receiver_bounds
            .get(&method)
            .into_iter()
            .flat_map(|b| b.iter())
        {
            // The instantiation binds every slot; a receiver's name binds only
            // the parameters its target writes, and a blanket impl's receiver
            // is the parameter itself.
            let arg: Vec<TypeArg<'_>> =
                match (impl_args.get(bound.slot as usize), bound.path.as_deref()) {
                    (Some(spellings), _) => spellings.iter().map(type_arg).collect(),
                    (None, None) => Vec::new(),
                    (None, Some([])) => vec![(receiver.clone(), receiver_args.as_slice())],
                    (None, Some(path)) => type_arg_at(&receiver_args, path)
                        .map(type_arg)
                        .into_iter()
                        .collect(),
                };
            self.bring(&bound.traits, &arg, &mut brought);
        }
        let method_args = info
            .map(|info| spelled(&info.method_type_args))
            .unwrap_or_default();
        for (slot, traits) in self
            .fn_bound_traits
            .get(&method)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let arg: Vec<TypeArg<'_>> = method_args
                .get(slot)
                .into_iter()
                .flatten()
                .map(type_arg)
                .collect();
            self.bring(traits, &arg, &mut brought);
        }
        brought
    }

    /// `id` as an impl may name it, with its arguments: itself, reference or
    /// not, then each base down its newtype chain, which it inherits impls
    /// from.
    fn spellings(&self, id: TypeId, rows: &Rows) -> Vec<FqTypeName> {
        let mut out = Vec::new();
        let mut current = id;
        loop {
            match self.types.get(current) {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => current = *inner,
                ResolvedType::Newtype { base_type, .. } => {
                    out.push(self.spelled_solved(current, rows));
                    current = *base_type;
                }
                _ => {
                    out.push(self.spelled_solved(current, rows));
                    return out;
                }
            }
        }
    }

    /// `id` spelled with its rows, each effect variable in them replaced by
    /// what `rows` solved it to: an impl names no variable. One left unsolved
    /// stands for the empty row, as it does once the check is done.
    fn spelled_solved(&self, id: TypeId, rows: &Rows) -> FqTypeName {
        self.types.fq_type_name_with_rows(id).map_rows(&|row| {
            rows.row(row)
                .into_iter()
                .filter(|effect| !effect.is_var())
                .collect()
        })
    }

    /// Adds what the type argument `arg` of a parameter bounded by `traits`
    /// brings. An argument with no impl to read, or none at all, leaves the
    /// effect parameter open.
    ///
    /// `arg` lists the argument's spellings, a newtype before the base it
    /// inherits its impls from; the first with an impl of the trait answers.
    fn bring(&self, traits: &[DefId], arg: &[TypeArg<'_>], brought: &mut Brought) {
        let reached = traits.iter().flat_map(|key| {
            std::iter::once(key).chain(self.supertraits.get(key).into_iter().flatten())
        });
        for key in reached.filter(|key| self.open_traits.contains(*key)) {
            brought.determined = true;
            match arg.iter().find_map(|(head, args)| {
                Some((*args, self.impl_effects.get(&(head.clone(), *key))?))
            }) {
                Some((args, entry)) => {
                    brought.effects.extend(self.close_over_args(
                        &entry.declared,
                        &entry.bounds,
                        &entry.target_args,
                        args,
                        None,
                    ));
                }
                None => match self.blanket_for(*key, arg) {
                    Some(blanket) => {
                        brought
                            .effects
                            .extend(blanket.declared.iter().filter(|e| !e.is_param()).cloned());
                        self.bring(&blanket.bounds, arg, brought);
                    }
                    None => brought.open = true,
                },
            }
        }
    }

    /// The blanket impl of `trait_key` that answers for `arg`: one whose
    /// target bounds `arg` implements outright.
    fn blanket_for(&self, trait_key: DefId, arg: &[TypeArg<'_>]) -> Option<&BlanketImpl> {
        self.blanket_impls.get(&trait_key)?.iter().find(|blanket| {
            blanket.bounds.iter().all(|bound| {
                arg.iter()
                    .any(|(head, _)| self.impl_effects.contains_key(&(head.clone(), *bound)))
            })
        })
    }

    /// What an impl brings once its own effect parameter is filled from the
    /// receiver's type arguments `args`: what the impl of each open trait
    /// bounding one of its parameters brings, and the row each argument has
    /// where the impl's `target_args` write the parameter in a function type's
    /// row, which is how an adapter's impl reaches the closure it holds. A
    /// variable in that row is solved where the effects are read. An argument
    /// that is neither leaves the parameter. A blanket impl's receiver, the
    /// type itself, answers the bound on the impl's target where `receiver`
    /// spells it; a call leaves that to `blanket_for`.
    fn close_over_args(
        &self,
        declared: &[EffectRef],
        bounds: &[ReceiverBound],
        target_args: &[FqTypeName],
        args: &[FqTypeName],
        receiver: Option<&[TypeArg<'_>]>,
    ) -> IndexSet<EffectRef> {
        let mut out: IndexSet<EffectRef> = IndexSet::default();
        for effect in declared {
            if !effect.is_param() {
                out.insert(effect.clone());
                continue;
            }
            let mut filled = false;
            for (written, arg) in target_args.iter().zip(args) {
                filled |= rows_written_as(written, arg, effect, &mut out);
            }
            let mut brought = Brought::default();
            for bound in bounds {
                match bound.path.as_deref() {
                    None => {}
                    Some([]) => {
                        if let Some(receiver) = receiver {
                            self.bring(&bound.traits, receiver, &mut brought);
                        }
                    }
                    Some(path) => {
                        if let Some(arg) = type_arg_at(args, path) {
                            self.bring(&bound.traits, &[type_arg(arg)], &mut brought);
                        }
                    }
                }
            }
            out.extend(brought.effects);
            // One bound left open keeps the parameter, whatever the others fill.
            if brought.open || !(filled || brought.determined) {
                out.insert(effect.clone());
            }
        }
        out
    }

    /// What installing impl block `block` for a handler of type `handler`
    /// performs: what its methods declare, an effect parameter of the impl
    /// closed over the handler's type arguments as a dispatch to one of the
    /// methods closes it.
    fn installed_effects(&self, block: DefId, handler: TypeId, rows: &Rows) -> IndexSet<EffectRef> {
        let declared: Vec<EffectRef> = self.block_effects[&block].iter().cloned().collect();
        let defs = self.resolutions.defs();
        let bounds = defs
            .members(block)
            .iter()
            .find_map(|&method| self.receiver_bounds.get(&defs.ast_id(method)))
            .map(|bounds| &**bounds)
            .unwrap_or_default();
        let target = self
            .types
            .fq_type_name_with_rows(self.types.impl_target_whole(block));
        let spellings = self.spellings(handler, rows);
        // A blanket impl's target is its own parameter, which no spelling of
        // the handler names and which has no arguments to fill.
        let args = spellings
            .iter()
            .find(|spelling| spelling.head() == target.head())
            .map(|spelling| spelling.args().to_vec())
            .unwrap_or_default();
        let receiver: Vec<TypeArg<'_>> = spellings.iter().map(type_arg).collect();
        self.close_over_args(
            &declared,
            bounds,
            &spelled_target_args(self.types, block),
            &args,
            Some(&receiver),
        )
    }

    /// The effects the trait method behind a dispatch declares. `None` where
    /// the dispatch names no trait, or names an `interface` or a `resource`.
    fn declared_by_trait(&self, func_ref: &FunctionRef) -> Option<&[EffectRef]> {
        let method_info = func_ref.method_info.as_ref()?;
        self.effects_declared_by(
            method_info.trait_name.as_ref()?,
            method_info.declared_method_name(),
        )
    }

    /// The effects one trait method declares. `None` for a name no trait
    /// declares, an `interface` operation, or a `resource` method.
    fn effects_declared_by(&self, trait_name: &FqTraitName, method: &str) -> Option<&[EffectRef]> {
        self.trait_method_effects
            .get(&(trait_name.canonical()?, method.to_string()))
            .map(Vec::as_slice)
    }

    /// Parameter type ids for a method / static dispatch target.
    fn method_param_types(&self, func_ref: &FunctionRef) -> Vec<TypeId> {
        self.callee_entry(func_ref, self.fn_params, self.mangled_params)
    }

    /// The declared return type of the declaration a dispatch selected.
    fn callee_return(&self, func_ref: &FunctionRef) -> Option<TypeId> {
        self.fn_returns.get(&self.template_body(func_ref)?).copied()
    }

    /// The declaration whose body a dispatch to `func_ref` runs.
    fn template_body(&self, func_ref: &FunctionRef) -> Option<AstId> {
        let def = func_ref.template.as_ref().and_then(TemplateId::def)?;
        Some(self.resolutions.defs().ast_id(def))
    }

    /// `func_ref`'s entry, looked up by the declaration the dispatch selected.
    /// Its name carries the receiver's type arguments, so only a callee no
    /// declaration names is looked up by name.
    fn callee_entry<V: Clone + Default>(
        &self,
        func_ref: &FunctionRef,
        by_decl: &IndexMap<AstId, V>,
        by_name: &IndexMap<(ModuleSource, String), V>,
    ) -> V {
        let template = func_ref.template.as_ref();
        if template.and_then(TemplateId::operation).is_some() {
            // An operation declares no effects: what a call of one demands is
            // its owner's, which `operation_requirements` reads.
            return V::default();
        }
        let entry = match template.and_then(TemplateId::def) {
            Some(def) => by_decl.get(&self.resolutions.defs().ast_id(def)),
            None => by_name.get(&(func_ref.module_source.clone(), func_ref.name.clone())),
        };
        entry.cloned().unwrap_or_default()
    }
}

/// Resolve `EffectRef::Param` effects to concrete effects: the union of what
/// every determiner of the parameter brings, each function-typed argument and
/// `brought`, the bounded type arguments. `is_method` drops the leading `self`
/// parameter so params line up with `args`.
///
/// Both walks resolve before they read a callee's effects, so an `effect E`
/// bound to a concrete effect at the call site is seen by each of them.
fn resolve_effect_params(
    sem: &Semantics,
    callee_effects: &[EffectRef],
    param_types: &[TypeId],
    is_method: bool,
    args: &[Expr],
    brought: &Brought,
    rows: &Rows,
) -> Resolved {
    let type_table = &sem.types;
    let skip = usize::from(is_method && !param_types.is_empty());
    // The callee's own effect parameter, and any its parameters' rows name,
    // which a call's type may carry out as a variable to solve.
    let mut param_names: IndexSet<String> = callee_effects
        .iter()
        .filter(|e| e.is_param())
        .map(|e| e.name().to_string())
        .collect();
    for &param_type in param_types.iter().skip(skip) {
        type_table.effect_params_in(param_type, &mut param_names);
    }
    if param_names.is_empty() {
        return Resolved {
            required: callee_effects.to_vec(),
            params: IndexMap::default(),
        };
    }
    // `None` until an argument determines the parameter. An argument that
    // determines it to be pure leaves an empty set, which is not the same
    // answer as never having been determined.
    let mut concrete: IndexMap<String, Option<IndexSet<EffectRef>>> =
        param_names.iter().map(|n| (n.clone(), None)).collect();
    // The parameters an argument determines that no type says the effects of,
    // which stay requirements whatever else determines them.
    let mut unread: IndexSet<String> = IndexSet::default();
    for (param_type, arg) in param_types.iter().skip(skip).zip(args.iter()) {
        // A parameter whose rows sit inside it (`Holder<F>`, `F: fn() with
        // E`) meets them in the argument's type at the same places.
        let ResolvedType::Function {
            effects: formal, ..
        } = type_table.get(*param_type)
        else {
            for arg_type in sem.expression_types(arg.id()) {
                pair_effects(type_table, *param_type, arg_type, rows, &mut concrete);
            }
            continue;
        };
        if !formal
            .iter()
            .any(|e| e.is_param() && param_names.contains(e.name()))
        {
            continue;
        }
        let actual_rows = fn_rows(type_table, sem.expression_types(arg.id()));
        if actual_rows.is_empty() {
            unread.extend(
                formal
                    .iter()
                    .filter(|e| e.is_param())
                    .map(|e| e.name().to_string()),
            );
            continue;
        }
        // A function value performs what its row holds, less what the
        // parameter's type names outright, which the callee answers itself.
        let actual: Vec<EffectRef> = rows
            .row(&actual_rows.concat())
            .into_iter()
            .filter(|e| e.is_param() || !formal.contains(e))
            .collect();
        for formal_effect in formal {
            if let EffectRef::Param { name } = formal_effect
                && let Some(slot) = concrete.get_mut(name)
            {
                slot.get_or_insert_with(IndexSet::default)
                    .extend(actual.iter().cloned());
            }
        }
    }
    // What the type arguments bring, read through the rows solved so far.
    let brought_effects = rows.row(&brought.effects.iter().cloned().collect::<Vec<_>>());
    // What each parameter stands for at this call, for a variable it became.
    let params = concrete
        .iter()
        .filter_map(|(name, from_args)| {
            if from_args.is_none() && !brought.determined {
                return None;
            }
            let mut set = from_args.clone().unwrap_or_default();
            set.extend(brought_effects.iter().cloned());
            Some((name.clone(), set))
        })
        .collect();
    let mut resolved = Vec::new();
    for effect in callee_effects {
        match effect {
            EffectRef::Param { name } => {
                let from_args = concrete.get(name).and_then(Option::as_ref);
                // A function declares one effect parameter, `_` or `effect E`,
                // and a bound's open trait passes its effects to it whatever it
                // is called: the body performs them under it.
                // A parameter nothing determined stays the requirement, so
                // only a caller holding it satisfies it.
                if from_args.is_none() && !brought.determined {
                    resolved.push(effect.clone());
                    continue;
                }
                let mut set = from_args.cloned().unwrap_or_default();
                set.extend(brought_effects.iter().cloned());
                resolved.extend(set);
                if brought.open || unread.contains(name) {
                    resolved.push(effect.clone());
                }
            }
            EffectRef::Concrete(_) | EffectRef::Undeclared { .. } | EffectRef::Var { .. } => {
                resolved.push(effect.clone());
            }
        }
    }
    Resolved {
        required: resolved,
        params,
    }
}

/// What a call to one callee requires, and what each of its effect parameters
/// stood for there.
struct Resolved {
    required: Vec<EffectRef>,
    params: IndexMap<String, IndexSet<EffectRef>>,
}

/// For each effect parameter a row of `declared` names, the variables in the
/// same row of `actual`, the type a use instantiated it to.
fn pair_rows(
    types: &TypeTable,
    declared: TypeId,
    actual: TypeId,
    met: &mut IndexMap<String, IndexSet<String>>,
) {
    types.zip_rows(declared, actual, true, &mut |de, ae| {
        for param in de.iter().filter(|e| e.is_param()) {
            met.entry(param.name().to_string()).or_default().extend(
                ae.iter()
                    .filter(|e| e.is_var())
                    .map(|e| e.name().to_string()),
            );
        }
    });
}

/// Add to each effect parameter a row of a parameter's declared type names
/// what the same row of the argument's type performs beyond the row's
/// concrete effects.
fn pair_effects(
    types: &TypeTable,
    declared: TypeId,
    actual: TypeId,
    rows: &Rows,
    met: &mut IndexMap<String, Option<IndexSet<EffectRef>>>,
) {
    types.zip_rows(declared, actual, true, &mut |de, ae| {
        let performed: Vec<EffectRef> = rows
            .row(ae)
            .into_iter()
            .filter(|e| !de.contains(e))
            .collect();
        for param in de.iter().filter(|e| e.is_param()) {
            if let Some(slot) = met.get_mut(param.name()) {
                slot.get_or_insert_with(IndexSet::default)
                    .extend(performed.iter().cloned());
            }
        }
    });
}

/// What a free call's type arguments bring to the callee's open effect
/// parameter: each bounded parameter of the callee, instantiated at `site`.
fn brought_by_call(
    index: &EffectIndex<'_>,
    annotations: Option<&TypeAnnotations>,
    callee: AstId,
    site: AstId,
    rows: &Rows,
) -> Brought {
    let mut brought = Brought::default();
    let Some(slots) = index.fn_bound_traits.get(&callee) else {
        return brought;
    };
    let instantiations: Vec<_> = annotations
        .into_iter()
        .flat_map(|ann| ann.all(|facts| &facts.generic_instantiations, site))
        .collect();
    for (slot, traits) in slots.iter().enumerate() {
        if instantiations.is_empty() {
            index.bring(traits, &[], &mut brought);
        }
        for instantiation in &instantiations {
            let spellings = instantiation
                .type_args
                .get(slot)
                .map(|&arg| index.spellings(arg, rows))
                .unwrap_or_default();
            let arg: Vec<TypeArg<'_>> = spellings.iter().map(type_arg).collect();
            index.bring(traits, &arg, &mut brought);
        }
    }
    brought
}

/// Add to `out` what `actual` has in each function type's row where `written`
/// names `param` in the same row, beyond what that row names outright, and
/// say whether `written` names it anywhere.
fn rows_written_as(
    written: &FqTypeName,
    actual: &FqTypeName,
    param: &EffectRef,
    out: &mut IndexSet<EffectRef>,
) -> bool {
    let mut found = false;
    if let (
        TypeHead::Function {
            signature: written_signature,
            effects: written_row,
            ..
        },
        TypeHead::Function {
            signature: actual_signature,
            effects: actual_row,
            ..
        },
    ) = (written.head(), actual.head())
    {
        if written_row.contains(param) {
            found = true;
            out.extend(
                actual_row
                    .iter()
                    .filter(|e| !written_row.contains(e))
                    .cloned(),
            );
        }
        for (w, a) in written_signature.iter().zip(actual_signature) {
            found |= rows_written_as(w, a, param, out);
        }
    }
    for (w, a) in written.args().iter().zip(actual.args()) {
        found |= rows_written_as(w, a, param, out);
    }
    found
}

/// A type argument as an impl lookup reads it: its head, and its own type
/// arguments.
type TypeArg<'a> = (FqTypeName, &'a [FqTypeName]);

fn type_arg(arg: &FqTypeName) -> TypeArg<'_> {
    (arg.head_only(), arg.args())
}

/// The type argument a non-empty `path` reaches in `args`: an index into
/// `args`, then into that argument's own arguments. `None` where the path
/// leads past them.
fn type_arg_at<'a>(args: &'a [FqTypeName], path: &[usize]) -> Option<&'a FqTypeName> {
    let (first, rest) = path
        .split_first()
        .expect("the empty path is the receiver itself");
    let mut arg = args.get(*first)?;
    for &i in rest {
        arg = arg.args().get(i)?;
    }
    Some(arg)
}

impl SemEffectWalker<'_> {
    fn binding_granted_effects(&self, binding: &EffectHandlerBinding) -> Vec<EffectRef> {
        binding_granted_effects(self.annotations, self.index, binding)
    }

    fn report_missing(&mut self, effects: &[EffectRef], callee: &str, span: Span) {
        self.report_missing_as(effects, callee, span, EffectFault::Missing);
    }

    fn report_missing_as(
        &mut self,
        effects: &[EffectRef],
        callee: &str,
        span: Span,
        fault: fn(EffectKind) -> EffectFault,
    ) {
        // A variable left unsolved here is solved by the pass that keeps its
        // findings: the body's walk asserts so once it settles.
        let effects = self.rows.row(effects);
        let effects = effects.as_slice();
        if let Some(collected) = &mut self.collecting {
            collected.extend(
                effects
                    .iter()
                    .filter(|effect| effect.is_param() || !self.current.contains(*effect))
                    .cloned(),
            );
            return;
        }
        if self.holds_every_effect {
            return;
        }
        for effect in effects {
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
            if self.current.contains(effect) {
                continue;
            }
            self.out.push(EffectError {
                callee: callee.to_string(),
                missing_effect: effect.name().to_string(),
                fault: fault(self.index.kind_of(effect)),
                span,
                module: self.module.clone(),
            });
        }
    }
}

impl AstVisitor<'_> for SemEffectWalker<'_> {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        // A clause may perform no effect at all, which the purity check
        // reports; a missing effect there would be the same fault twice.
        if stmt.as_contract().is_some() {
            return;
        }
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
                    let (resolved, resource) =
                        dispatch_effects(self.sem, self.index, func_ref, false, &[], &self.rows);
                    let callee = func_ref
                        .method_info
                        .as_ref()
                        .map_or(func_ref.name.as_str(), |m| m.method_name.as_str());
                    self.report_resolved(func_ref, &resolved, &resource, callee, for_of.span);
                }
            }
        }
        ast::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::WithHandler(with_handler) => {
                // `with H => h do { body }` installs handlers, granting each
                // handled effect to the body (calls inside it — directly or via
                // helpers — observe the installed handler). The handler
                // expressions themselves run outside the grant. A binding
                // installs inside the ones before it, so its handler holds
                // what they grant.
                let mut granted: Vec<EffectRef> = Vec::new();
                for binding in &with_handler.handlers {
                    ast::walk_expr(self, &binding.handler);
                    for (handled, effects) in
                        binding_performed_effects(self.annotations, self.index, binding, &self.rows)
                    {
                        let effects: Vec<EffectRef> = effects
                            .into_iter()
                            .filter(|effect| !granted.contains(effect))
                            .collect();
                        self.report_missing_as(
                            &effects,
                            handled.name(),
                            binding.handler.span(),
                            EffectFault::MissingForHandler,
                        );
                    }
                    granted.extend(self.binding_granted_effects(binding));
                }
                let around = self
                    .reach
                    .open
                    .iter()
                    .flat_map(|scope| scope.installed.iter().cloned())
                    .collect();
                self.reach.open.push(HandlerScope {
                    installed: granted.clone(),
                    around,
                    calls: Vec::new(),
                    span: with_handler.span,
                });
                self.walk_granted(granted, |walker| {
                    ast::walk_block(walker, &with_handler.body);
                });
                let scope = self.reach.open.pop().expect("pushed above");
                self.reach.closed.push(scope);
                return;
            }
            // A closure's body runs wherever the closure is called, not where it
            // is written: what it performs is its type's row, demanded of each
            // caller.
            Expr::Closure(closure) => {
                self.closure_performs(closure);
                return;
            }
            _ => {}
        }
        // A call is checked after its arguments, so a variable a call among
        // them mints is solved before this one reads it.
        ast::walk_expr(self, expr);
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
                    let (resolved, resource) = dispatch_effects(
                        sem,
                        self.index,
                        &dispatch.function_ref,
                        false,
                        &method_call.args,
                        &self.rows,
                    );
                    let returns = self.index.callee_return(&dispatch.function_ref);
                    self.solve_rows(returns, method_call.id, &resolved.params);
                    self.report_resolved(
                        &dispatch.function_ref,
                        &resolved,
                        &resource,
                        &method_call.method,
                        method_call.span,
                    );
                }
            }
            Expr::StaticMethodCall(static_call) => {
                for (func_ref, self_in_args) in dispatches_at(self.annotations, static_call.id) {
                    let (resolved, resource) = dispatch_effects(
                        self.sem,
                        self.index,
                        &func_ref,
                        self_in_args,
                        &static_call.args,
                        &self.rows,
                    );
                    let returns = self.index.callee_return(&func_ref);
                    self.solve_rows(returns, static_call.id, &resolved.params);
                    self.report_resolved(
                        &func_ref,
                        &resolved,
                        &resource,
                        &static_call.method,
                        static_call.span,
                    );
                }
            }
            _ => {}
        }
    }
}

impl SemEffectWalker<'_> {
    /// Run `walk` holding `granted` besides what is held already.
    fn walk_granted(
        &mut self,
        granted: impl IntoIterator<Item = EffectRef>,
        walk: impl FnOnce(&mut Self),
    ) {
        let added: Vec<EffectRef> = granted
            .into_iter()
            .filter(|effect| self.current.insert(effect.clone()))
            .collect();
        walk(self);
        for effect in added {
            self.current.shift_remove(&effect);
        }
    }

    /// What a closure literal's body performs, which is its type's row. The
    /// body holds only what it grants itself: a handle it captured or was
    /// handed is no grant, so every host call on one is performed. A handler
    /// it installs discharges its own.
    fn closure_performs(&mut self, closure: &ast::ClosureExpr) -> Vec<EffectRef> {
        if let Some(performed) = self.closures.get(&closure.id) {
            return performed.clone();
        }
        // One type per walk that reached the literal, each with its own row.
        let closure_types: Vec<TypeId> = self.sem.expression_types(closure.id).collect();
        let current = held_in_body(
            self.annotations,
            self.index,
            &self.sem.types,
            IndexSet::default(),
            Body::Expr(&closure.body),
        );
        let mut scratch = Vec::new();
        let mut body = SemEffectWalker {
            sem: self.sem,
            annotations: self.annotations,
            index: self.index,
            current,
            holds_every_effect: false,
            param_types: self.param_types.clone(),
            module: self.module.clone(),
            out: &mut scratch,
            closures: std::mem::take(&mut self.closures),
            collecting: Some(IndexSet::default()),
            rows: std::mem::take(&mut self.rows),
            // A closure's calls run where it is called, which counts what
            // its type performs instead.
            reach: BodyReach::default(),
        };
        body.visit_expr(&closure.body);
        let performed: Vec<EffectRef> = body
            .collecting
            .take()
            .expect("the walk collects")
            .into_iter()
            .collect();
        self.closures = std::mem::take(&mut body.closures);
        self.rows = std::mem::take(&mut body.rows);
        self.reach.closed.append(&mut body.reach.closed);
        self.closures.insert(closure.id, performed.clone());
        for row in fn_rows(&self.sem.types, closure_types) {
            for var in row.iter().filter(|e| e.is_var()) {
                self.rows.widen(var.name(), &performed);
            }
        }
        performed
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
            &self.rows,
        );
        for site in sites {
            self.solve_rows(site.returns, id, &site.params);
            let mut owners = site.dispatched.all();
            owners.extend(site.resource.all());
            self.reach.note(owners, site.body, &site.declared);
            self.report_missing(&site.declared, &site.name, span);
            self.report_dispatched(&site.dispatched, &site.name, span);
            self.report_dispatched(&site.resource, &site.name, span);
        }
    }

    /// Report what dispatching a callee demands, saying where its component
    /// records nothing.
    fn report_dispatched(&mut self, demands: &OperationDemands, callee: &str, span: Span) {
        self.report_missing(&demands.certain, callee, span);
        self.report_missing_as(
            &demands.unrecorded,
            callee,
            span,
            EffectFault::MissingUnrecorded,
        );
    }

    /// Report what a dispatch to one callee requires ([`dispatch_effects`]),
    /// noting what it reaches.
    fn report_resolved(
        &mut self,
        func_ref: &FunctionRef,
        resolved: &Resolved,
        resource: &OperationDemands,
        callee: &str,
        span: Span,
    ) {
        let body = self.index.template_body(func_ref);
        self.reach.note(resource.all(), body, &resolved.required);
        self.report_missing(&resolved.required, callee, span);
        self.report_dispatched(resource, callee, span);
    }

    /// Record what each variable a call's type carries out stands for. Where
    /// the callee's declared return type names an effect parameter, the call's
    /// type holds the variable that parameter became at the call.
    fn solve_rows(
        &mut self,
        returns: Option<TypeId>,
        call: AstId,
        params: &IndexMap<String, IndexSet<EffectRef>>,
    ) {
        let Some(declared) = returns else {
            return;
        };
        let mut met: IndexMap<String, IndexSet<String>> = IndexMap::default();
        for actual in self.sem.expression_types(call) {
            pair_rows(&self.sem.types, declared, actual, &mut met);
        }
        for (param, vars) in met {
            // A parameter nothing at the call determines stands for no effect:
            // the callee holds for every row it may take, the empty one among
            // them, as its body's flows are bounded to.
            let stands: Vec<EffectRef> = params
                .get(&param)
                .map(|stands| stands.iter().cloned().collect())
                .unwrap_or_default();
            for var in vars {
                self.rows.widen(&var, &stands);
            }
        }
    }
}

/// What each effect variable a body's calls minted is solved to so far, and
/// which solutions the walk has read.
#[derive(Default)]
struct Rows {
    solved: EffectRows,
    /// Each variable read, with the length its solution had when first read.
    reads: RefCell<IndexMap<String, Option<usize>>>,
}

impl Rows {
    fn starting_from(solved: EffectRows) -> Self {
        Self {
            solved,
            reads: RefCell::default(),
        }
    }

    /// `effects` with each variable replaced by what it is solved to so far.
    fn row(&self, effects: &[EffectRef]) -> Vec<EffectRef> {
        solve_row(effects, &self.solved, &mut |var| {
            self.reads
                .borrow_mut()
                .entry(var.to_string())
                .or_insert_with(|| self.solved.get(var).map(Vec::len));
        })
    }

    /// Add `effects` to what `var` is solved to. A solution only grows: one
    /// call site may resolve through several dispatches, each adding what it
    /// brings.
    fn widen(&mut self, var: &str, effects: &[EffectRef]) -> bool {
        let row = self.solved.entry(var.to_string()).or_default();
        let had = row.len();
        for effect in effects {
            if !row.contains(effect) {
                row.push(effect.clone());
            }
        }
        row.len() != had
    }

    /// Hand each flow's row on to the variables of the slot it moves into,
    /// until none grows: a variable stands for every value moved into it.
    fn propagate_flows(&mut self, index: &EffectIndex<'_>) {
        loop {
            let mut grew = false;
            for flow in index.flows_of(self) {
                if flow.into.iter().any(EffectRef::is_param) {
                    continue;
                }
                let carried: Vec<EffectRef> = self
                    .row(&flow.from)
                    .into_iter()
                    .filter(|e| !e.is_var() && !flow.into.contains(e))
                    .collect();
                for var in flow.into.iter().filter(|e| e.is_var()) {
                    grew |= self.widen(var.name(), &carried);
                }
            }
            if !grew {
                return;
            }
        }
    }

    /// Report each effect a value performs beyond the row of the slot a flow
    /// moved it into, where that row names no variable to grow. An effect
    /// parameter there admits only itself: the function holds for every row
    /// its caller may pick for it, the empty one among them.
    fn bound_flows(&self, index: &EffectIndex<'_>, out: &mut Vec<EffectError>) {
        for flow in index.flows_of(self) {
            if flow.into.iter().any(EffectRef::is_var) {
                continue;
            }
            let admitted =
                expand_through_closure(&flow.into.iter().cloned().collect(), index.closure);
            for effect in self.row(&flow.from) {
                assert!(
                    !effect.is_var(),
                    "a settled body's flows read only solved variables"
                );
                if admitted.contains(&effect) {
                    continue;
                }
                out.push(EffectError {
                    callee: String::new(),
                    missing_effect: effect.name().to_string(),
                    fault: EffectFault::BeyondExpectedType(index.kind_of(&effect)),
                    span: flow.span,
                    module: flow.module.clone(),
                });
            }
        }
    }

    /// Every variable a settled body's walk read is solved: each is minted by a
    /// call or a closure literal in the body, which the walk solves.
    fn assert_solved(&self, function: &str) {
        let reads = self.reads.borrow();
        let unsolved: Vec<&String> = reads
            .keys()
            .filter(|var| !self.solved.contains_key(*var))
            .collect();
        assert!(
            unsolved.is_empty(),
            "in {function}: effect variables read but never solved: {unsolved:?}"
        );
    }

    /// Whether every solution the walk read is the one it ends with, so a
    /// further walk would read the same.
    fn read_final(&self) -> bool {
        self.reads
            .borrow()
            .iter()
            .all(|(var, len)| self.solved.get(var).map(Vec::len) == *len)
    }
}

/// The row of each function type among `types`.
fn fn_rows(table: &TypeTable, types: impl IntoIterator<Item = TypeId>) -> Vec<&[EffectRef]> {
    types
        .into_iter()
        .filter_map(|type_id| match table.get(type_id) {
            ResolvedType::Function { effects, .. } => Some(effects.as_slice()),
            _ => None,
        })
        .collect()
}

/// The spans of the bindings `expr` declares with a closure literal as its
/// initializer.
fn closure_bindings(expr: &Expr) -> Vec<Span> {
    struct Closures(Vec<Span>);
    impl AstVisitor<'_> for Closures {
        fn visit_stmt(&mut self, stmt: &Stmt) {
            if let Stmt::Let(binding) = stmt
                && let Some(Expr::Closure(_)) = &binding.value
                && let Pattern::Ident { span, .. } | Pattern::MutIdent { span, .. } =
                    &binding.pattern
            {
                self.0.push(*span);
            }
            ast::walk_stmt(self, stmt);
        }
    }
    let mut closures = Closures(Vec::new());
    closures.visit_expr(expr);
    closures.0
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
    run_purity_checks(sem, &data.index(&sem.types), &mut out);
    out
}

/// Answers whether an expression performs an effect, for a lint that reports
/// only an expression that does not.
pub(crate) struct EffectProbe<'a> {
    sem: &'a Semantics,
    state: &'a AnnotateState,
    data: OwnedEffectData,
}

impl<'a> EffectProbe<'a> {
    /// `None` where the elaborator left no state to answer from.
    pub(crate) fn new(sem: &'a Semantics) -> Option<Self> {
        let state = sem.state.as_ref()?;
        let data = OwnedEffectData::build(sem, state, IndexSet::default());
        Some(Self { sem, state, data })
    }

    /// Whether evaluating `expr`, written in `module`, performs an effect.
    pub(crate) fn performs_effect(&self, module: &ModuleSource, expr: &Expr) -> bool {
        let index = self.data.index(&self.sem.types);
        let mut out = Vec::new();
        PurityWalker::new(self.sem, self.state, &index, module, &mut out).visit_expr(expr);
        !out.is_empty()
    }
}

/// Walk every expression that must be pure, appending violations.
/// Shared by [`check_purity_semantic`] and [`check_semantics`].
fn run_purity_checks(sem: &Semantics, index: &EffectIndex, out: &mut Vec<PurityError>) {
    let Some(state) = sem.state.as_ref() else {
        return;
    };
    for (src, module) in &sem.modules {
        if !is_effect_checked(src) {
            continue;
        }
        let mut walker = PurityWalker::new(sem, state, index, src, out);
        for item in &module.items {
            match item {
                Item::Function(func) => walker.check_signature(func),
                Item::Impl(impl_block) => {
                    for method in &impl_block.methods {
                        walker.check_signature(method);
                    }
                }
                Item::Trait(trait_decl) => {
                    for method in &trait_decl.methods {
                        walker.check_signature(method);
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
                    // An unknown name is the effect check's to report.
                    walker.granted = ambient_effect_names(&global.attributes)
                        .filter_map(|(name, _)| effect_named_in(name, src, index))
                        .collect();
                    walker.grants_every = ambient_admits_every(&global.attributes);
                    walker.check(PureContext::GlobalInitializer, &global.initializer);
                    walker.granted.clear();
                    walker.grants_every = false;
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
    /// Whether an `#[ambient(_)]` on the global being walked admits every
    /// effect.
    grants_every: bool,
    /// Names the callee of an indirect call through a function-typed parameter.
    /// Always empty: a global initializer has no enclosing function, and a
    /// default expression cannot name a parameter — the call site evaluates it
    /// before any is bound.
    param_types: IndexMap<String, TypeId>,
    /// The span of the `contract` clause being walked. A build may skip the
    /// clause, so it may write only the values it creates: a write reaching
    /// anything else would make what the call computes depend on the build.
    clause: Option<Span>,
    /// Where the clause binds a closure it writes, by the binding's span.
    clause_closures: Vec<Span>,
    out: &'a mut Vec<PurityError>,
}

impl<'a> PurityWalker<'a> {
    /// A walker over `module_source`, outside any `with … do`.
    fn new(
        sem: &'a Semantics,
        state: &'a AnnotateState,
        index: &'a EffectIndex<'a>,
        module_source: &'a ModuleSource,
        out: &'a mut Vec<PurityError>,
    ) -> Self {
        Self {
            sem,
            annotations: state.module_semantics.get(module_source).map(|m| &m.types),
            index,
            module_source,
            context: PureContext::DefaultValue,
            granted: IndexSet::default(),
            grants_every: false,
            param_types: IndexMap::default(),
            clause: None,
            clause_closures: Vec::new(),
            out,
        }
    }

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

    /// A function's defaults and its `contract` clauses.
    fn check_signature(&mut self, func: &ast::Function) {
        self.check_defaults(&func.params);
        self.check_contracts(func);
    }

    fn check_contracts(&mut self, func: &ast::Function) {
        let sem = self.sem;
        for clause in func.contracts() {
            self.clause = Some(clause.span);
            for expr in std::iter::once(&clause.condition).chain(&clause.message) {
                self.clause_closures = closure_bindings(expr);
                self.check(PureContext::Contract, expr);
                for_each_write_site(sem, expr, |site| self.flag_clause_write(site));
            }
        }
        self.clause = None;
    }

    /// Flags `site` where it writes state the clause being walked did not
    /// create.
    fn flag_clause_write(&mut self, site: WriteSite<'_>) {
        match site {
            WriteSite::Place(place) | WriteSite::Unresolved(place) => self.flag_write(place),
            // A `&mut place` operand is that place's `Place` site. Any other
            // operand reaching a `&mut` passes on one the clause did not take.
            WriteSite::Lent(Expr::Unary(unary)) if unary.op == ast::UnaryOp::MutRef => {}
            // A closure the clause writes is walked with it, so its writes are
            // judged where they stand, wherever it is called.
            WriteSite::Lent(value) | WriteSite::Opaque(value) if self.is_clause_closure(value) => {}
            WriteSite::Lent(operand) => self.flag_mutate(operand),
            WriteSite::Opaque(callee) => {
                let name = unparse_expr_source(callee);
                self.flag(Impurity::CallValue(name), callee.span());
            }
        }
    }

    /// Flags a write to `place` unless it lands in a value the clause created
    /// and that holds nothing able to write elsewhere. A copy of a value
    /// holding a `&mut` shares what that `&mut` points to, wherever the copy
    /// is bound.
    fn flag_write(&mut self, place: &Expr) {
        let written = self.sem.write_target(place).place;
        let owned = match written {
            Expr::Ident(_) => {
                self.declared_in_clause(written) && !self.sem.expr_reaches_mut_ref(written)
            }
            Expr::FieldAccess(_) | Expr::Index(_) => false,
            Expr::Unary(unary) if unary.op == ast::UnaryOp::Deref => false,
            // A value the clause computed, which nothing outside it holds.
            _ => !self.sem.expr_reaches_mut_ref(written),
        };
        if !owned {
            self.flag_mutate(place);
        }
    }

    /// Flags a write the clause makes through `place`, named by what it starts
    /// from.
    fn flag_mutate(&mut self, place: &Expr) {
        let root = place.place_root();
        self.flag(Impurity::Mutate(unparse_expr_source(root)), root.span());
    }

    /// Whether `expr` names a binding the clause being walked declares.
    fn declared_in_clause(&self, expr: &Expr) -> bool {
        let Some(clause) = self.clause else {
            return false;
        };
        self.binding_span(expr)
            .is_some_and(|span| clause.encloses(&span))
    }

    /// Whether `expr` is a closure the clause being walked writes, or a
    /// binding it initializes with one.
    fn is_clause_closure(&self, expr: &Expr) -> bool {
        matches!(expr, Expr::Closure(_))
            || self
                .binding_span(expr)
                .is_some_and(|span| self.clause_closures.contains(&span))
    }

    /// Where the binding `expr` names is declared.
    fn binding_span(&self, expr: &Expr) -> Option<Span> {
        let Expr::Ident(ident) = expr else {
            return None;
        };
        self.sem
            .referenced_symbol_at(ident.id)
            .and_then(|symbol| symbol.span)
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
        // A `Param` left after resolution stands for effects no handler here
        // can have installed, so it is unanswered like any other. A variable
        // stands for what a closure written in the expression performs, which
        // the walk reads where the closure is written.
        !self.grants_every
            && effects.iter().any(|effect| {
                !effect.is_var() && (effect.is_param() || !self.granted.contains(effect))
            })
    }

    fn flag_if_effectful(
        &mut self,
        func_ref: &FunctionRef,
        self_in_args: bool,
        args: &[Expr],
        callee: &str,
        span: Span,
    ) {
        let (resolved, resource) = dispatch_effects(
            self.sem,
            self.index,
            func_ref,
            self_in_args,
            args,
            &Rows::default(),
        );
        if self.unanswered(&resolved.required) || self.unanswered(&resource.all()) {
            self.flag(Impurity::Call(callee.to_string()), span);
        }
    }

    /// Flags a call of an operation whose owner demands a capability the
    /// position does not hold.
    fn flag_if_operation(&mut self, owner: Option<DefId>, op: &str, span: Span) {
        // An operation declares no effect parameters, so there is nothing for
        // the arguments to resolve.
        let required = operation_requirements(self.sem, self.index, owner);
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
            &Rows::default(),
        );
        // `dispatched` is left to `flag_if_operation`, which asks the owner
        // once per site rather than once per dispatch.
        for site in sites {
            if self.unanswered(&site.declared) || self.unanswered(&site.resource.all()) {
                self.flag(Impurity::Call(site.name), span);
            }
        }
    }
}

impl AstVisitor<'_> for PurityWalker<'_> {
    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Call(call) => {
                if let Expr::Ident(ident) = &call.callee {
                    let owner = self.index.resolutions.operation_owner(ident);
                    self.flag_if_operation(owner, ident.case_name(), call.span);
                }
                self.flag_call(&call.callee, call.id, &call.args, call.span);
            }
            Expr::TaggedTemplate(tagged) => {
                self.flag_call(&tagged.tag, tagged.id, &[], tagged.span);
            }
            Expr::MethodCall(method_call) => {
                let sem = self.sem;
                for dispatch in sem.method_dispatches_at(method_call.id) {
                    self.flag_if_effectful(
                        &dispatch.function_ref,
                        false,
                        &method_call.args,
                        &method_call.method,
                        method_call.span,
                    );
                }
            }
            Expr::StaticMethodCall(static_call) => {
                if let ast::Type::Named(named) = &static_call.target_type {
                    let owner = self.index.resolutions.declared(named.id);
                    self.flag_if_operation(owner, &static_call.method, static_call.span);
                }
                for (func_ref, self_in_args) in dispatches_at(self.annotations, static_call.id) {
                    self.flag_if_effectful(
                        &func_ref,
                        self_in_args,
                        &static_call.args,
                        &static_call.method,
                        static_call.span,
                    );
                }
            }
            Expr::WithHandler(with_handler) => {
                // The install discharges what its body dispatches, so the body
                // walks under the grant. The handler expressions run outside it,
                // and a binding's handler holds what the bindings before it grant.
                for binding in &with_handler.handlers {
                    ast::walk_expr(self, &binding.handler);
                }
                let mut added: Vec<EffectRef> = Vec::new();
                for binding in &with_handler.handlers {
                    for (handled, effects) in binding_performed_effects(
                        self.annotations,
                        self.index,
                        binding,
                        &Rows::default(),
                    ) {
                        if self.unanswered(&effects) {
                            self.flag(
                                Impurity::Install(handled.name().to_string()),
                                binding.handler.span(),
                            );
                        }
                    }
                    added.extend(
                        binding_granted_effects(self.annotations, self.index, binding)
                            .into_iter()
                            .filter(|effect| self.granted.insert(effect.clone())),
                    );
                }
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
