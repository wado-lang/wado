//! Shared call graph for the value-copy interprocedural analyses.
//!
//! `confine`, `compute_return_conventions`, and `compute_receiver_alias` are all
//! monotone backward summaries: a function's result depends only on its callees'
//! results. Re-scanning every function each round is O(functions · rounds); a
//! worklist keyed by the reverse edges (a callee's change re-enqueues only its
//! callers) reaches the same least fixpoint touching each function only when a
//! callee it reads actually moves.

use super::funcset::FuncIndex;
use crate::flat_package::FlatPackage;
use crate::tir::{FunctionRef, TirExpr, TirExprKind};
use crate::tir_visitor::TirRefVisitor;
use std::collections::VecDeque;

/// Dense function ids (position in `project.functions`) plus reverse call edges.
pub struct CallGraph {
    index: FuncIndex,
    /// `callers[callee]` lists every function that calls `callee`.
    callers: Vec<Vec<u32>>,
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
            for callee in collector.callees {
                callers[callee as usize].push(caller as u32);
            }
        }
        Self { index, callers }
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
