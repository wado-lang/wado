//! Per-function dirty-set gating for the optimizer fixed-point loop, so a pass
//! skips what has not changed since it last ran. Each function carries a
//! monotonic `revision` and each pass a per-function `watermark`;
//! [`FunctionGate::mark_changed`] bumps the revision and, conservatively, its
//! 1-hop callers and callees. Keyed by [`FuncId`], the index in `functions`.
//!
//! Every loop pass is optional, the IR being valid without it, so an imprecise
//! gate costs optimization quality and never correctness. When in doubt, the
//! propagation marks dirty. What a rewrite stales is the memos' to notice, by
//! each function's write count (`super::body_memo`), not the gate's.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cranelift_entity::EntityRef;

use crate::hashmap::IndexSet;
use crate::nir::FuncId;
use crate::nir_arena::ExprKind;
use crate::nir_engine::EngineBuffers;
use crate::nir_package::NirPackage;
use crate::parallel::Executor;

/// The gated passes. Each owns a column of per-function watermarks. Add a
/// variant when a pass becomes gate-aware; `COUNT` sizes the watermark table.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GatedPass {
    /// Pre-inline peephole run (hosts `MatchToSwitchRule`, `string_push`, …).
    /// Kept on a separate watermark column from the post-inline run: the two
    /// invocations apply different rule sets, so a function quiescent for the
    /// pre-inline run must still be revisited by the post-inline run (which
    /// hosts `RefElimRule` / `ElideBoxLocalRule` / `LabeledBlockFusionRule` /
    /// `array_literal`), and vice versa.
    PeepholePre,
    /// Post-inline peephole run.
    PeepholePost,
    CopyProp,
    ConstFold,
    Sroa,
    Licm,
    TmplHoist,
    ContainerSroa,
    Inline,
    Dae,
    Drve,
    SroaParam,
    SroaVariantReturn,
    ValueCopyDemote,
    ScalarForward,
    LetBlockFlatten,
    /// Whole-program only: the column says whether any function changed since
    /// the pass last ran, never which ones to skip.
    ParamSpec,
    /// The post-loop cleanup fixpoints ([`super::run_bounded_fixpoint`]). Each
    /// owns a fresh gate, so `BranchPrune` serves both of its passes.
    StoreLoadForward,
    BranchPrune,
    CondImplPostPromote,
}

impl GatedPass {
    const COUNT: usize = 20;
}

/// Static call graph over [`FuncId`]s, built once at loop start from each call
/// node's stamped `func_id`. An indirect or out-of-package call has no edge, and
/// a pass that restructures calls leaves its out-edges stale — both only cost
/// 1-hop propagation precision, never correctness, and the rewritten function is
/// reported dirty regardless.
struct CallGraph {
    callees: Vec<Vec<FuncId>>,
    callers: Vec<Vec<FuncId>>,
}

impl CallGraph {
    fn build(project: &NirPackage) -> Self {
        let n = project.functions.len();
        // Read the callee off each call node's stamped `func_id` (`FuncId ==
        // store position`, Phase 4). Every call is born resolved (Phase 5d), so
        // `func_id` is total here — a `None` would be an unstamped call the
        // graph conservatively ignores.
        let mut callees: Vec<Vec<FuncId>> = vec![Vec::new(); n];
        let mut callers: Vec<Vec<FuncId>> = vec![Vec::new(); n];
        for (i, func_rc) in project.functions.iter().enumerate() {
            let func = func_rc.borrow();
            let Some(body) = func.body.as_ref() else {
                continue;
            };
            let mut seen: IndexSet<FuncId> = IndexSet::default();
            for node in body.exprs.values() {
                let func_id = match &node.kind {
                    ExprKind::Call { func_id, .. } => func_id,
                    _ => continue,
                };
                seen.insert(*func_id);
            }
            for &callee in &seen {
                callers[callee.index()].push(FuncId::new(i));
            }
            callees[i] = seen.into_iter().collect();
        }
        Self { callees, callers }
    }
}

/// Per-function dirty-set gate. See the module docs.
pub struct FunctionGate {
    id: u64,
    revision: Vec<u64>,
    watermarks: [Vec<u64>; GatedPass::COUNT],
    graph: CallGraph,
    /// The threads a sweep's visits run on.
    exec: Arc<Executor>,
}

/// Tells one gate's state from another's.
static NEXT_GATE_ID: AtomicU64 = AtomicU64::new(0);

impl FunctionGate {
    /// Build the gate for one optimizer run, its sweeps running on `exec`.
    /// Every function starts dirty (`revision = 1`, watermarks `0`), so the
    /// first iteration processes everything.
    pub fn new(project: &NirPackage, exec: &Arc<Executor>) -> Self {
        let n = project.functions.len();
        Self::with_graph(n, CallGraph::build(project), Arc::clone(exec))
    }

    fn with_graph(n: usize, graph: CallGraph, exec: Arc<Executor>) -> Self {
        Self {
            id: NEXT_GATE_ID.fetch_add(1, Ordering::Relaxed),
            revision: vec![1; n],
            watermarks: std::array::from_fn(|_| vec![0; n]),
            graph,
            exec,
        }
    }

    /// Which gate this is, so state kept for one optimizer run is never read
    /// against another's.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The threads this gate's sweeps run on, for the whole-program walks a
    /// pass takes before its sweep.
    pub fn exec(&self) -> &Arc<Executor> {
        &self.exec
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
    }

    /// Whether `pass` should process `func` (it changed since `pass` last saw
    /// it).
    pub fn needs(&mut self, pass: GatedPass, func: FuncId) -> bool {
        self.ensure(func.index() + 1);
        self.revision[func.index()] > self.watermarks[pass as usize][func.index()]
    }

    /// Record that `pass` has processed `func` at its current revision.
    pub fn seen(&mut self, pass: GatedPass, func: FuncId) {
        self.ensure(func.index() + 1);
        self.watermarks[pass as usize][func.index()] = self.revision[func.index()];
    }

    /// The functions `pass` must (re)examine this round, each marked seen.
    pub fn dirty_funcs(&mut self, pass: GatedPass, len: usize) -> Vec<FuncId> {
        (0..len)
            .map(FuncId::new)
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
    pub fn mark_changed(&mut self, func: FuncId) {
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

    /// Mark every function in `0..len` seen by `pass`: a whole-program pass that
    /// ran to its own fixed point leaves nothing pending behind it.
    pub fn catch_up(&mut self, pass: GatedPass, len: usize) {
        for i in 0..len {
            self.seen(pass, FuncId::new(i));
        }
    }

    /// Whether any function in `0..len` is still dirty for `pass`. Lets a
    /// caller skip whole-program work it would only need inside the loop.
    pub fn any_pending(&mut self, pass: GatedPass, len: usize) -> bool {
        (0..len).any(|i| self.needs(pass, FuncId::new(i)))
    }

    /// The functions `pass` must (re)process, in store order. Unlike
    /// [`Self::dirty_funcs`], marks none of them seen.
    pub fn pending(&mut self, pass: GatedPass, len: usize) -> Vec<FuncId> {
        (0..len)
            .map(FuncId::new)
            .filter(|&fid| self.needs(pass, fid))
            .collect()
    }

    /// Drive a gate-aware per-function pass over one sweep, on the gate's
    /// threads: visit each function pending for `pass` when the sweep starts,
    /// mark each seen, and mark the changed ones and their neighbours dirty once
    /// the sweep ends. A visit holds its own function mutably and reads
    /// everything else as the sweep found it, so neither the order the visits
    /// run in nor what one rewrites changes what another sees (WEP: Parallel
    /// Optimizer). Returns whether any function changed. `len` is the current
    /// function count (read once; these passes do not add functions mid-pass).
    /// Each visit gets its thread's engine scratch buffers.
    pub fn run_gated_par(
        &mut self,
        pass: GatedPass,
        len: usize,
        visit: impl Fn(&mut EngineBuffers, FuncId) -> bool + Sync,
    ) -> bool {
        let pending = self.pending(pass, len);
        self.sweep_pending_par(pass, &pending, visit)
    }

    /// [`Self::run_gated_par`] over `pending`, what [`Self::pending`] answered
    /// with nothing marked changed since: for a pass that plans its sweep from
    /// the functions it will visit.
    pub fn sweep_pending_par(
        &mut self,
        pass: GatedPass,
        pending: &[FuncId],
        visit: impl Fn(&mut EngineBuffers, FuncId) -> bool + Sync,
    ) -> bool {
        let changed = self
            .exec
            .map_init(pending, EngineBuffers::default, |buffers, &fid| {
                visit(buffers, fid)
            });
        self.close_sweep(pass, pending, &changed)
    }

    /// [`Self::run_gated_par`] for a visit that hands back what it changed: a
    /// `Some` marks the function changed. Returns the `Some`s in store order.
    pub fn sweep_par<R: Send>(
        &mut self,
        pass: GatedPass,
        len: usize,
        visit: impl Fn(FuncId) -> Option<R> + Sync,
    ) -> Vec<(FuncId, R)> {
        let pending = self.pending(pass, len);
        let outcomes = self.exec.map(&pending, |&fid| visit(fid));
        let changed: Vec<bool> = outcomes.iter().map(Option::is_some).collect();
        self.close_sweep(pass, &pending, &changed);
        pending
            .into_iter()
            .zip(outcomes)
            .filter_map(|(fid, outcome)| Some((fid, outcome?)))
            .collect()
    }

    /// Mark the sweep's functions seen, then the changed ones and their
    /// neighbours dirty. Returns whether any changed.
    fn close_sweep(&mut self, pass: GatedPass, pending: &[FuncId], changed: &[bool]) -> bool {
        for &fid in pending {
            self.seen(pass, fid);
        }
        let mut any = false;
        for (&fid, &changed) in pending.iter().zip(changed) {
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

    /// `GatedPass::COUNT` is hand-maintained and sizes the watermark table
    /// (`[Vec<u64>; COUNT]` indexed by `pass as usize`), so a variant added
    /// without bumping COUNT would panic at runtime. The exhaustive `match`
    /// forces this test to be updated when a variant is added (compile error),
    /// and the assert then catches a stale COUNT.
    #[test]
    fn gated_pass_count_matches_variants() {
        let all = [
            GatedPass::PeepholePre,
            GatedPass::PeepholePost,
            GatedPass::CopyProp,
            GatedPass::ConstFold,
            GatedPass::Sroa,
            GatedPass::Licm,
            GatedPass::TmplHoist,
            GatedPass::ContainerSroa,
            GatedPass::Inline,
            GatedPass::Dae,
            GatedPass::Drve,
            GatedPass::SroaParam,
            GatedPass::SroaVariantReturn,
            GatedPass::ValueCopyDemote,
            GatedPass::ScalarForward,
            GatedPass::LetBlockFlatten,
            GatedPass::ParamSpec,
            GatedPass::StoreLoadForward,
            GatedPass::BranchPrune,
            GatedPass::CondImplPostPromote,
        ];
        for p in all {
            match p {
                GatedPass::PeepholePre
                | GatedPass::PeepholePost
                | GatedPass::CopyProp
                | GatedPass::ConstFold
                | GatedPass::Sroa
                | GatedPass::Licm
                | GatedPass::TmplHoist
                | GatedPass::ContainerSroa
                | GatedPass::Inline
                | GatedPass::Dae
                | GatedPass::Drve
                | GatedPass::SroaParam
                | GatedPass::SroaVariantReturn
                | GatedPass::ValueCopyDemote
                | GatedPass::ScalarForward
                | GatedPass::LetBlockFlatten
                | GatedPass::ParamSpec
                | GatedPass::StoreLoadForward
                | GatedPass::BranchPrune
                | GatedPass::CondImplPostPromote => {}
            }
        }
        assert_eq!(all.len(), GatedPass::COUNT);
    }

    /// Build a gate with `n` functions and an explicit call graph, bypassing
    /// `NirPackage` so the propagation algebra can be tested in isolation.
    fn gate_with_graph(n: usize, edges: &[(usize, usize)]) -> FunctionGate {
        let mut callees: Vec<Vec<FuncId>> = vec![Vec::new(); n];
        let mut callers: Vec<Vec<FuncId>> = vec![Vec::new(); n];
        for &(caller, callee) in edges {
            callees[caller].push(FuncId::new(callee));
            callers[callee].push(FuncId::new(caller));
        }
        FunctionGate::with_graph(n, CallGraph { callees, callers }, Arc::default())
    }

    #[test]
    fn fresh_gate_needs_every_function() {
        let mut gate = gate_with_graph(3, &[]);
        for i in 0..3 {
            assert!(gate.needs(GatedPass::PeepholePre, FuncId::new(i)));
        }
    }

    #[test]
    fn seen_clears_need_until_next_change() {
        let mut gate = gate_with_graph(2, &[]);
        let f = FuncId::new(0);
        gate.seen(GatedPass::PeepholePre, f);
        assert!(!gate.needs(GatedPass::PeepholePre, f));
        // Another pass is unaffected by Peephole catching up.
        assert!(gate.needs(GatedPass::CopyProp, f));
        gate.mark_changed(f);
        assert!(gate.needs(GatedPass::PeepholePre, f));
    }

    #[test]
    fn mark_changed_propagates_one_hop_both_directions() {
        // 0 -> 1 -> 2 (0 calls 1, 1 calls 2).
        let mut gate = gate_with_graph(3, &[(0, 1), (1, 2)]);
        for p in [GatedPass::PeepholePre, GatedPass::CopyProp] {
            for i in 0..3 {
                gate.seen(p, FuncId::new(i));
            }
        }
        // Changing the middle function dirties its caller (0) and callee (2).
        gate.mark_changed(FuncId::new(1));
        assert!(gate.needs(GatedPass::PeepholePre, FuncId::new(0)));
        assert!(gate.needs(GatedPass::PeepholePre, FuncId::new(1)));
        assert!(gate.needs(GatedPass::PeepholePre, FuncId::new(2)));
    }

    #[test]
    fn peephole_pre_and_post_are_independent_columns() {
        // The pre- and post-inline peephole runs apply different rule sets, so
        // each owns its own watermark column. After the pre-inline run catches
        // up on a function, the post-inline run must still process it — with a
        // shared column it would have been wrongly skipped, never applying the
        // post-inline-only rules (ref_elim / elide_box_local / labeled_block_fusion).
        let mut gate = gate_with_graph(1, &[]);
        let f = FuncId::new(0);
        gate.seen(GatedPass::PeepholePre, f);
        assert!(!gate.needs(GatedPass::PeepholePre, f));
        assert!(gate.needs(GatedPass::PeepholePost, f));
    }

    #[test]
    fn catch_up_drains_the_column_until_a_change() {
        let mut gate = gate_with_graph(2, &[]);
        gate.catch_up(GatedPass::ParamSpec, 2);
        assert!(!gate.any_pending(GatedPass::ParamSpec, 2));
        assert!(gate.any_pending(GatedPass::CopyProp, 2));
        gate.mark_changed(FuncId::new(1));
        assert!(gate.any_pending(GatedPass::ParamSpec, 2));
    }

    #[test]
    fn mark_changed_leaves_non_neighbours_clean() {
        // 0 -> 1; function 2 is unrelated.
        let mut gate = gate_with_graph(3, &[(0, 1)]);
        for i in 0..3 {
            gate.seen(GatedPass::PeepholePre, FuncId::new(i));
        }
        gate.mark_changed(FuncId::new(0));
        assert!(!gate.needs(GatedPass::PeepholePre, FuncId::new(2)));
    }

    #[test]
    fn run_gated_processes_only_dirty_and_reports_changes() {
        // 0 -> 1; functions 0,1,2. Mark all seen for Peephole, then run a
        // CopyProp pass that changes function 2; only dirty functions are
        // visited, and the change propagates to function 2's (none) neighbours.
        let mut gate = gate_with_graph(3, &[(0, 1)]);
        for i in 0..3 {
            gate.seen(GatedPass::CopyProp, FuncId::new(i));
        }
        // Nothing dirty for CopyProp now: the sweep visits nothing.
        let visited = |gate: &mut FunctionGate| -> Vec<FuncId> {
            gate.sweep_par(GatedPass::CopyProp, 3, Some)
                .into_iter()
                .map(|(fid, _)| fid)
                .collect()
        };
        assert!(visited(&mut gate).is_empty());
        // Dirty function 1 (and its caller 0 via propagation), then run again.
        gate.mark_changed(FuncId::new(1));
        assert_eq!(visited(&mut gate), vec![FuncId::new(0), FuncId::new(1)]);
    }
}
