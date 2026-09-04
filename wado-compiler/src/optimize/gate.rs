//! Per-function dirty-set gating for the optimizer fixed-point loop, so a pass
//! skips what has not changed since it last ran. Each function carries a
//! monotonic `revision` and each pass a per-function `watermark`;
//! [`FunctionGate::mark_changed`] bumps the revision and, conservatively, its
//! 1-hop callers and callees. Keyed by [`FuncId`], the index in `functions`.
//!
//! Every loop pass is optional, the IR being valid without it, so an imprecise
//! gate costs optimization quality and never correctness. When in doubt, the
//! propagation marks dirty.

use cranelift_entity::EntityRef;

use crate::nir::FuncId;
use crate::nir_arena::ExprKind;
use crate::nir_package::NirPackage;

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
}

impl GatedPass {
    const COUNT: usize = 16;
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
            let mut seen: Vec<FuncId> = Vec::new();
            for node in body.exprs.values() {
                let func_id = match &node.kind {
                    ExprKind::Call { func_id, .. } => func_id,
                    _ => continue,
                };
                let callee = *func_id;
                if !seen.contains(&callee) {
                    seen.push(callee);
                }
            }
            for &callee in &seen {
                callers[callee.index()].push(FuncId::new(i));
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

    /// Whether any function in `0..len` is still dirty for `pass`. Lets a
    /// caller skip whole-program work it would only need inside the loop.
    pub fn any_pending(&mut self, pass: GatedPass, len: usize) -> bool {
        (0..len).any(|i| self.needs(pass, FuncId::new(i)))
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
        mut f: impl FnMut(FuncId) -> bool,
    ) -> bool {
        let mut any = false;
        for i in 0..len {
            let fid = FuncId::new(i);
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
                | GatedPass::LetBlockFlatten => {}
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
        // Nothing dirty for CopyProp now: run_gated visits nothing.
        let mut visited = Vec::new();
        let changed = gate.run_gated(GatedPass::CopyProp, 3, |fid| {
            visited.push(fid);
            false
        });
        assert!(visited.is_empty());
        assert!(!changed);
        // Dirty function 1 (and its caller 0 via propagation), then run again.
        gate.mark_changed(FuncId::new(1));
        let mut visited = Vec::new();
        gate.run_gated(GatedPass::CopyProp, 3, |fid| {
            visited.push(fid);
            false
        });
        assert_eq!(visited, vec![FuncId::new(0), FuncId::new(1)]);
    }
}
