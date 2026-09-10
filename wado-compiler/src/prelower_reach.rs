//! What a program reaches over the monomorphized TIR, and what only `lower`
//! and `optimize` name.
//!
//! `WADO_TRACE=prelower_reach` prints the difference: every function that
//! survives `optimize` although the walk below never reached it. Each one is a
//! call a later phase mints, and a prune before `lower` needs all of them
//! declared. Keyed by `FunctionRef::full_name`, which the TIR and NIR forms
//! spell alike; that key merges a generic's instances, so a callee minted for
//! one instance of a reached generic does not show up.

use std::cell::RefCell;
use std::rc::Rc;

use crate::compiler_trace;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::nir;
use crate::nir_package::NirPackage;
use crate::tir::{FunctionRef, TirExpr, TirExprKind, TirFunction};
use crate::tir_visitor::TirRefVisitor;
use crate::trace;

pub(crate) const TRACE_TARGET: &str = "prelower_reach";

pub(crate) fn enabled() -> bool {
    trace::filter().enabled(TRACE_TARGET)
}

fn function_key(func: &TirFunction) -> String {
    FunctionRef::from_resolved(func, func.module_source.clone()).full_name()
}

/// The exports the emitted component keeps, matching `optimize::dce`'s entries.
fn is_root(func: &TirFunction, flat: &FlatPackage) -> bool {
    func.is_cm_export
        || (func.is_export && flat.wasm_module_sources.contains_key(&func.module_source))
}

/// What [`reachable`] found, against the population it walked. A survivor
/// outside `present` is a function a later phase created — a specialization,
/// a functor, a helper — which no prune before `lower` could have dropped.
pub(crate) struct Reached {
    pub(crate) present: IndexSet<String>,
    pub(crate) reached: IndexSet<String>,
    /// How many of `present` carry a body, which is what `lower` translates.
    pub(crate) bodied: usize,
}

/// Every function the program reaches from its exports, plus what a global
/// initializer calls — reify emits every global, so its calls are live.
pub(crate) fn reachable(flat: &FlatPackage) -> Reached {
    let mut bodies: IndexMap<String, &Rc<RefCell<TirFunction>>> = IndexMap::default();
    for func_rc in &flat.functions {
        bodies.insert(function_key(&func_rc.borrow()), func_rc);
    }

    let mut work: Vec<String> = Vec::new();
    for func_rc in &flat.functions {
        let func = func_rc.borrow();
        if is_root(&func, flat) {
            work.push(function_key(&func));
        }
    }
    for global in &flat.globals {
        work.extend(callees(global.init.slot_expr()));
    }

    let mut reached: IndexSet<String> = IndexSet::default();
    while let Some(key) = work.pop() {
        if !reached.insert(key.clone()) {
            continue;
        }
        let Some(func_rc) = bodies.get(&key) else {
            continue;
        };
        let func = func_rc.borrow();
        if let Some(body) = &func.body {
            let mut collector = Callees::default();
            collector.walk_block(body);
            work.extend(collector.keys);
        }
    }
    Reached {
        bodied: flat
            .functions
            .iter()
            .filter(|func_rc| func_rc.borrow().body.is_some())
            .count(),
        present: bodies.into_keys().collect(),
        reached,
    }
}

/// Report the functions `optimize` kept that were present at TIR and that
/// [`reachable`] never reached — the calls `lower` and `optimize` mint.
pub(crate) fn audit(found: &Reached, package: &NirPackage) {
    // `dce` marks rather than removes, so the live set is what it left bodied.
    let live: Vec<String> = package
        .functions
        .iter()
        .filter_map(|func_rc| {
            let func = func_rc.borrow();
            (!func.is_dead)
                .then(|| nir::FunctionRef::from_resolved(&func, func.module_source.clone()))
                .map(|r| r.full_name())
        })
        .collect();
    let mut minted: Vec<String> = live
        .iter()
        .filter(|name| found.present.contains(*name) && !found.reached.contains(*name))
        .cloned()
        .collect();
    minted.sort_unstable();
    minted.dedup();
    compiler_trace!(
        TRACE_TARGET,
        "reached {} of {} names ({} bodied) at TIR; \
         {} of {} survivors were at TIR and unreached",
        found.reached.len(),
        found.present.len(),
        found.bodied,
        minted.len(),
        live.len()
    );
    for name in &minted {
        compiler_trace!(TRACE_TARGET, "  {name}");
    }
}

fn callees(expr: &TirExpr) -> Vec<String> {
    let mut collector = Callees::default();
    collector.walk_expr(expr);
    collector.keys
}

#[derive(Default)]
struct Callees {
    keys: Vec<String>,
}

impl TirRefVisitor for Callees {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if let TirExprKind::Call { func, .. } = &expr.kind {
            self.keys.push(func.full_name());
        }
        self.walk_expr(expr);
    }
}
