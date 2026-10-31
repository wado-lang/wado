//! What a program reaches over the monomorphized TIR, and the prune that drops
//! the rest before `lower` translates it.
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
use crate::name::{FunctionId, LocalMethodName, global_init_target, is_type_bridge};
use crate::nir_package::NirPackage;
use crate::reflect_marker::{bridge_callee, is_marker_bridge};
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirFunction};
use crate::tir_visitor::TirRefVisitor;
use crate::trace;
use crate::type_table::TypeTable;

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

/// [`MINTABLE`] as the set [`is_root`] tests an impl's trait against.
fn mintable_traits(flat: &FlatPackage) -> IndexSet<DefId> {
    let type_table = flat.type_table.borrow();
    let items = type_table.compiler_items();
    MINTABLE
        .iter()
        .filter_map(|item| items.trait_def(*item))
        .collect()
}

/// The compiler-item functions a phase from `lower` on calls with no TIR body
/// naming them. Every other one is called only from code the elaborator or
/// synthesis wrote, which the walk follows like any call.
const MINTED_AFTER_TIR: &[CompilerItem] = &[
    // `lower`
    CompilerItem::AssertFailed,
    CompilerItem::FormatterWriteLiteral,
    CompilerItem::I128FromF64,
    CompilerItem::I128FromI64,
    CompilerItem::I128FromPair,
    CompilerItem::I128FromU64,
    CompilerItem::I64Add128Soft,
    CompilerItem::I64MulWideSSoft,
    CompilerItem::I64MulWideUSoft,
    CompilerItem::I64Sub128Soft,
    CompilerItem::ListIndexAssign,
    CompilerItem::U128FromF64,
    CompilerItem::U128FromI64,
    CompilerItem::U128FromPair,
    CompilerItem::U128FromU64,
    // `optimize`
    CompilerItem::StringLen,
    CompilerItem::StringPushAscii,
    CompilerItem::StringPushChar,
    CompilerItem::StringPushStr,
    CompilerItem::StringReserveUninit,
    CompilerItem::StringSetByteUnchecked,
    CompilerItem::StringWriteStrAt,
];

/// Whether the program reaches `func` without any TIR body naming it: an
/// export, a [`MINTED_AFTER_TIR`] item, a type bridge no reflection marker
/// calls, a global initializer, a [`MINTABLE`] impl. Before monomorphization a
/// marker's type arguments may still be a template's parameters, naming no
/// bridge, so every bridge is a root there.
fn is_root(
    func: &TirFunction,
    flat: &FlatPackage,
    mintable: &IndexSet<DefId>,
    stage: Stage,
) -> bool {
    func.is_cm_export
        || (func.is_export && flat.wasm_module_sources.contains_key(&func.module_source))
        || func
            .compiler_item
            .is_some_and(|item| MINTED_AFTER_TIR.contains(&item))
        || (is_type_bridge(&func.name)
            && (stage == Stage::BeforeMonomorphize || !is_marker_bridge(&func.name)))
        || global_init_target(&func.name).is_some()
        || func
            .method_info
            .as_ref()
            .and_then(LocalMethodName::trait_decl)
            .is_some_and(|def| mintable.contains(&def))
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

/// Where in the pipeline a walk runs, which decides what it must root.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Over templates and the code calling them: a call through a type
    /// parameter names no function yet, and the drop glue an instance's
    /// elaboration adds names none.
    BeforeMonomorphize,
    BeforeLower,
}

/// A trait method by its declaration and name: what a call through a type
/// parameter dispatches to before monomorphization picks the impl.
type DispatchGroup = (DefId, String);

/// Every function the program reaches from its roots. Walks only the bodies it
/// reaches, which is the work [`prune`] saves.
fn reach(flat: &FlatPackage, stage: Stage) -> IndexSet<FunctionId> {
    let mintable = mintable_traits(flat);
    let type_table = flat.type_table.borrow();
    let drop_trait = type_table.compiler_items().trait_def(CompilerItem::Drop);
    let mut bodies: IndexMap<FunctionId, &Rc<RefCell<TirFunction>>> = IndexMap::default();
    let mut impls: IndexMap<DispatchGroup, Vec<FunctionId>> = IndexMap::default();
    let mut work: Vec<FunctionId> = Vec::new();
    for func_rc in &flat.functions {
        let func = func_rc.borrow();
        let key = function_key(&func);
        let group = dispatch_group(func.method_info.as_ref());
        // Before monomorphization a template calls another by a spelling
        // (`EnumCase<E>::holds`) no function carries, so each is a root.
        let unsettled = stage == Stage::BeforeMonomorphize
            && (func.is_template()
                || group
                    .as_ref()
                    .is_some_and(|(def, _)| Some(*def) == drop_trait));
        if unsettled || is_root(&func, flat, &mintable, stage) {
            work.push(key.clone());
        }
        if let Some(group) = group {
            impls.entry(group).or_default().push(key.clone());
        }
        bodies.insert(key, func_rc);
    }
    let mut reached: IndexSet<FunctionId> = IndexSet::default();
    let mut dispatched: IndexSet<DispatchGroup> = IndexSet::default();
    while let Some(key) = work.pop() {
        if !reached.insert(key.clone()) {
            continue;
        }
        let Some(func_rc) = bodies.get(&key) else {
            continue;
        };
        let Some(body) = &func_rc.borrow().body else {
            continue;
        };
        for callee in Callees::of_body(body, &type_table, stage) {
            // A callee no function is named is a call through a type
            // parameter, which may reach any impl of the method.
            if !bodies.contains_key(&callee.key)
                && let Some(group) = callee.dispatch
                && dispatched.insert(group.clone())
            {
                work.extend(impls.get(&group).into_iter().flatten().cloned());
            }
            work.push(callee.key);
        }
    }
    reached
}

fn dispatch_group(method: Option<&LocalMethodName>) -> Option<DispatchGroup> {
    let method = method?;
    Some((
        method.trait_decl()?,
        method.declared_method_name().to_string(),
    ))
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
                Callees::of_body(body, &type_table, Stage::BeforeLower).map(|callee| callee.key),
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

/// What every pipeline does with `flat` on its way into `monomorphize`, so
/// no instance is made for a caller nothing reaches. A template stays: what
/// it reaches is known only once an instance substitutes it.
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
        let func = func_rc.borrow();
        let key = function_key(&func);
        let keep =
            reached.contains(&key) || (stage == Stage::BeforeMonomorphize && func.is_template());
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

/// A function a body names, and the trait method a call through a type
/// parameter dispatches to.
struct Callee {
    key: FunctionId,
    dispatch: Option<DispatchGroup>,
}

struct Callees<'a> {
    type_table: &'a TypeTable,
    stage: Stage,
    callees: Vec<Callee>,
}

impl<'a> Callees<'a> {
    fn of_body(
        body: &TirBlock,
        type_table: &'a TypeTable,
        stage: Stage,
    ) -> impl Iterator<Item = Callee> {
        let mut collector = Self {
            type_table,
            stage,
            callees: Vec::new(),
        };
        collector.walk_block(body);
        collector.callees.into_iter()
    }

    fn push(&mut self, module_source: &ModuleSource, name: &str) {
        self.callees.push(Callee {
            key: FunctionId::free(module_source, name),
            dispatch: None,
        });
    }
}

impl TirRefVisitor for Callees<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Call {
                func, type_args, ..
            } => {
                self.callees.push(Callee {
                    key: FunctionId::free(&func.module_source, &func.name),
                    dispatch: dispatch_group(func.method_info.as_ref()),
                });
                // `lower` rewrites the marker into a call of its bridge. Before
                // monomorphization every bridge is a root instead.
                if self.stage == Stage::BeforeLower
                    && let Some(bridge) = bridge_callee(self.type_table, func, type_args)
                {
                    self.push(&bridge.module_source, &bridge.name);
                }
            }
            // A function used as a value: `IndirectCall` names the value, never
            // the function, so this is the only edge to what it may reach.
            TirExprKind::FuncRef {
                module_source,
                name,
                ..
            } => self.push(module_source, name),
            _ => {}
        }
        self.walk_expr(expr);
    }
}
