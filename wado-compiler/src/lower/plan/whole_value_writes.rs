//! Which reference-parameter positions a function replaces outright (`*p = v`),
//! as a least fixpoint over the call graph. A caller can only lose a write its
//! callee actually makes.

use super::value_copy::callgraph::CallGraph;
use super::value_copy::funcset::FuncKeyMap;
use super::value_copy::is_reference_type;
use super::value_copy::stores::RefCarrying;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::tir::{
    ResolvedType, TirBlock, TirCapture, TirExpr, TirExprKind, TirFunction, TirStmt, TirStmtKind,
    TirUnaryOp,
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
    carrying: &RefCarrying,
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
        let found = writes_in(&func, body, &computed, carrying);
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
    body: &TirBlock,
    computed: &WholeValueWrites,
    carrying: &RefCarrying,
) -> IndexSet<u32> {
    let positions: IndexMap<u32, u32> = func
        .params
        .iter()
        .enumerate()
        .map(|(i, p)| (p.local_index, u32::try_from(i).unwrap()))
        .collect();
    replaced_locals(Body::Block(body), computed, carrying)
        .iter()
        .filter_map(|local| positions.get(local).copied())
        .collect()
}

/// The local slots a whole-value write reaches inside one body, so a borrow
/// bound to one is refused only when something actually replaces through it.
/// A function's body is a block; a closure's is an expression.
pub fn replaced_locals(
    body: Body<'_>,
    computed: &WholeValueWrites,
    carrying: &RefCarrying,
) -> IndexSet<u32> {
    let mut walker = WriteWalker::new(computed, carrying);
    // One walk follows a reference only as far forward as it is handed on; a
    // loop handing one backwards needs another. Repeat until nothing grows.
    loop {
        walker.grew = false;
        body.walk(&mut walker);
        if !walker.grew {
            break;
        }
    }
    walker.found
}

/// What a body is spelled as, which differs between a function and a closure.
#[derive(Clone, Copy)]
pub enum Body<'a> {
    Block(&'a TirBlock),
    Expr(&'a TirExpr),
}

impl Body<'_> {
    fn walk(self, walker: &mut WriteWalker) {
        match self {
            Body::Block(block) => walker.visit_block(block),
            Body::Expr(expr) => walker.visit_expr(expr),
        }
    }
}

/// The storage a write reaches, in the namespace of the body being walked.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Root {
    Local(u32),
    /// A capture index, which only the enclosing closure can name a slot for.
    Capture(u32),
}

/// The local an assignment target is rooted at, past the projections leading to
/// the slot it writes.
fn target_local(target: &TirExpr) -> Option<u32> {
    match &target.kind {
        TirExprKind::Local { index, .. } => Some(*index),
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::Index { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. } => target_local(inner),
        _ => None,
    }
}

fn once(root: Root) -> IndexSet<Root> {
    let mut set = IndexSet::default();
    set.insert(root);
    set
}

struct WriteWalker<'a> {
    computed: &'a WholeValueWrites,
    carrying: &'a RefCarrying<'a>,
    /// Local slots reached by a whole-value write.
    found: IndexSet<u32>,
    /// Capture indices reached, for the enclosing walk to map back to its own
    /// slots.
    found_captures: IndexSet<u32>,
    /// `let q = p` on a `&mut` hands the same box on rather than copying it, so
    /// `q` is the storage `p` names. Resolved at insertion, so the map is flat.
    aliases: IndexMap<u32, Root>,
    /// The storage a local may hand a reference back out of. `aliases` is the
    /// must-alias chain a `&mut` binding forms; this is the may-set, so a
    /// reference taken out of an aggregate reaches everything put into it.
    holds: IndexMap<u32, IndexSet<Root>>,
    grew: bool,
}

impl<'a> WriteWalker<'a> {
    fn new(computed: &'a WholeValueWrites, carrying: &'a RefCarrying<'a>) -> Self {
        Self {
            computed,
            carrying,
            found: IndexSet::default(),
            found_captures: IndexSet::default(),
            aliases: IndexMap::default(),
            holds: IndexMap::default(),
            grew: false,
        }
    }

    /// The storage `expr` names, if it names one directly, seeing through the
    /// `&mut *x` reborrow that forwarding a reference argument spells and the
    /// `&mut` bindings that carry one on. A reference reached out of something
    /// that holds it is [`WriteWalker::carried`]'s question instead.
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

    /// Every storage a reference obtained from `expr` may name. A projection
    /// hands on what it came out of only where it yields a reference: reading
    /// data out of one copies the data, not the reference that found it.
    fn carried(&self, expr: &TirExpr) -> IndexSet<Root> {
        match &expr.kind {
            TirExprKind::Local { index, .. } => {
                let mut out = IndexSet::default();
                if let Some(&root) = self.aliases.get(index) {
                    out.insert(root);
                }
                if let Some(held) = self.holds.get(index) {
                    out.extend(held.iter().copied());
                }
                if out.is_empty() {
                    out.insert(Root::Local(*index));
                }
                out
            }
            TirExprKind::Capture { index, .. } => once(Root::Capture(*index)),
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr: place,
            } => match &place.kind {
                // `&mut *x` reborrows what `x` names rather than naming `x`.
                TirExprKind::Unary {
                    op: TirUnaryOp::Deref,
                    expr: inner,
                } => self.carried(inner),
                _ => self.carried_place(place),
            },
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: inner,
            }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::Index { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. } => {
                if is_reference_type(expr.type_id, self.carrying.type_table()) {
                    self.carried(inner)
                } else {
                    IndexSet::default()
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                fields.iter().flat_map(|f| self.carried(&f.value)).collect()
            }
            TirExprKind::TupleLiteral { elements } | TirExprKind::ArrayLiteral { elements } => {
                elements.iter().flat_map(|e| self.carried(e)).collect()
            }
            TirExprKind::VariantConstruct {
                payload: Some(p), ..
            } => self.carried(p),
            // What a call routes to its result is read off a body, which this
            // walk does not have. A result whose type cannot hold a reference
            // hands on nothing whatever the body does; one that can is read as
            // handing on every argument, which only widens the carrier set.
            TirExprKind::Call { args, .. } => {
                if self.carrying.holds(expr.type_id) {
                    args.iter().flat_map(|a| self.carried(&a.expr)).collect()
                } else {
                    IndexSet::default()
                }
            }
            TirExprKind::IndirectCall { args, .. } => {
                if self.carrying.holds(expr.type_id) {
                    args.iter().flat_map(|a| self.carried(a)).collect()
                } else {
                    IndexSet::default()
                }
            }
            _ => IndexSet::default(),
        }
    }

    /// What `&place` hands on. A reference to a place is a route to every
    /// reference the place holds, and to what the place names where it is itself
    /// one. A place that can hold none is storage of its own.
    fn carried_place(&self, place: &TirExpr) -> IndexSet<Root> {
        if self.carrying.holds(place.type_id) {
            return self.carried(place);
        }
        IndexSet::default()
    }

    fn record(&mut self, expr: &TirExpr) {
        for root in self.carried(expr) {
            match root {
                Root::Local(index) => {
                    self.grew |= self.found.insert(index);
                }
                Root::Capture(index) => {
                    self.grew |= self.found_captures.insert(index);
                }
            }
        }
    }

    /// A value put into `local` is reachable through any reference taken back
    /// out of it, whatever aggregate holds it in between.
    fn hold(&mut self, local: u32, value: &TirExpr) {
        let roots = self.carried(value);
        if roots.is_empty() {
            return;
        }
        let held = self.holds.entry(local).or_default();
        for root in roots {
            self.grew |= held.insert(root);
        }
    }

    /// A closure body numbers its locals in its own namespace, so walking it
    /// against this one would both invent slots it never named and miss the
    /// ones it did: only what it reaches through a capture is storage the
    /// enclosing body owns.
    fn walk_closure(&mut self, body: &TirExpr, captures: &[TirCapture]) {
        let mut inner = WriteWalker::new(self.computed, self.carrying);
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
        {
            let alias = matches!(
                self.carrying.type_table().get(*type_id),
                ResolvedType::MutRef(_)
            )
            .then(|| self.root_of(value))
            .flatten();
            match alias {
                Some(root) => {
                    self.aliases.insert(*local_index, root);
                }
                None => self.hold(*local_index, value),
            }
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            // `*p = v` replaces the referent outright. Any other write puts the
            // value where the target's local can hand it back out.
            TirExprKind::Assign { target, value } => {
                if let TirExprKind::Unary {
                    op: TirUnaryOp::Deref,
                    expr: operand,
                } = &target.kind
                {
                    self.record(operand);
                } else if let Some(local) = target_local(target) {
                    self.hold(local, value);
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
