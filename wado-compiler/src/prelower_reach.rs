//! What a program reaches over the TIR, and the prune that drops the rest:
//! once before `monomorphize` instantiates it, and again before `lower`
//! translates it.
//!
//! `WADO_TRACE=prelower_reach` prints what it found. See WEP 2026-05-26.

use std::cell::RefCell;
use std::rc::Rc;

use crate::compiler_item::CompilerItem;
use crate::compiler_trace;
use crate::defs::DefId;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::name::{
    FqTypeName, FunctionId, LocalMethodName, Receiver, TypeHead, global_init_function,
    is_dispatch_wrapper, is_effect_default, is_type_bridge,
};
use crate::nir_package::NirPackage;
use crate::reflect_marker::{bridge_callee, is_marker_bridge};
use crate::tir::{
    ResolvedType, StructDef, TemplateId, TirBlock, TirExpr, TirExprKind, TirFunction, TypeId,
};
use crate::tir_visitor::TirRefVisitor;
use crate::trace;
use crate::type_table::{InstanceKey, TypeTable};

const TRACE_TARGET: &str = "prelower_reach";

fn enabled() -> bool {
    trace::filter().enabled(TRACE_TARGET)
}

/// The identity `nir::FunctionRef::function_id` keys on. A mangled instance
/// carries its type arguments in `name`, so module and name alone settle it.
fn function_key(func: &TirFunction) -> FunctionId {
    FunctionId::free(&func.module_source, &func.name)
}

/// The traits `lower` dispatches to from a node that names no callee — a match
/// pattern, a wide-int literal comparison, a `builtin::variant_tag` marker — so
/// an impl of one is reachable however unreachable the call graph says it is.
const MINTABLE: &[CompilerItem] = &[
    CompilerItem::Eq,
    CompilerItem::Ord,
    CompilerItem::ReflectVariant,
];

/// What decides a root at one [`Stage`].
struct Roots<'a> {
    flat: &'a FlatPackage,
    stage: Stage,
    /// [`MINTABLE`] as the set an impl's trait is tested against.
    mintable: IndexSet<DefId>,
}

impl<'a> Roots<'a> {
    fn new(flat: &'a FlatPackage, stage: Stage) -> Self {
        let type_table = flat.type_table.borrow();
        let items = type_table.compiler_items();
        Self {
            flat,
            stage,
            mintable: MINTABLE
                .iter()
                .filter_map(|item| items.trait_def(*item))
                .collect(),
        }
    }

    /// How the program reaches `func` when no TIR body names it.
    fn entry(&self, func: &TirFunction, type_table: &TypeTable) -> Entry {
        let impl_trait = func
            .method_info
            .as_ref()
            .and_then(LocalMethodName::trait_decl);
        let root = func.is_cm_export
            || (func.is_export
                && self
                    .flat
                    .wasm_module_sources
                    .contains_key(&func.module_source))
            || func
                .compiler_item
                .is_some_and(CompilerItem::minted_after_tir);
        if root {
            return Entry::Root;
        }
        if self.stage == Stage::BeforeMonomorphize
            && let Some(entry) = self.minted_by_instances(func, impl_trait, type_table)
        {
            return entry;
        }
        if (is_type_bridge(&func.name) && !is_marker_bridge(&func.name))
            || impl_trait.is_some_and(|def| self.mintable.contains(&def))
        {
            Entry::Root
        } else {
            Entry::Called
        }
    }

    /// How the phases between monomorphization and `lower` may call `func`
    /// with no TIR body naming it: an instance's elaboration spells drop glue,
    /// type bridges, effect dispatch wrappers, `core:builtin` accessors,
    /// compiler items and [`MINTABLE`] impls by the type arguments it settles.
    /// A method and a type bridge belong to one type, so they wait for it.
    fn minted_by_instances(
        &self,
        func: &TirFunction,
        impl_trait: Option<DefId>,
        type_table: &TypeTable,
    ) -> Option<Entry> {
        // An instance's elaboration calls a trait method of each type argument
        // directly: a `$hole_fmt$` arm, a derived impl.
        let minted_method = func.compiler_item.is_some() || impl_trait.is_some();
        if minted_method && func.method_info.is_some() {
            return Some(Entry::on_type(receiver_head(func.method_info.as_ref())));
        }
        if is_type_bridge(&func.name) && !is_effect_default(&func.name) {
            let head = func
                .params
                .first()
                .and_then(|param| type_head(type_table, param.type_id));
            return Some(Entry::on_type(head));
        }
        (func.compiler_item.is_some()
            || is_effect_default(&func.name)
            || is_dispatch_wrapper(&func.name)
            || func.module_source.is_core_builtin())
        .then_some(Entry::Root)
    }
}

/// How a walk reaches a function.
enum Entry {
    /// From the start.
    Root,
    /// Once a reached body names a value of the type it belongs to.
    OnType(Head),
    /// Only through a body naming it.
    Called,
}

impl Entry {
    /// [`Self::OnType`] the head, or [`Self::Root`] where no head says which
    /// type it waits on.
    fn on_type(head: Option<Head>) -> Self {
        head.map_or(Self::Root, Self::OnType)
    }
}

/// What identifies a type to [`TypeReach`]: the head an `impl` header writes,
/// which a receiver and a value's type share.
type Head = TypeHead;

/// The head of a value's type, through references. `None` for a type a caller
/// has yet to settle.
fn type_head(type_table: &TypeTable, id: TypeId) -> Option<Head> {
    let id = type_table.peel_refs(id);
    match type_table.get(id) {
        ResolvedType::Primitive(_)
        | ResolvedType::Unit
        | ResolvedType::Struct { .. }
        | ResolvedType::Enum { .. }
        | ResolvedType::Variant { .. }
        | ResolvedType::Resource { .. }
        | ResolvedType::GenericResource { .. }
        | ResolvedType::GenericInstance { .. }
        | ResolvedType::Newtype { .. }
        | ResolvedType::Flags { .. }
        | ResolvedType::BuiltinArray(_)
        | ResolvedType::Function { .. } => settled(type_table.fq_base_type_name(id).head().clone()),
        _ => None,
    }
}

/// The head an impl method's receiver writes. `None` for a blanket impl's,
/// which any type may take.
fn receiver_head(method: Option<&LocalMethodName>) -> Option<Head> {
    match &method?.receiver {
        Receiver::Type(fq) | Receiver::RefTo(_, fq) => settled(fq.head().clone()),
        Receiver::Ref(_) => None,
    }
}

/// Every settled head a call's receiver spells, its type arguments' included.
fn receiver_heads(method: &LocalMethodName) -> Vec<Head> {
    fn walk(name: &FqTypeName, heads: &mut Vec<Head>) {
        heads.extend(settled(name.head().clone()));
        for arg in name.args() {
            walk(arg, heads);
        }
    }
    let mut heads = Vec::new();
    if let Receiver::Type(fq) | Receiver::RefTo(_, fq) = &method.receiver {
        walk(fq, &mut heads);
    }
    for arg in &method.struct_type_args {
        walk(arg, &mut heads);
    }
    heads
}

fn settled(head: Head) -> Option<Head> {
    match head {
        TypeHead::Binder { .. }
        | TypeHead::ParamBucket { .. }
        | TypeHead::Projection(_)
        | TypeHead::Unresolved(_) => None,
        TypeHead::Declared(_)
        | TypeHead::Shape { .. }
        | TypeHead::Builtin(_)
        | TypeHead::Tuple
        | TypeHead::Function { .. } => Some(head),
    }
}

/// The types the reached bodies hold values of, closed over their fields and
/// payloads, and the functions waiting on one.
struct TypeReach<'t> {
    type_table: &'t TypeTable,
    /// A declaration's field and payload types, and what its associated types
    /// answer; an anonymous shape's are its constituents.
    members: IndexMap<Head, Vec<TypeId>>,
    seen: IndexSet<TypeId>,
    reached: IndexSet<Head>,
    /// The modules a reached open shape is scoped by. Monomorphization
    /// substitutes one into a closed shape of the same module, so every shape
    /// of one counts as reached.
    open_shape_modules: IndexSet<ModuleSource>,
    waiting: IndexMap<Head, Vec<FunctionId>>,
}

impl<'t> TypeReach<'t> {
    fn new(flat: &FlatPackage, type_table: &'t TypeTable) -> Self {
        let declared = |def: DefId| FqTypeName::declared(type_table.defs(), def).head().clone();
        let mut members: IndexMap<Head, Vec<TypeId>> = IndexMap::default();
        for decl in &flat.structs {
            if let StructDef::Decl(def) = decl.def {
                members
                    .entry(declared(def))
                    .or_default()
                    .extend(decl.fields.iter().map(|field| field.type_id));
            }
        }
        for decl in &flat.variants {
            members
                .entry(declared(decl.def))
                .or_default()
                .extend(decl.cases.iter().map(|case| case.payload));
        }
        // A projection names no head until monomorphization resolves it, so
        // what a receiver's associated types answer is reached with it.
        for (receiver, answer) in type_table.assoc_type_answers() {
            if let Some(head) = instance_head(type_table, receiver) {
                members.entry(head).or_default().push(answer);
            }
        }
        Self {
            type_table,
            members,
            seen: IndexSet::default(),
            reached: IndexSet::default(),
            open_shape_modules: IndexSet::default(),
            waiting: IndexMap::default(),
        }
    }

    /// Queue `key` once `head` is reached.
    fn wait(&mut self, head: Head, key: FunctionId, work: &mut Vec<FunctionId>) {
        if self.is_reached(&head) {
            work.push(key);
        } else {
            self.waiting.entry(head).or_default().push(key);
        }
    }

    /// Reach `id` and every type it is built over, releasing what waited on one.
    fn add(&mut self, id: TypeId, work: &mut Vec<FunctionId>) {
        let tt = self.type_table;
        let mut stack = vec![id];
        while let Some(id) = stack.pop() {
            if !self.seen.insert(id) {
                continue;
            }
            stack.extend(tt.get_newtype_base(id));
            tt.any_constituent(id, &mut |part| {
                stack.push(part);
                false
            });
            if let Some(head) = type_head(tt, id) {
                self.reach_head(head, &mut stack, work);
            }
            if let ResolvedType::Struct {
                def: StructDef::Anon(shape),
                ..
            } = tt.get(id)
                && tt.anon_shape_is_open(*shape)
                && let module = tt.anon_struct_module(*shape)
                && self.open_shape_modules.insert(module.clone())
            {
                self.waiting.retain(|head, keys| {
                    let substituted = shape_module(head) == Some(module);
                    if substituted {
                        work.append(keys);
                    }
                    !substituted
                });
            }
        }
    }

    /// Reach the type a call's receiver names: a static method holds no value
    /// of it.
    fn add_receiver(&mut self, head: Head, work: &mut Vec<FunctionId>) {
        let mut stack = Vec::new();
        self.reach_head(head, &mut stack, work);
        for id in stack {
            self.add(id, work);
        }
    }

    /// Release what waited on `head`, and queue its members onto `stack`.
    fn reach_head(&mut self, head: Head, stack: &mut Vec<TypeId>, work: &mut Vec<FunctionId>) {
        if self.reached.contains(&head) {
            return;
        }
        stack.extend(self.members.get(&head).into_iter().flatten().copied());
        work.extend(self.waiting.swap_remove(&head).into_iter().flatten());
        self.reached.insert(head);
    }

    fn is_reached(&self, head: &Head) -> bool {
        self.reached.contains(head)
            || shape_module(head).is_some_and(|module| self.open_shape_modules.contains(module))
    }
}

/// The head of the receiver an associated type answers for.
fn instance_head(type_table: &TypeTable, receiver: &InstanceKey) -> Option<Head> {
    match receiver {
        InstanceKey::Nominal(def, _) => {
            settled(FqTypeName::declared(type_table.defs(), *def).head().clone())
        }
        InstanceKey::Ref(inner) | InstanceKey::MutRef(inner) => instance_head(type_table, inner),
        InstanceKey::Other(id) => type_head(type_table, *id),
    }
}

/// The module an anonymous shape's head is scoped by.
fn shape_module(head: &Head) -> Option<&ModuleSource> {
    match head {
        TypeHead::Shape { module, .. } => Some(module),
        _ => None,
    }
}

/// Where in the pipeline a walk runs, which decides what it must root.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Over templates and the code calling them, before an instance exists.
    BeforeMonomorphize,
    BeforeLower,
}

/// What [`reachable`] found, against the population it walked.
pub(crate) struct Reached {
    /// Every function present at TIR, and how many of them `lower` translates.
    present: IndexSet<FunctionId>,
    bodied: usize,
    reached: IndexSet<FunctionId>,
    /// Every callee any TIR body names, reached or not.
    named: IndexSet<FunctionId>,
}

/// Every function the program reaches from its roots. Walks only the bodies it
/// reaches, which is the work [`prune`] saves.
fn reach(flat: &FlatPackage, stage: Stage) -> IndexSet<FunctionId> {
    let roots = Roots::new(flat, stage);
    let type_table = flat.type_table.borrow();
    let mut bodies: IndexMap<FunctionId, &Rc<RefCell<TirFunction>>> = IndexMap::default();
    let mut templates: IndexMap<TemplateId, FunctionId> = IndexMap::default();
    let mut work: Vec<FunctionId> = Vec::new();
    let mut types = TypeReach::new(flat, &type_table);
    for func_rc in &flat.functions {
        let func = func_rc.borrow();
        let key = function_key(&func);
        match roots.entry(&func, &type_table) {
            Entry::Root => work.push(key.clone()),
            Entry::OnType(head) => types.wait(head, key.clone(), &mut work),
            Entry::Called => {}
        }
        if stage == Stage::BeforeMonomorphize
            && let Some(template) = func.template_id()
        {
            templates.insert(template, key.clone());
        }
        bodies.insert(key, func_rc);
    }
    let mut reached: IndexSet<FunctionId> = IndexSet::default();
    while let Some(key) = work.pop() {
        if !reached.insert(key.clone()) {
            continue;
        }
        let Some(func_rc) = bodies.get(&key) else {
            continue;
        };
        let func = func_rc.borrow();
        let Some(body) = &func.body else {
            continue;
        };
        let refs = BodyRefs::of(body, &type_table, stage);
        if stage == Stage::BeforeMonomorphize {
            let signature = func.params.iter().map(|param| param.type_id);
            for id in signature.chain([func.return_type]).chain(refs.types) {
                types.add(id, &mut work);
            }
            for head in refs.receivers {
                types.add_receiver(head, &mut work);
            }
        }
        for callee in refs.callees {
            // A template calls another by a spelling no function carries
            // (`EnumCase<E>::holds`); its declaration names the function.
            if let Some(template) = &callee.template
                && let Some(key) = templates.get(template)
            {
                work.push(key.clone());
            }
            work.push(callee.key);
        }
    }
    reached
}

/// [`reach`], against the population it walked, for [`audit`] to report.
fn reachable(flat: &FlatPackage) -> Reached {
    let mut present: IndexSet<FunctionId> = IndexSet::default();
    let mut named: IndexSet<FunctionId> = IndexSet::default();
    let mut bodied = 0;
    let type_table = flat.type_table.borrow();
    for func_rc in &flat.functions {
        let func = func_rc.borrow();
        present.insert(function_key(&func));
        if let Some(body) = &func.body {
            bodied += 1;
            named.extend(
                BodyRefs::of(body, &type_table, Stage::BeforeLower)
                    .callees
                    .into_iter()
                    .map(|callee| callee.key),
            );
        }
    }
    Reached {
        reached: reach(flat, Stage::BeforeLower),
        present,
        named,
        bodied,
    }
}

/// Report the functions `optimize` kept that [`before_lower`]'s walk never
/// reached — the calls `lower` and `optimize` mint.
pub(crate) fn audit(found: Option<&Reached>, package: &NirPackage) {
    let Some(found) = found else {
        return;
    };
    // `dce` marks rather than removes, so the live set is what it left bodied.
    let live: Vec<FunctionId> = package
        .functions
        .iter()
        .filter_map(|func_rc| {
            let func = func_rc.borrow();
            (!func.is_dead).then(|| FunctionId::free(&func.module_source, &func.name))
        })
        .collect();
    let mut unreached: Vec<FunctionId> = live
        .iter()
        .filter(|name| found.present.contains(*name) && !found.reached.contains(*name))
        .cloned()
        .collect();
    unreached.sort_by_cached_key(ToString::to_string);
    let (chained, minted): (Vec<&FunctionId>, Vec<&FunctionId>) = unreached
        .iter()
        .partition(|name| found.named.contains(*name));
    compiler_trace!(
        TRACE_TARGET,
        "reached {} of {} names ({} bodied) at TIR; of {} survivors, \
         {} are minted later and {} follow an unreached caller",
        found.reached.len(),
        found.present.len(),
        found.bodied,
        live.len(),
        minted.len(),
        chained.len()
    );
    for name in &minted {
        compiler_trace!(TRACE_TARGET, "  minted  {name}");
    }
    for name in &chained {
        compiler_trace!(TRACE_TARGET, "  chained {name}");
    }
}

/// What every pipeline does with `flat` on its way into `lower`. One entry
/// point, because `compile` and `dump` lower separately.
///
/// `WADO_NO_PRELOWER_PRUNE` holds the prune back, so a missing root is a flag
/// to flip rather than a compiler to rebuild.
pub(crate) fn before_lower(flat: &mut FlatPackage) -> Option<Reached> {
    let found = enabled().then(|| reachable(flat));
    if prunes() {
        prune(flat, Stage::BeforeLower);
    }
    found
}

/// What every pipeline does with `flat` on its way into `monomorphize`, so no
/// instance is made for a caller nothing reaches.
pub(crate) fn before_monomorphize(flat: &mut FlatPackage) {
    if prunes() {
        prune(flat, Stage::BeforeMonomorphize);
    }
}

fn prunes() -> bool {
    std::env::var_os("WADO_NO_PRELOWER_PRUNE").is_none()
}

/// Drop what no root reaches, and record the drop for the assert in `lower`'s
/// `Interner::resolve`.
fn prune(flat: &mut FlatPackage, stage: Stage) {
    let reached = reach(flat, stage);
    let before = flat.functions.len();
    let pruned = &mut flat.pruned;
    flat.functions.retain(|func_rc| {
        let key = function_key(&func_rc.borrow());
        let keep = reached.contains(&key);
        if !keep {
            pruned.insert(key);
        }
        keep
    });
    compiler_trace!(
        TRACE_TARGET,
        "pruned {} of {before} functions",
        before - flat.functions.len()
    );
}

/// A function a body names, and the declaration it selected.
struct Callee {
    key: FunctionId,
    template: Option<TemplateId>,
}

/// What a body names: its callees and, before monomorphization, the types of
/// the values it holds.
struct BodyRefs<'a> {
    type_table: &'a TypeTable,
    stage: Stage,
    callees: Vec<Callee>,
    types: Vec<TypeId>,
    receivers: Vec<Head>,
}

impl<'a> BodyRefs<'a> {
    fn of(body: &TirBlock, type_table: &'a TypeTable, stage: Stage) -> Self {
        let mut refs = Self {
            type_table,
            stage,
            callees: Vec::new(),
            types: Vec::new(),
            receivers: Vec::new(),
        };
        refs.walk_block(body);
        refs
    }

    fn push(&mut self, module_source: &ModuleSource, name: &str, template: Option<&TemplateId>) {
        self.callees.push(Callee {
            key: FunctionId::free(module_source, name),
            template: template.cloned(),
        });
    }
}

impl TirRefVisitor for BodyRefs<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if self.stage == Stage::BeforeMonomorphize {
            self.types.push(expr.type_id);
        }
        match &expr.kind {
            TirExprKind::Call {
                func, type_args, ..
            } => {
                self.push(&func.module_source, &func.name, func.template.as_ref());
                match self.stage {
                    Stage::BeforeMonomorphize => {
                        self.types.extend(type_args);
                        if let Some(method) = &func.method_info {
                            self.receivers.extend(receiver_heads(method));
                        }
                    }
                    // `lower` rewrites the marker into a call of its bridge.
                    // Before monomorphization its type arguments may still be a
                    // template's, so a bridge waits on its type there instead.
                    Stage::BeforeLower => {
                        if let Some(bridge) = bridge_callee(self.type_table, func, type_args) {
                            self.push(&bridge.module_source, &bridge.name, None);
                        }
                    }
                }
            }
            // A function used as a value: `IndirectCall` names the value, never
            // the function, so this is the only edge to what it may reach.
            TirExprKind::FuncRef {
                module_source,
                name,
                template,
                ..
            } => self.push(module_source, name, template.as_ref()),
            // `globals::extract` runs a global's initializer before its first
            // use, so a use is the edge to it.
            TirExprKind::GlobalVarGet {
                module_source,
                name,
            }
            | TirExprKind::GlobalVarSet {
                module_source,
                name,
                ..
            } => self.push(module_source, &global_init_function(name), None),
            _ => {}
        }
        self.walk_expr(expr);
    }
}
