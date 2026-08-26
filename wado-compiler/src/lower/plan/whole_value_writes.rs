//! Which reference-parameter positions a function replaces outright (`*p = v`),
//! as a least fixpoint over the call graph. A caller can only lose a write its
//! callee actually makes.

use super::value_copy::callgraph::CallGraph;
use super::value_copy::funcset::FuncKeyMap;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::tir::{
    ResolvedType, TirCapture, TirExpr, TirExprKind, TirFunction, TirStmt, TirStmtKind, TirUnaryOp,
    TypeTable,
};
use crate::tir_visitor::TirRefVisitor;

/// Per-function parameter positions replaced by a whole-value write.
pub type WholeValueWrites = FuncKeyMap<IndexSet<u32>>;

/// A function with no body cannot spell `*p = v`, so it starts — and stays —
/// empty. An indirect callee says nothing about itself, so every position it is
/// handed counts as replaced.
pub fn compute(
    project: &FlatPackage,
    call_graph: &CallGraph,
    types: &TypeTable,
) -> WholeValueWrites {
    let mut computed: WholeValueWrites = FuncKeyMap::default();
    for func in &project.functions {
        let func = func.borrow();
        computed.insert(
            func.module_source.clone(),
            func.name.clone(),
            IndexSet::default(),
        );
    }

    call_graph.solve(project, |id| {
        let func = project.functions[id as usize].borrow();
        let Some(body) = &func.body else {
            return false;
        };
        let found = writes_in(&func, body, &computed, types);
        let current = computed
            .get(&func.module_source, &func.name)
            .cloned()
            .unwrap_or_default();
        if found.iter().all(|pos| current.contains(pos)) {
            return false;
        }
        let mut merged = current;
        for pos in found {
            merged.insert(pos);
        }
        computed.insert(func.module_source.clone(), func.name.clone(), merged);
        true
    });

    computed
}

fn writes_in(
    func: &TirFunction,
    body: &crate::tir::TirBlock,
    computed: &WholeValueWrites,
    types: &TypeTable,
) -> IndexSet<u32> {
    let positions: IndexMap<u32, u32> = func
        .params
        .iter()
        .enumerate()
        .map(|(i, p)| (p.local_index, u32::try_from(i).unwrap()))
        .collect();
    replaced_locals(body, computed, types)
        .iter()
        .filter_map(|local| positions.get(local).copied())
        .collect()
}

/// The local slots a whole-value write reaches inside one body, so a borrow
/// bound to one is refused only when something actually replaces through it.
pub fn replaced_locals(
    body: &crate::tir::TirBlock,
    computed: &WholeValueWrites,
    types: &TypeTable,
) -> IndexSet<u32> {
    let mut walker = WriteWalker::new(computed, types);
    walker.visit_block(body);
    walker.found
}

/// The same, for a closure body, which is an expression rather than a block.
pub fn replaced_locals_in_expr(
    body: &TirExpr,
    computed: &WholeValueWrites,
    types: &TypeTable,
) -> IndexSet<u32> {
    let mut walker = WriteWalker::new(computed, types);
    walker.visit_expr(body);
    walker.found
}

/// The storage a write reaches, in the namespace of the body being walked.
#[derive(Clone, Copy)]
enum Root {
    Local(u32),
    /// A capture index, which only the enclosing closure can name a slot for.
    Capture(u32),
}

struct WriteWalker<'a> {
    computed: &'a WholeValueWrites,
    types: &'a TypeTable,
    /// Local slots reached by a whole-value write.
    found: IndexSet<u32>,
    /// Capture indices reached, for the enclosing walk to map back to its own
    /// slots.
    found_captures: IndexSet<u32>,
    /// `let q = p` on a `&mut` hands the same box on rather than copying it, so
    /// `q` is the storage `p` names. Resolved at insertion, so the map is flat.
    aliases: IndexMap<u32, Root>,
}

impl<'a> WriteWalker<'a> {
    fn new(computed: &'a WholeValueWrites, types: &'a TypeTable) -> Self {
        Self {
            computed,
            types,
            found: IndexSet::default(),
            found_captures: IndexSet::default(),
            aliases: IndexMap::default(),
        }
    }

    /// The storage `expr` names, if it names one directly, seeing through the
    /// `&mut *x` reborrow that forwarding a reference argument spells and the
    /// `&mut` bindings that carry one on. One reached any other way is already
    /// covered: whatever derived it is itself a write this walk sees, or an
    /// escape `stores` answers for.
    fn root_of(&self, expr: &TirExpr) -> Option<Root> {
        match &expr.kind {
            TirExprKind::Local { index, .. } => Some(
                self.aliases
                    .get(index)
                    .copied()
                    .unwrap_or(Root::Local(*index)),
            ),
            TirExprKind::Capture { index, .. } => Some(Root::Capture(*index)),
            TirExprKind::Unary {
                op: TirUnaryOp::MutRef,
                expr: inner,
            } => match &inner.kind {
                TirExprKind::Unary {
                    op: TirUnaryOp::Deref,
                    expr: root,
                } => self.root_of(root),
                _ => None,
            },
            _ => None,
        }
    }

    fn record(&mut self, expr: &TirExpr) {
        match self.root_of(expr) {
            Some(Root::Local(index)) => {
                self.found.insert(index);
            }
            Some(Root::Capture(index)) => {
                self.found_captures.insert(index);
            }
            None => {}
        }
    }

    /// A closure body numbers its locals in its own namespace, so walking it
    /// against this one would both invent slots it never named and miss the
    /// ones it did: only what it reaches through a capture is storage the
    /// enclosing body owns.
    fn walk_closure(&mut self, body: &TirExpr, captures: &[TirCapture]) {
        let mut inner = WriteWalker::new(self.computed, self.types);
        inner.visit_expr(body);
        for index in inner.found_captures {
            let Some(capture) = captures.get(index as usize) else {
                continue;
            };
            self.found.insert(capture.outer_index);
        }
    }
}

impl TirRefVisitor for WriteWalker<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        // A `&mut` binding is not a copy: `let q = p` leaves `q` naming `p`'s
        // box, so `*q = v` replaces what `p` names. Without this the callee
        // reports replacing nothing and the caller neither refuses nor writes
        // back.
        if let TirStmtKind::Let {
            value,
            local_index,
            type_id,
            ..
        } = &stmt.kind
            && matches!(self.types.get(*type_id), ResolvedType::MutRef(_))
            && let Some(root) = self.root_of(value)
        {
            self.aliases.insert(*local_index, root);
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            // `*p = v` replaces the referent outright.
            TirExprKind::Assign { target, .. } => {
                if let TirExprKind::Unary {
                    op: TirUnaryOp::Deref,
                    expr: operand,
                } = &target.kind
                {
                    self.record(operand);
                }
            }
            // Forwarding a parameter to a position the callee replaces.
            TirExprKind::Call { func, args, .. } => {
                // `computed` outlives `self`, so the summary is read by borrow:
                // this runs on every call node of every fixpoint round.
                let computed = self.computed;
                if let Some(callee) = computed.get(&func.module_source, &func.name) {
                    for (i, arg) in args.iter().enumerate() {
                        if callee.contains(&u32::try_from(i).unwrap()) {
                            self.record(&arg.expr);
                        }
                    }
                }
            }
            // A functor declares nothing about what it replaces.
            TirExprKind::IndirectCall { args, .. } => {
                for arg in args {
                    self.record(arg);
                }
            }
            TirExprKind::Closure { body, captures, .. } => {
                self.walk_closure(body, captures);
                return;
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}
