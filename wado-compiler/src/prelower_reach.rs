//! What a program reaches over the monomorphized TIR, and what only `lower`
//! and `optimize` name.
//!
//! `WADO_TRACE=prelower_reach` prints the difference: every function that
//! survives `optimize` although the walk below never reached it. Each one is a
//! call a later phase mints, and a prune before `lower` needs all of them
//! declared.
//!
//! It sees only survivors, so a clean report is not a clean bill: a minter
//! whose target [`prune`] already dropped never reaches `optimize` — it panics
//! in `wir_build` instead. Compiling the corpus with and without the prune and
//! comparing the bytes is what covers that.

use std::cell::RefCell;
use std::rc::Rc;

use crate::compiler_trace;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::name::{FreeFunctionName, FunctionId};
use crate::nir_package::NirPackage;
use crate::tir::{TirExpr, TirExprKind, TirFunction};
use crate::tir_visitor::TirRefVisitor;
use crate::trace;

pub(crate) const TRACE_TARGET: &str = "prelower_reach";

pub(crate) fn enabled() -> bool {
    trace::filter().enabled(TRACE_TARGET)
}

/// The identity `nir::FunctionRef::function_id` keys on. Module and name alone:
/// a mangled instance carries its type arguments in `name`, and neither
/// `method_info` nor `monomorph_info` takes part.
///
/// The `ModuleSource` goes in whole rather than rendered, because `Display`
/// drops the variant: a `Local` and a `Wasm` module spelling the same path
/// would share a key, and the body one of them reaches would go unwalked.
fn key(module_source: &ModuleSource, name: &str) -> FunctionId {
    FunctionId::Free(FreeFunctionName::from_module_source(module_source, name))
}

fn function_key(func: &TirFunction) -> FunctionId {
    key(&func.module_source, &func.name)
}

/// The exports the emitted component keeps, matching `optimize::dce`'s entries,
/// plus what a later phase may call without any TIR body naming it.
///
/// A compiler item is a name the compiler itself resolves, so a rewrite in
/// `lower` or `optimize` can mint the call. A `$`-prefixed name is a synthesized
/// bridge, which lowering names from a `builtin::` marker call's type arguments
/// rather than from the helper.
fn is_root(func: &TirFunction, flat: &FlatPackage) -> bool {
    func.is_cm_export
        || (func.is_export && flat.wasm_module_sources.contains_key(&func.module_source))
        || func.compiler_item.is_some()
        || func.name.starts_with('$')
}

/// What [`reachable`] found, against the population it walked. A survivor
/// outside `present` is a function a later phase created — a specialization,
/// a functor, a helper — which no prune before `lower` could have dropped.
pub(crate) struct Reached {
    pub(crate) present: IndexSet<FunctionId>,
    pub(crate) reached: IndexSet<FunctionId>,
    /// Every callee any TIR body names, reached or not. A survivor outside it
    /// is one a later phase mints; a survivor inside it is only unreached
    /// because its caller is, so the gap to close is the caller's.
    pub(crate) named: IndexSet<FunctionId>,
    /// How many of `present` carry a body, which is what `lower` translates.
    pub(crate) bodied: usize,
}

/// Every function the program reaches from its roots, plus what a global
/// initializer calls — reify emits every global, so its calls are live.
///
/// Walks only the bodies it reaches, which is the point: the unreached ones are
/// the work [`prune`] saves, and touching them here would spend it.
fn reach(flat: &FlatPackage) -> IndexSet<FunctionId> {
    let mut bodies: IndexMap<FunctionId, &Rc<RefCell<TirFunction>>> = IndexMap::default();
    for func_rc in &flat.functions {
        bodies.insert(function_key(&func_rc.borrow()), func_rc);
    }

    let mut work: Vec<FunctionId> = Vec::new();
    for func_rc in &flat.functions {
        let func = func_rc.borrow();
        if is_root(&func, flat) {
            work.push(function_key(&func));
        }
    }
    for global in &flat.globals {
        work.extend(callees(global.init.slot_expr()));
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
        if let Some(body) = &func.body {
            let mut collector = Callees::default();
            collector.walk_block(body);
            work.extend(collector.keys);
        }
    }
    reached
}

/// [`reach`], against the population it walked, for [`audit`] to report.
pub(crate) fn reachable(flat: &FlatPackage) -> Reached {
    let mut present: IndexSet<FunctionId> = IndexSet::default();
    let mut named: IndexSet<FunctionId> = IndexSet::default();
    let mut bodied = 0;
    for func_rc in &flat.functions {
        let func = func_rc.borrow();
        present.insert(function_key(&func));
        if let Some(body) = &func.body {
            bodied += 1;
            let mut collector = Callees::default();
            collector.walk_block(body);
            named.extend(collector.keys);
        }
    }
    Reached {
        reached: reach(flat),
        present,
        named,
        bodied,
    }
}

/// Report the functions `optimize` kept that were present at TIR and that
/// [`reachable`] never reached — the calls `lower` and `optimize` mint.
pub(crate) fn audit(found: &Reached, package: &NirPackage) {
    // `dce` marks rather than removes, so the live set is what it left bodied.
    let live: Vec<FunctionId> = package
        .functions
        .iter()
        .filter_map(|func_rc| {
            let func = func_rc.borrow();
            (!func.is_dead).then(|| key(&func.module_source, &func.name))
        })
        .collect();
    let mut minted: Vec<FunctionId> = live
        .iter()
        .filter(|name| found.present.contains(*name) && !found.reached.contains(*name))
        .cloned()
        .collect();
    minted.sort_unstable_by_key(ToString::to_string);
    minted.dedup();
    let (chained, unnamed): (Vec<&FunctionId>, Vec<&FunctionId>) =
        minted.iter().partition(|name| found.named.contains(*name));
    compiler_trace!(
        TRACE_TARGET,
        "reached {} of {} names ({} bodied) at TIR; of {} survivors, \
         {} are minted later and {} follow an unreached caller",
        found.reached.len(),
        found.present.len(),
        found.bodied,
        live.len(),
        unnamed.len(),
        chained.len()
    );
    for name in &unnamed {
        compiler_trace!(TRACE_TARGET, "  minted  {name}");
    }
    for name in &chained {
        compiler_trace!(TRACE_TARGET, "  chained {name}");
    }
}

/// What every pipeline does with `flat` on its way into `lower`: snapshot what
/// the program reaches for [`audit`], and prune to it when asked. One entry
/// point because `compile` and `dump` lower separately, and a policy spelled at
/// each of them is one they can disagree on.
pub(crate) fn before_lower(flat: &mut FlatPackage) -> Option<Reached> {
    let found = enabled().then(|| reachable(flat));
    if std::env::var_os("WADO_PRELOWER_PRUNE").is_some() {
        prune(flat);
    }
    found
}

/// Drop what no root reaches, so `lower` never translates it.
///
/// Unsound as it stands, which is why it is off by default: [`is_root`] cannot
/// see a callee `lower` names from a node that is not a call — a match pattern
/// (`lower/translate/pattern.rs`), a wide-int literal
/// (`lower/wide_int_literal.rs`), a `builtin::variant_tag` marker
/// (`lower/translate.rs`), or a synthesized closure-functor body
/// (`lower/plan/closure.rs`). All four land in `Interner::resolve`, so that is
/// where a sound prune has to be answered.
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

fn callees(expr: &TirExpr) -> Vec<FunctionId> {
    let mut collector = Callees::default();
    collector.visit_expr(expr);
    collector.keys
}

#[derive(Default)]
struct Callees {
    keys: Vec<FunctionId>,
}

impl TirRefVisitor for Callees {
    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Call { func, .. } => self.keys.push(key(&func.module_source, &func.name)),
            // A function used as a value: `IndirectCall` names the value, never
            // the function, so this is the only edge to what it may reach.
            TirExprKind::FuncRef {
                module_source,
                name,
                ..
            } => self.keys.push(key(module_source, name)),
            _ => {}
        }
        self.walk_expr(expr);
    }
}
