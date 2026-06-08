//! Per-function identity and dirty-set gating for the optimizer fixed-point
//! loop (WEP Phase 6).
//!
//! The loop runs every pass over every function each iteration, even though
//! after the first few rounds only a handful of functions still change. This
//! module lets a pass skip functions that have not changed since it last
//! processed them.
//!
//! # Identity
//!
//! [`FunctionId`] is a function's index in `NirPackage::functions`. That index
//! is immutable across one `run_optimization_passes` call — only `dce`, which
//! runs *outside* the loop, reorders the vector — so it is a stable id for the
//! gate's lifetime. It is typed (a `cranelift_entity` id, like the arena's
//! `ExprId` / `BlockId`) so the dense side-tables below are keyed without
//! passing raw indices around. It is deliberately *not* a phase-stable id:
//! cross-`dce` / cross-phase identity (for incremental compilation) would need
//! the function store to preserve ids across removal, a larger change scoped
//! out of Phase 6.
//!
//! # Model
//!
//! Each function has a monotonic `revision`. Each gated pass keeps a per-
//! function `watermark`: it processes a function only when
//! `revision > watermark`, then catches the watermark up. A pass that changes a
//! function calls [`FunctionGate::mark_changed`], bumping that function's
//! revision and — conservatively, along the 1-hop call graph — its callers and
//! callees (a callee shrinking enables inlining / constant folding in callers; a
//! call site appearing or vanishing changes a callee's dead-argument analysis).
//!
//! A pass that is not yet gate-aware reports change at package granularity; the
//! loop then calls [`FunctionGate::bump_all`], so every gated pass conservatively
//! re-examines every function. As more passes become gate-aware (reporting
//! per-function change instead of forcing `bump_all`), the steady-state skip
//! rate — and the speedup — grows.
//!
//! # Safety
//!
//! Every loop pass is an optimization; the IR is valid without it. So an
//! imprecise gate can only cost optimization *quality* (a missed rewrite),
//! never correctness. The propagation is therefore tuned conservative: when in
//! doubt, mark dirty.

use cranelift_entity::EntityRef;

use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::nir_arena::ExprKind;
use crate::nir_package::NirPackage;

/// Stable identity of a function within one optimizer run: its index in
/// `NirPackage::functions`. See the module docs.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FunctionId(u32);
cranelift_entity::entity_impl!(FunctionId, "func");

/// The gated passes. Each owns a column of per-function watermarks. Add a
/// variant when a pass becomes gate-aware; `COUNT` sizes the watermark table.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GatedPass {
    Peephole,
    CopyProp,
    ConstFold,
    Sroa,
    Cse,
    Licm,
    ConditionImplication,
    StoreLoadForward,
    TmplHoist,
    ElideBoxLocal,
    RefElim,
}

impl GatedPass {
    const COUNT: usize = 11;
}

/// Static call graph over [`FunctionId`]s, built once at loop start.
///
/// Edges resolve a call node's `FunctionRef` to a `FunctionId` through the
/// `(module_source, full_name)` key (the same key `const_folding::build_callee_map`
/// uses). Indirect calls (`ExprKind::IndirectCall`) and calls to functions
/// outside the package have no static edge and are simply absent — conservative
/// for propagation, which only risks under-optimizing.
///
/// The graph is not refreshed as bodies change: a pass that restructures calls
/// (notably `inline`) is not yet gate-aware, so it forces `bump_all`, which
/// dominates any staleness. Incremental refresh can come with those passes.
struct CallGraph {
    callees: Vec<Vec<FunctionId>>,
    callers: Vec<Vec<FunctionId>>,
}

impl CallGraph {
    fn build(project: &NirPackage) -> Self {
        let n = project.functions.len();
        let mut name_to_id: IndexMap<(ModuleSource, String), FunctionId> = IndexMap::default();
        for (i, func_rc) in project.functions.iter().enumerate() {
            let func = func_rc.borrow();
            let key = (
                func.module_source.clone(),
                crate::nir::FunctionRef::from_resolved(&func, func.module_source.clone())
                    .full_name(),
            );
            name_to_id.insert(key, FunctionId::new(i));
        }

        let mut callees: Vec<Vec<FunctionId>> = vec![Vec::new(); n];
        let mut callers: Vec<Vec<FunctionId>> = vec![Vec::new(); n];
        for (i, func_rc) in project.functions.iter().enumerate() {
            let func = func_rc.borrow();
            let Some(body) = func.body.as_ref() else {
                continue;
            };
            let mut seen: Vec<FunctionId> = Vec::new();
            for node in body.exprs.values() {
                let func_ref = match &node.kind {
                    ExprKind::Call { func, .. } | ExprKind::MethodCall { func, .. } => func,
                    _ => continue,
                };
                let key = (func_ref.module_source.clone(), func_ref.full_name());
                if let Some(&callee) = name_to_id.get(&key)
                    && !seen.contains(&callee)
                {
                    seen.push(callee);
                }
            }
            for &callee in &seen {
                callers[callee.index()].push(FunctionId::new(i));
            }
            callees[i] = seen;
        }
        Self { callees, callers }
    }
}

/// Per-function dirty-set gate. See the module docs.
pub struct FunctionGate {
    revision: Vec<u64>,
    watermarks: [Vec<u64>; GatedPass::COUNT],
    graph: CallGraph,
}

impl FunctionGate {
    /// Build the gate for one optimizer run. Every function starts dirty
    /// (`revision = 1`, watermarks `0`), so the first iteration processes
    /// everything.
    pub fn new(project: &NirPackage) -> Self {
        let n = project.functions.len();
        Self {
            revision: vec![1; n],
            watermarks: std::array::from_fn(|_| vec![0; n]),
            graph: CallGraph::build(project),
        }
    }

    /// Grow the side-tables to cover `len` functions. A pass may add functions
    /// mid-loop (`value_copy_demote` specializes shallow copies), so the gate
    /// cannot assume a fixed count. New functions start dirty (`revision = 1`,
    /// watermark `0`) with no known call-graph edges; the adding pass is not
    /// gate-aware and `bump_all`s, which covers the missing edges conservatively.
    fn ensure(&mut self, len: usize) {
        while self.revision.len() < len {
            self.revision.push(1);
            for w in &mut self.watermarks {
                w.push(0);
            }
            self.graph.callees.push(Vec::new());
            self.graph.callers.push(Vec::new());
        }
    }

    /// Whether `pass` should process `func` (it changed since `pass` last saw
    /// it).
    pub fn needs(&mut self, pass: GatedPass, func: FunctionId) -> bool {
        self.ensure(func.index() + 1);
        self.revision[func.index()] > self.watermarks[pass as usize][func.index()]
    }

    /// Record that `pass` has processed `func` at its current revision.
    pub fn seen(&mut self, pass: GatedPass, func: FunctionId) {
        self.ensure(func.index() + 1);
        self.watermarks[pass as usize][func.index()] = self.revision[func.index()];
    }

    /// Record that `func`'s body changed: bump its revision and, conservatively,
    /// its 1-hop call-graph neighbours (callers and callees).
    pub fn mark_changed(&mut self, func: FunctionId) {
        self.ensure(func.index() + 1);
        let i = func.index();
        self.revision[i] += 1;
        for &c in &self.graph.callers[i] {
            self.revision[c.index()] += 1;
        }
        for &c in &self.graph.callees[i] {
            self.revision[c.index()] += 1;
        }
    }

    /// Conservatively mark every function dirty for every gated pass. Used when a
    /// pass that is not gate-aware reports a package-level change, since the gate
    /// cannot tell which functions it touched.
    pub fn bump_all(&mut self) {
        for r in &mut self.revision {
            *r += 1;
        }
    }

    /// Drive a gate-aware per-function pass: call `f` only for the functions
    /// `pass` needs to (re)process, marking each seen afterwards and bumping the
    /// gate when `f` reports a change. Returns whether any function changed.
    /// `len` is the current function count (read once; these passes do not add
    /// functions mid-pass).
    pub fn run_gated(
        &mut self,
        pass: GatedPass,
        len: usize,
        mut f: impl FnMut(FunctionId) -> bool,
    ) -> bool {
        let mut any = false;
        for i in 0..len {
            let fid = FunctionId::new(i);
            if !self.needs(pass, fid) {
                continue;
            }
            let changed = f(fid);
            self.seen(pass, fid);
            if changed {
                self.mark_changed(fid);
                any = true;
            }
        }
        any
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a gate with `n` functions and an explicit call graph, bypassing
    /// `NirPackage` so the propagation algebra can be tested in isolation.
    fn gate_with_graph(n: usize, edges: &[(usize, usize)]) -> FunctionGate {
        let mut callees: Vec<Vec<FunctionId>> = vec![Vec::new(); n];
        let mut callers: Vec<Vec<FunctionId>> = vec![Vec::new(); n];
        for &(caller, callee) in edges {
            callees[caller].push(FunctionId::new(callee));
            callers[callee].push(FunctionId::new(caller));
        }
        FunctionGate {
            revision: vec![1; n],
            watermarks: std::array::from_fn(|_| vec![0; n]),
            graph: CallGraph { callees, callers },
        }
    }

    #[test]
    fn fresh_gate_needs_every_function() {
        let mut gate = gate_with_graph(3, &[]);
        for i in 0..3 {
            assert!(gate.needs(GatedPass::Peephole, FunctionId::new(i)));
        }
    }

    #[test]
    fn seen_clears_need_until_next_change() {
        let mut gate = gate_with_graph(2, &[]);
        let f = FunctionId::new(0);
        gate.seen(GatedPass::Peephole, f);
        assert!(!gate.needs(GatedPass::Peephole, f));
        // Another pass is unaffected by Peephole catching up.
        assert!(gate.needs(GatedPass::CopyProp, f));
        gate.mark_changed(f);
        assert!(gate.needs(GatedPass::Peephole, f));
    }

    #[test]
    fn mark_changed_propagates_one_hop_both_directions() {
        // 0 -> 1 -> 2 (0 calls 1, 1 calls 2).
        let mut gate = gate_with_graph(3, &[(0, 1), (1, 2)]);
        for p in [GatedPass::Peephole, GatedPass::CopyProp] {
            for i in 0..3 {
                gate.seen(p, FunctionId::new(i));
            }
        }
        // Changing the middle function dirties its caller (0) and callee (2).
        gate.mark_changed(FunctionId::new(1));
        assert!(gate.needs(GatedPass::Peephole, FunctionId::new(0)));
        assert!(gate.needs(GatedPass::Peephole, FunctionId::new(1)));
        assert!(gate.needs(GatedPass::Peephole, FunctionId::new(2)));
    }

    #[test]
    fn mark_changed_leaves_non_neighbours_clean() {
        // 0 -> 1; function 2 is unrelated.
        let mut gate = gate_with_graph(3, &[(0, 1)]);
        for i in 0..3 {
            gate.seen(GatedPass::Peephole, FunctionId::new(i));
        }
        gate.mark_changed(FunctionId::new(0));
        assert!(!gate.needs(GatedPass::Peephole, FunctionId::new(2)));
    }

    #[test]
    fn bump_all_dirties_everything() {
        let mut gate = gate_with_graph(3, &[]);
        for i in 0..3 {
            gate.seen(GatedPass::Peephole, FunctionId::new(i));
        }
        gate.bump_all();
        for i in 0..3 {
            assert!(gate.needs(GatedPass::Peephole, FunctionId::new(i)));
        }
    }
}
