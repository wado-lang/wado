//! What a program reaches over the TIR: the roots `monomorphize` walks from,
//! and the prune that drops what no root reaches before `lower` translates it.
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
    FunctionId, LocalMethodName, Receiver, TypeHead, global_init_function, is_effect_default,
    is_type_bridge,
};
use crate::nir_package::NirPackage;
use crate::reflect_marker::{bridge_callee, is_marker_bridge};
use crate::tir::{ResolvedType, TemplateId, TirBlock, TirExpr, TirExprKind, TirFunction, TypeId};
use crate::tir_visitor::TirRefVisitor;
use crate::trace;
use crate::type_table::TypeTable;

const TRACE_TARGET: &str = "prelower_reach";

fn enabled() -> bool {
    trace::filter().enabled(TRACE_TARGET)
}

/// The identity `nir::FunctionRef::function_id` keys on. A mangled instance
/// carries its type arguments in `name`, so module and name alone settle it.
pub(crate) fn function_key(func: &TirFunction) -> FunctionId {
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

/// The functions a phase calls with no TIR body naming them.
pub(crate) struct Roots {
    wasm_modules: IndexSet<ModuleSource>,
    /// [`MINTABLE`] as the set an impl's trait is tested against.
    mintable: IndexSet<DefId>,
    /// `Drop`, whose impls the drop glue an instance's elaboration adds calls.
    drop: Option<DefId>,
}

impl Roots {
    pub(crate) fn new(flat: &FlatPackage) -> Self {
        let type_table = flat.type_table.borrow();
        let items = type_table.compiler_items();
        Self {
            wasm_modules: flat.wasm_module_sources.keys().cloned().collect(),
            mintable: MINTABLE
                .iter()
                .filter_map(|item| items.trait_def(*item))
                .collect(),
            drop: items.trait_def(CompilerItem::Drop),
        }
    }

    fn impl_trait(func: &TirFunction) -> Option<DefId> {
        func.method_info
            .as_ref()
            .and_then(LocalMethodName::trait_decl)
    }

    /// Whether the host or what `lower` mints calls `func`.
    fn entry_point(&self, func: &TirFunction) -> bool {
        func.is_cm_export
            || (func.is_export && self.wasm_modules.contains(&func.module_source))
            || func
                .compiler_item
                .is_some_and(CompilerItem::minted_after_tir)
    }

    /// Whether `lower` or a later phase may call `func` with no TIR body naming
    /// it.
    fn before_lower(&self, func: &TirFunction) -> bool {
        self.entry_point(func)
            || (is_type_bridge(&func.name) && !is_marker_bridge(&func.name))
            || Self::impl_trait(func).is_some_and(|def| self.mintable.contains(&def))
    }

    /// How `monomorphize` reaches `func` when no body it scans names it.
    ///
    /// Besides what `lower` mints, the phases between monomorphization and
    /// `lower` call some with no body naming them: the CM binding's and effect
    /// dispatch's rewrites spell effect dispatch wrappers, default
    /// implementations, `core:builtin` accessors and compiler items. Drop glue
    /// calls `Drop` impls, and a reflection marker names its bridge by the
    /// erased types, which no instance spells before erasure. A bridge and an
    /// impl belong to one type, so they wait for a scanned body to hold it.
    pub(crate) fn before_monomorphize(&self, func: &TirFunction, type_table: &TypeTable) -> Entry {
        let root = self.entry_point(func)
            || (func.compiler_item.is_some() && func.method_info.is_none())
            || is_effect_default(&func.name)
            || func.is_dispatch_wrapper
            || func.module_source.is_core_builtin();
        if root {
            return Entry::Root;
        }
        if is_type_bridge(&func.name) {
            let head = func
                .params
                .first()
                .and_then(|param| type_head(type_table, param.type_id));
            return Entry::on_type(head);
        }
        let minted_impl = Self::impl_trait(func)
            .is_some_and(|def| self.mintable.contains(&def) || Some(def) == self.drop);
        if minted_impl {
            return Entry::on_type(receiver_head(func.method_info.as_ref()));
        }
        Entry::Called
    }
}

/// How `monomorphize` reaches a function.
pub(crate) enum Entry {
    /// From the start.
    Root,
    /// Once a scanned body holds a value of the type it belongs to.
    OnType(TypeHead),
    /// Only through a body naming it.
    Called,
}

impl Entry {
    /// [`Self::OnType`] the head, or [`Self::Root`] where no head says which
    /// type it waits on.
    fn on_type(head: Option<TypeHead>) -> Self {
        head.map_or(Self::Root, Self::OnType)
    }
}

/// The head of a value's type, through references, which an impl header and a
/// receiver share. `None` for a type a caller has yet to settle.
pub(crate) fn type_head(type_table: &TypeTable, id: TypeId) -> Option<TypeHead> {
    let id = type_table.peel_refs(id);
    match type_table.get(id) {
        ResolvedType::Primitive(_)
        | ResolvedType::Unit
        | ResolvedType::Struct { .. }
        | ResolvedType::Enum { .. }
        | ResolvedType::Variant { .. }
        | ResolvedType::Resource { .. }
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
fn receiver_head(method: Option<&LocalMethodName>) -> Option<TypeHead> {
    match &method?.receiver {
        Receiver::Type(fq) | Receiver::RefTo(_, fq) => settled(fq.head().clone()),
        Receiver::Ref(_) => None,
    }
}

fn settled(head: TypeHead) -> Option<TypeHead> {
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

/// Every type a value in `body` holds.
pub(crate) fn expr_types(body: &TirBlock) -> Vec<TypeId> {
    struct Types(Vec<TypeId>);
    impl TirRefVisitor for Types {
        fn visit_expr(&mut self, expr: &TirExpr) {
            self.0.push(expr.type_id);
            self.walk_expr(expr);
        }
    }
    let mut types = Types(Vec::new());
    types.walk_block(body);
    types.0
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
fn reach(flat: &FlatPackage) -> IndexSet<FunctionId> {
    let roots = Roots::new(flat);
    let type_table = flat.type_table.borrow();
    let mut bodies: IndexMap<FunctionId, &Rc<RefCell<TirFunction>>> = IndexMap::default();
    let mut work: Vec<FunctionId> = Vec::new();
    for func_rc in &flat.functions {
        let func = func_rc.borrow();
        let key = function_key(&func);
        if roots.before_lower(&func) {
            work.push(key.clone());
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
        if let Some(body) = &func_rc.borrow().body {
            work.extend(callees(body, &type_table).into_iter().map(|c| c.key));
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
            named.extend(callees(body, &type_table).into_iter().map(|c| c.key));
        }
    }
    Reached {
        reached: reach(flat),
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
        prune(flat);
    }
    found
}

/// Whether unreached functions are dropped, here and by `monomorphize`.
pub(crate) fn prunes() -> bool {
    std::env::var_os("WADO_NO_PRELOWER_PRUNE").is_none()
}

/// Drop what no root reaches, and record the drop for the assert in `lower`'s
/// `Interner::resolve`.
fn prune(flat: &mut FlatPackage) {
    let reached = reach(flat);
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
pub(crate) struct Callee {
    pub key: FunctionId,
    pub template: Option<TemplateId>,
}

/// The functions `body` names.
pub(crate) fn callees(body: &TirBlock, type_table: &TypeTable) -> Vec<Callee> {
    Callees::collect(type_table, |collector| collector.walk_block(body))
}

/// The functions `expr` names.
pub(crate) fn expr_callees(expr: &TirExpr, type_table: &TypeTable) -> Vec<Callee> {
    Callees::collect(type_table, |collector| collector.visit_expr(expr))
}

struct Callees<'a> {
    type_table: &'a TypeTable,
    callees: Vec<Callee>,
}

impl<'a> Callees<'a> {
    fn collect(type_table: &'a TypeTable, walk: impl FnOnce(&mut Self)) -> Vec<Callee> {
        let mut collector = Self {
            type_table,
            callees: Vec::new(),
        };
        walk(&mut collector);
        collector.callees
    }

    fn push(&mut self, module_source: &ModuleSource, name: &str, template: Option<&TemplateId>) {
        self.callees.push(Callee {
            key: FunctionId::free(module_source, name),
            template: template.cloned(),
        });
    }
}

impl TirRefVisitor for Callees<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Call {
                func, type_args, ..
            } => {
                self.push(&func.module_source, &func.name, func.template.as_ref());
                // `lower` rewrites the marker into a call of its bridge.
                if let Some(bridge) = bridge_callee(self.type_table, func, type_args) {
                    self.push(&bridge.module_source, &bridge.name, None);
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
