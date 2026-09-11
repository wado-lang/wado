//! What a program reaches over the monomorphized TIR, and what only `lower`
//! and `optimize` name.
//!
//! `WADO_TRACE=prelower_reach` prints the difference. See WEP 2026-05-26,
//! "A prune before `lower` is guessing".

use std::cell::RefCell;
use std::rc::Rc;

use crate::compiler_trace;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::name::FunctionId;
use crate::nir_package::NirPackage;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirFunction};
use crate::tir_visitor::TirRefVisitor;
use crate::trace;

const TRACE_TARGET: &str = "prelower_reach";

fn enabled() -> bool {
    trace::filter().enabled(TRACE_TARGET)
}

/// The identity `nir::FunctionRef::function_id` keys on. A mangled instance
/// carries its type arguments in `name`, so module and name alone settle it.
fn function_key(func: &TirFunction) -> FunctionId {
    FunctionId::free(&func.module_source, &func.name)
}

/// The exports the emitted component keeps, matching `optimize::dce`'s entries,
/// plus what a later phase may call without any TIR body naming it: a compiler
/// item the compiler resolves itself, and a `$`-prefixed synthesized bridge.
fn is_root(func: &TirFunction, flat: &FlatPackage) -> bool {
    func.is_cm_export
        || (func.is_export && flat.wasm_module_sources.contains_key(&func.module_source))
        || func.compiler_item.is_some()
        || func.name.starts_with('$')
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

/// Every function the program reaches from its roots, plus what a global
/// initializer calls — reify emits every global, so its calls are live. Walks
/// only the bodies it reaches, which is the work [`prune`] saves.
fn reach(flat: &FlatPackage) -> IndexSet<FunctionId> {
    let mut bodies: IndexMap<FunctionId, &Rc<RefCell<TirFunction>>> = IndexMap::default();
    let mut work: Vec<FunctionId> = Vec::new();
    for func_rc in &flat.functions {
        let func = func_rc.borrow();
        let key = function_key(&func);
        if is_root(&func, flat) {
            work.push(key.clone());
        }
        bodies.insert(key, func_rc);
    }
    for global in &flat.globals {
        work.extend(Callees::of_expr(global.init.slot_expr()));
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
            work.extend(Callees::of_body(body));
        }
    }
    reached
}

/// [`reach`], against the population it walked, for [`audit`] to report.
fn reachable(flat: &FlatPackage) -> Reached {
    let mut present: IndexSet<FunctionId> = IndexSet::default();
    let mut named: IndexSet<FunctionId> = IndexSet::default();
    let mut bodied = 0;
    for func_rc in &flat.functions {
        let func = func_rc.borrow();
        present.insert(function_key(&func));
        if let Some(body) = &func.body {
            bodied += 1;
            named.extend(Callees::of_body(body));
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
pub(crate) fn before_lower(flat: &mut FlatPackage) -> Option<Reached> {
    let found = enabled().then(|| reachable(flat));
    if std::env::var_os("WADO_PRELOWER_PRUNE").is_some() {
        prune(flat);
    }
    found
}

/// Drop what no root reaches, so `lower` never translates it. Unsound, and so
/// off by default: see WEP 2026-05-26, "A prune before `lower` is guessing".
fn prune(flat: &mut FlatPackage) {
    let reached = reach(flat);
    let before = flat.functions.len();
    flat.functions
        .retain(|func_rc| reached.contains(&function_key(&func_rc.borrow())));
    compiler_trace!(
        TRACE_TARGET,
        "pruned {} of {before} functions",
        before - flat.functions.len()
    );
}

#[derive(Default)]
struct Callees {
    keys: Vec<FunctionId>,
}

impl Callees {
    fn of_body(body: &TirBlock) -> Vec<FunctionId> {
        let mut collector = Self::default();
        collector.walk_block(body);
        collector.keys
    }

    fn of_expr(expr: &TirExpr) -> Vec<FunctionId> {
        let mut collector = Self::default();
        collector.visit_expr(expr);
        collector.keys
    }
}

impl TirRefVisitor for Callees {
    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Call { func, .. } => self
                .keys
                .push(FunctionId::free(&func.module_source, &func.name)),
            // A function used as a value: `IndirectCall` names the value, never
            // the function, so this is the only edge to what it may reach.
            TirExprKind::FuncRef {
                module_source,
                name,
                ..
            } => self.keys.push(FunctionId::free(module_source, name)),
            _ => {}
        }
        self.walk_expr(expr);
    }
}
