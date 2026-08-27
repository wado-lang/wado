//! Shared call graph for the value-copy interprocedural analyses.
//!
//! Every summary `plan` computes is a monotone backward one: a function's
//! result depends only on its callees'. So they share one graph, built after
//! synthesis, and the passes running before closure lifting adds functions
//! share a second. Re-scanning every function each round is O(functions ·
//! rounds); a
//! worklist keyed by the reverse edges (a callee's change re-enqueues only its
//! callers) reaches the same least fixpoint touching each function only when a
//! callee it reads actually moves.

use super::funcset::FuncIndex;
use crate::flat_package::FlatPackage;
use crate::tir::{FunctionRef, TirExpr, TirExprKind};
use crate::tir_visitor::TirRefVisitor;
use std::collections::VecDeque;

/// Dense function ids (position in `project.functions`) plus call edges both
/// ways: the reverse edges drive the worklist, the forward ones the components.
pub struct CallGraph {
    index: FuncIndex,
    /// `callers[callee]` lists every function that calls `callee`.
    callers: Vec<Vec<u32>>,
    /// `callees[caller]` lists every function it calls, deduplicated.
    callees: Vec<Vec<u32>>,
}

impl CallGraph {
    pub fn build(project: &FlatPackage) -> Self {
        let n = project.functions.len();
        let mut index = FuncIndex::default();
        for (i, func) in project.functions.iter().enumerate() {
            let func = func.borrow();
            index.insert(func.module_source.clone(), func.name.clone(), i as u32);
        }
        let mut callers: Vec<Vec<u32>> = vec![Vec::new(); n];
        let mut callees: Vec<Vec<u32>> = vec![Vec::new(); n];
        for (caller, func) in project.functions.iter().enumerate() {
            let func = func.borrow();
            let Some(body) = &func.body else { continue };
            let mut collector = CalleeCollector {
                index: &index,
                callees: Vec::new(),
            };
            collector.visit_block(body);
            collector.callees.sort_unstable();
            collector.callees.dedup();
            for &callee in &collector.callees {
                callers[callee as usize].push(caller as u32);
            }
            callees[caller] = collector.callees;
        }
        Self {
            index,
            callers,
            callees,
        }
    }

    /// Every function that calls `id`.
    pub fn callers_of(&self, id: u32) -> &[u32] {
        &self.callers[id as usize]
    }

    /// The strongly connected components of the call graph, each callee
    /// component before the components that call it. An analysis whose answer
    /// for one member depends on another's needs the cycle as a unit; Tarjan
    /// emits exactly that, and in the order that leaves everything outside a
    /// component already settled when it is reached.
    pub fn sccs(&self) -> Vec<Vec<u32>> {
        let n = self.callees.len();
        const UNVISITED: u32 = u32::MAX;
        let mut index = vec![UNVISITED; n];
        let mut low = vec![0u32; n];
        let mut on_stack = vec![false; n];
        let mut stack: Vec<u32> = Vec::new();
        let mut next_index = 0u32;
        let mut out: Vec<Vec<u32>> = Vec::new();
        // (node, next child to visit) — an explicit stack, since a call graph
        // is deep enough to overflow a recursive walk.
        let mut work: Vec<(u32, usize)> = Vec::new();
        for root in 0..n as u32 {
            if index[root as usize] != UNVISITED {
                continue;
            }
            work.push((root, 0));
            while let Some((v, child)) = work.pop() {
                if child == 0 {
                    index[v as usize] = next_index;
                    low[v as usize] = next_index;
                    next_index += 1;
                    stack.push(v);
                    on_stack[v as usize] = true;
                }
                if let Some(&w) = self.callees[v as usize].get(child) {
                    work.push((v, child + 1));
                    if index[w as usize] == UNVISITED {
                        work.push((w, 0));
                    } else if on_stack[w as usize] {
                        low[v as usize] = low[v as usize].min(index[w as usize]);
                    }
                    continue;
                }
                if low[v as usize] == index[v as usize] {
                    let mut component = Vec::new();
                    loop {
                        let w = stack.pop().expect("Tarjan stack holds the component");
                        on_stack[w as usize] = false;
                        component.push(w);
                        if w == v {
                            break;
                        }
                    }
                    out.push(component);
                }
                if let Some(&(parent, _)) = work.last() {
                    low[parent as usize] = low[parent as usize].min(low[v as usize]);
                }
            }
        }
        out
    }

    /// Dense id of `func`, or `None` for a callee outside this package.
    pub fn id_of(&self, func: &FunctionRef) -> Option<u32> {
        self.index.id(&func.module_source, &func.name)
    }

    /// Drive a monotone worklist: seed every body function, and each time
    /// `recompute(id)` reports a change, re-enqueue that function's callers.
    /// `recompute` returns whether the function's summary grew.
    pub fn solve(&self, project: &FlatPackage, mut recompute: impl FnMut(u32) -> bool) {
        let n = self.callers.len();
        let mut in_queue = vec![false; n];
        let mut queue: VecDeque<u32> = VecDeque::with_capacity(n);
        for (i, func) in project.functions.iter().enumerate() {
            if func.borrow().body.is_some() {
                in_queue[i] = true;
                queue.push_back(i as u32);
            }
        }
        while let Some(u) = queue.pop_front() {
            in_queue[u as usize] = false;
            if recompute(u) {
                for &caller in &self.callers[u as usize] {
                    if !in_queue[caller as usize] {
                        in_queue[caller as usize] = true;
                        queue.push_back(caller);
                    }
                }
            }
        }
    }
}

struct CalleeCollector<'a> {
    index: &'a FuncIndex,
    callees: Vec<u32>,
}

impl TirRefVisitor for CalleeCollector<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if let TirExprKind::Call { func, .. } = &expr.kind
            && let Some(id) = self.index.id(&func.module_source, &func.name)
        {
            self.callees.push(id);
        }
        self.walk_expr(expr);
    }
}
