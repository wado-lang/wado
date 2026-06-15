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
//! Every fixed-point loop pass is gate-aware, so every change is reported at
//! function granularity: a per-function pass skips functions it has already
//! processed at their current revision (via [`FunctionGate::run_gated`]); an
//! interprocedural pass pulls its candidate worklist from the dirty set (via
//! [`FunctionGate::dirty_funcs`]) and reports exactly the ones it touched (via
//! [`FunctionGate::mark_changed`]).
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
use crate::nir_engine::CachedAnalysis;
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
    TmplHoist,
    ContainerSroa,
    Inline,
    Dae,
    Drve,
    SroaParam,
    ValueCopyDemote,
}

impl GatedPass {
    const COUNT: usize = 13;
}

/// Static call graph over [`FunctionId`]s, built once at loop start.
///
/// Edges resolve a call node's `FunctionRef` to a `FunctionId` through the
/// `(module_source, full_name)` key (the same key `const_folding::build_callee_map`
/// uses). Indirect calls (`ExprKind::IndirectCall`) and calls to functions
/// outside the package have no static edge and are simply absent — conservative
/// for propagation, which only risks under-optimizing.
///
/// The graph is built once and not refreshed as bodies change. A pass that
/// restructures calls (`inline` copies a callee body in; `container_sroa` /
/// `value_copy_demote` retarget calls to per-field / shallow-copy callees)
/// leaves the rewritten function's out-edges stale. That only reduces the
/// precision of 1-hop dirty propagation — a quality knob, never correctness,
/// since every loop pass is optional (see the module safety note) — and the
/// rewritten function is itself reported dirty regardless. Incremental refresh
/// could improve propagation precision but is not needed for soundness.
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
    /// Parked per-function engine analysis (`ValueGraph` + alias sets), tagged
    /// with the `revision` it was built at. A pass reuses it when the function
    /// is unchanged since the park (`revision` matches); `revision` bumps on the
    /// function's own change *and* any neighbour change, which over-covers the
    /// analysis's real dependency (body + callee immutability), so a reuse is
    /// never stale. Kept live across the loop's two value-graph sessions: the
    /// `cse` + `store_load_forward` session parks a param-seeded graph, and the
    /// `licm` + `condition_implication` session reuses it for every function
    /// `const_fold` leaves unchanged between them — so one build serves both
    /// instead of two. (`cse` seeds params too, making the two configs identical;
    /// seeding is equivalence-invariant for cse/slf, which test only `ValueId`
    /// equality.) A function changed by `const_fold` falls back to a fresh build.
    vg_cache: Vec<Option<(u64, CachedAnalysis)>>,
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
            vg_cache: (0..n).map(|_| None).collect(),
        }
    }

    /// Grow the side-tables to cover `len` functions. A pass may add functions
    /// mid-loop (`value_copy_demote` appends shallow-copy specializations), so
    /// the gate cannot assume a fixed count. New functions start dirty
    /// (`revision = 1`, watermark `0`) with no known call-graph edges, so every
    /// gated pass processes them at least once; their missing edges only reduce
    /// 1-hop propagation precision (quality, not correctness — see the module
    /// safety note).
    fn ensure(&mut self, len: usize) {
        while self.revision.len() < len {
            self.revision.push(1);
            for w in &mut self.watermarks {
                w.push(0);
            }
            self.graph.callees.push(Vec::new());
            self.graph.callers.push(Vec::new());
        }
        while self.vg_cache.len() < len {
            self.vg_cache.push(None);
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

    /// The functions `pass` must (re)examine this round, each marked seen.
    pub fn dirty_funcs(&mut self, pass: GatedPass, len: usize) -> Vec<FunctionId> {
        (0..len)
            .map(FunctionId::new)
            .filter(|&fid| {
                let dirty = self.needs(pass, fid);
                if dirty {
                    self.seen(pass, fid);
                }
                dirty
            })
            .collect()
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

    /// Like [`Self::run_gated`], for the value-graph passes that share the
    /// [`vg_cache`](Self::vg_cache). For each dirty function it hands `f` the
    /// parked analysis (if the function is unchanged since the park) and parks
    /// the returned analysis back when `f` reports no change. A changed function
    /// has its revision bumped, so its stale parked entry is never reused.
    pub fn run_gated_cached(
        &mut self,
        pass: GatedPass,
        len: usize,
        mut f: impl FnMut(FunctionId, Option<CachedAnalysis>) -> (bool, Option<CachedAnalysis>),
    ) -> bool {
        let mut any = false;
        for i in 0..len {
            let fid = FunctionId::new(i);
            if !self.needs(pass, fid) {
                continue;
            }
            let cached = self.take_analysis(fid);
            let (changed, analysis) = f(fid, cached);
            self.seen(pass, fid);
            if changed {
                self.mark_changed(fid);
                any = true;
            }
            // Park the returned analysis even when the pass changed the function:
            // a pass that returns `Some` after a change asserts its edits left the
            // graph valid (value-preserving), so the next value-graph pass reuses
            // it instead of rebuilding. Tagged at the post-`mark_changed` revision.
            if let Some(analysis) = analysis {
                self.park_analysis(fid, analysis);
            }
        }
        any
    }

    /// Re-tag the parked `ValueGraph` for `func` to its current revision,
    /// asserting it survived the calling pass's value-preserving edits — so the
    /// next value-graph pass reuses it instead of rebuilding. Used by
    /// `const_fold`, whose folds are graph-preserving (the graph already holds
    /// the folded constants via `current_value` / `fold_binary_const`). No-op if
    /// nothing is parked for `func`.
    pub fn carry_vg_cache(&mut self, func: FunctionId) {
        let i = func.index();
        let cur = self.revision[i];
        if let Some(Some((rev, _))) = self.vg_cache.get_mut(i).map(|s| s.as_mut()) {
            *rev = cur;
        }
    }

    /// Take the parked analysis for `func` if it is still valid (parked at the
    /// current revision). A stale entry is dropped.
    fn take_analysis(&mut self, func: FunctionId) -> Option<CachedAnalysis> {
        let i = func.index();
        match self.vg_cache.get_mut(i)?.take() {
            Some((rev, analysis)) if rev == self.revision[i] => Some(analysis),
            _ => None,
        }
    }

    /// Park `analysis` for `func`, tagged with its current revision.
    fn park_analysis(&mut self, func: FunctionId, analysis: CachedAnalysis) {
        self.ensure(func.index() + 1);
        let i = func.index();
        self.vg_cache[i] = Some((self.revision[i], analysis));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `GatedPass::COUNT` is hand-maintained and sizes the watermark table
    /// (`[Vec<u64>; COUNT]` indexed by `pass as usize`), so a variant added
    /// without bumping COUNT would panic at runtime. The exhaustive `match`
    /// forces this test to be updated when a variant is added (compile error),
    /// and the assert then catches a stale COUNT.
    #[test]
    fn gated_pass_count_matches_variants() {
        let all = [
            GatedPass::Peephole,
            GatedPass::CopyProp,
            GatedPass::ConstFold,
            GatedPass::Sroa,
            GatedPass::Cse,
            GatedPass::Licm,
            GatedPass::TmplHoist,
            GatedPass::ContainerSroa,
            GatedPass::Inline,
            GatedPass::Dae,
            GatedPass::Drve,
            GatedPass::SroaParam,
            GatedPass::ValueCopyDemote,
        ];
        for p in all {
            match p {
                GatedPass::Peephole
                | GatedPass::CopyProp
                | GatedPass::ConstFold
                | GatedPass::Sroa
                | GatedPass::Cse
                | GatedPass::Licm
                | GatedPass::TmplHoist
                | GatedPass::ContainerSroa
                | GatedPass::Inline
                | GatedPass::Dae
                | GatedPass::Drve
                | GatedPass::SroaParam
                | GatedPass::ValueCopyDemote => {}
            }
        }
        assert_eq!(all.len(), GatedPass::COUNT);
    }

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
            vg_cache: (0..n).map(|_| None).collect(),
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
    fn run_gated_processes_only_dirty_and_reports_changes() {
        // 0 -> 1; functions 0,1,2. Mark all seen for Peephole, then run a
        // CopyProp pass that changes function 2; only dirty functions are
        // visited, and the change propagates to function 2's (none) neighbours.
        let mut gate = gate_with_graph(3, &[(0, 1)]);
        for i in 0..3 {
            gate.seen(GatedPass::CopyProp, FunctionId::new(i));
        }
        // Nothing dirty for CopyProp now: run_gated visits nothing.
        let mut visited = Vec::new();
        let changed = gate.run_gated(GatedPass::CopyProp, 3, |fid| {
            visited.push(fid);
            false
        });
        assert!(visited.is_empty());
        assert!(!changed);
        // Dirty function 1 (and its caller 0 via propagation), then run again.
        gate.mark_changed(FunctionId::new(1));
        let mut visited = Vec::new();
        gate.run_gated(GatedPass::CopyProp, 3, |fid| {
            visited.push(fid);
            false
        });
        assert_eq!(visited, vec![FunctionId::new(0), FunctionId::new(1)]);
    }
}
