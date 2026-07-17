//! Post-inline read-only clone forwarding.
//!
//! Inlining an accessor that copies a projection of its receiver
//! (`build(&self) -> List { return *self }`, finalizing a `[1, 2, 3]` builder)
//! leaves a residual `let t = array_clone(&place)` whose result is only ever
//! read — most visibly `array_clone(&array_clone(&place))`, the redundant
//! double-copy of a constant into a read-only binding that later feeds another
//! copy. The fold-time value-copy elision cannot reach it: the copy is created
//! by `inline` + `value_copy_demote`, after the pre-inline elider has run.
//!
//! This pass recovers it. When `t` is a read-only binding of `array_clone(&place)`
//! (or its shallow twin) whose sole use is the referent of *another*
//! `array_clone(&t)`, the inner clone is redundant: the outer clone already
//! re-materializes a fresh copy, so `t` only needs to hold `place`'s value at that
//! one point. The pass rewrites the outer clone to read `place` directly and drops
//! the dead binding, turning `array_clone(&array_clone(&place))` into a single
//! `array_clone(&place)`.
//!
//! Soundness rests on four gates, all conservative:
//! - `t` is read exactly once and never mutated, reassigned, or `&mut`-borrowed;
//! - that single read is the argument of an `array_clone` call — so the outer
//!   clone makes a fresh, independent copy at the use and `t` is dead afterward.
//!   This is the crux: a use that instead *stored* `t` into a longer-lived
//!   aggregate (`*r = List { repr: t }`) would keep it aliased past the use, where
//!   a later mutation of `place` could corrupt it, so such uses are not matched;
//! - the use is in the *same block*, after the binding, a straight-line range;
//! - no statement in that interval mutates `place`'s root local or writes *any*
//!   global, so `place`'s value at the use equals its value at the binding.

use super::alias::{FirstParamTypes, first_param_types, method_mutates_receiver};
use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{FuncId, FunctionRef, NirFunction, NirUnaryOp};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef, StmtId, StmtKind};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::tir::TypeTable;

/// The builtin general-name of a call's callee descriptor, resolving a
/// monomorphized instance (`array_clone::<i32>`) to its generic builtin name.
fn builtin_gname(func: &FunctionRef) -> Option<String> {
    func.builtin_name()
        .or_else(|| func.monomorphized_builtin_name())
}

/// Whether the call's stamped `func_id` is a value-copying array clone
/// (`array_clone` / its shallow spine twin).
fn is_array_clone(func_id: FuncId, descriptors: &[FunctionRef]) -> bool {
    matches!(
        builtin_gname(super::dce::callee_descriptor(descriptors, func_id)).as_deref(),
        Some("builtin::array_clone" | "builtin::array_clone_shallow")
    )
}

/// Whole-function occurrence / mutation profile of a local, for the read-only
/// single-use gate.
#[derive(Default)]
struct Usage {
    /// Every `Local` mention (reads and assignment targets).
    occurrences: u32,
    reassigned: bool,
    /// Field-mutated (`t.f = …`), `&mut`-borrowed, or passed as a mutable arg.
    mutated: bool,
}

impl Usage {
    /// Read exactly once and never written through any channel: the single
    /// occurrence is a plain read whose aliasing is unobservable.
    fn is_read_only_single_use(&self) -> bool {
        self.occurrences == 1 && !self.reassigned && !self.mutated
    }
}

/// The referent's root: a local (whose mutations the interval check tracks) or a
/// global (any global write in the interval is conservatively disqualifying).
enum PlaceRoot {
    Local(u32),
    Global,
}

/// The root of a re-materializable place — a pure `Local` / global / field-access
/// chain. `None` for anything the pass will not re-read (a deref, an index, a
/// call result), which keeps a copy.
fn place_root(body: &Body, expr: ExprId) -> Option<PlaceRoot> {
    match &body.exprs[expr].kind {
        ExprKind::Local { index, .. } => Some(PlaceRoot::Local(*index)),
        ExprKind::GlobalVarGet { .. } => Some(PlaceRoot::Global),
        ExprKind::FieldAccess { expr: inner, .. } => place_root(body, inner.as_expr()?),
        _ => None,
    }
}

/// If `expr` is `array_clone(&place)` / `array_clone_shallow(&place)`, return the
/// referent `place` expression (the argument with its leading `&` peeled).
fn clone_referent(body: &Body, expr: ExprId, descriptors: &[FunctionRef]) -> Option<ExprId> {
    let ExprKind::Call { func_id, args, .. } = &body.exprs[expr].kind else {
        return None;
    };
    if !is_array_clone(*func_id, descriptors) || args.len() != 1 {
        return None;
    }
    let arg = args[0].expr.as_expr()?;
    match &body.exprs[arg].kind {
        ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr: inner,
        } => inner.as_expr(),
        _ => None,
    }
}

/// Root local a place / assignment target is anchored at, following pure
/// projections. Used to record which local a statement mutates.
fn root_local(body: &Body, expr: ExprId) -> Option<u32> {
    match &body.exprs[expr].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::Unary { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::Index { expr: inner, .. } => inner.as_expr().and_then(|e| root_local(body, e)),
        _ => None,
    }
}

/// Accumulate whole-function usage for every local.
fn collect_usage(body: &Body, usage: &mut IndexMap<u32, Usage>) {
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node {
            match &body.exprs[id].kind {
                ExprKind::Local { index, .. } => {
                    usage.entry(*index).or_default().occurrences += 1;
                }
                ExprKind::Assign { target, .. } => {
                    if let ExprKind::Local { index, .. } = &body.exprs[*target].kind {
                        usage.entry(*index).or_default().reassigned = true;
                    } else if let Some(l) = root_local(body, *target) {
                        usage.entry(l).or_default().mutated = true;
                    }
                }
                ExprKind::Unary {
                    op: NirUnaryOp::MutRef,
                    expr: inner,
                } => {
                    if let Some(l) = inner.as_expr().and_then(|e| root_local(body, e)) {
                        usage.entry(l).or_default().mutated = true;
                    }
                }
                ExprKind::MethodCall { args, .. } | ExprKind::Call { args, .. } => {
                    for a in args {
                        if a.is_mut
                            && let Some(l) = a.expr.as_expr().and_then(|e| root_local(body, e))
                        {
                            usage.entry(l).or_default().mutated = true;
                        }
                    }
                }
                _ => {}
            }
        }
        body.for_each_child(node, |c| stack.push(c));
    }
}

/// Whether a call in the interval can reach the place's root through a channel
/// invisible to a syntactic scan: a global-rooted place (the callee may write
/// any global), or an address-taken local (a `&mut` reference to it may have
/// escaped into the callee, or into a closure the call invokes).
fn call_disturbs_root(root: &PlaceRoot, address_taken: &IndexSet<u32>) -> bool {
    match root {
        PlaceRoot::Global => true,
        PlaceRoot::Local(r) => address_taken.contains(r),
    }
}

/// Whether the subtree rooted at `stmt` mutates `root` (of a local place) or
/// writes any global — anything that can change a re-materialized place's value
/// across the binding→use interval. Beyond direct assignments / `&mut` borrows /
/// `mut` arguments, this covers three otherwise-invisible channels: a mutating
/// method receiver (`xs.push(0)`, whose receiver is a bare `Local` with no
/// `MutRef` node), a write through a `&mut` reference created before the interval
/// (rooted at the reference, not the pointee — folded into the address-taken
/// gate), and a callee-internal global write (any call disqualifies a
/// global-rooted place).
fn stmt_disturbs_place(
    body: &Body,
    stmt: StmtId,
    root: &PlaceRoot,
    address_taken: &IndexSet<u32>,
    fpt: &FirstParamTypes,
    type_table: &TypeTable,
) -> bool {
    let mut disturbs = false;
    let mut stack = vec![NodeRef::Stmt(stmt)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node {
            match &body.exprs[id].kind {
                ExprKind::GlobalVarSet { .. } => return true,
                ExprKind::Assign { target, .. } => {
                    if let PlaceRoot::Local(r) = root {
                        // A direct write to the root, or — once the root is
                        // address-taken — any write, since a `&mut` alias of it
                        // may be the target we cannot attribute.
                        if root_local(body, *target) == Some(*r) || address_taken.contains(r) {
                            disturbs = true;
                        }
                    }
                }
                ExprKind::Unary {
                    op: NirUnaryOp::MutRef,
                    expr: inner,
                } => {
                    if let (PlaceRoot::Local(r), Some(l)) =
                        (root, inner.as_expr().and_then(|e| root_local(body, e)))
                        && l == *r
                    {
                        disturbs = true;
                    }
                }
                ExprKind::MethodCall {
                    receiver,
                    func_id,
                    args,
                    ..
                } => {
                    if call_disturbs_root(root, address_taken) {
                        return true;
                    }
                    if let PlaceRoot::Local(r) = root
                        && let Some(re) = receiver.as_expr()
                        && root_local(body, re) == Some(*r)
                        && method_mutates_receiver(body, re, *func_id, fpt, type_table, true, None)
                    {
                        disturbs = true;
                    }
                    disturbs |= mut_arg_hits_root(body, args, root);
                }
                ExprKind::Call { args, .. } => {
                    if call_disturbs_root(root, address_taken) {
                        return true;
                    }
                    disturbs |= mut_arg_hits_root(body, args, root);
                }
                ExprKind::IndirectCall { .. } | ExprKind::CmRawCall { .. } => {
                    if call_disturbs_root(root, address_taken) {
                        return true;
                    }
                }
                _ => {}
            }
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    disturbs
}

/// Whether a `mut` call argument roots at the local place root.
fn mut_arg_hits_root(
    body: &Body,
    args: &[crate::nir_arena::ArenaCallArg],
    root: &PlaceRoot,
) -> bool {
    args.iter().any(|a| {
        a.is_mut
            && matches!(
                (root, a.expr.as_expr().and_then(|e| root_local(body, e))),
                (PlaceRoot::Local(r), Some(l)) if l == *r
            )
    })
}

/// Locals whose address is taken by a `&mut` borrow anywhere in the function:
/// a write through such a reference can reach the local without naming it.
fn collect_address_taken(body: &Body, out: &mut IndexSet<u32>) {
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node
            && let ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr: inner,
            } = &body.exprs[id].kind
            && let Some(l) = inner.as_expr().and_then(|e| root_local(body, e))
        {
            out.insert(l);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
}

/// The `Local { index }` node that sits as the referent of an
/// `array_clone(&Local { index })` call in `stmt`'s subtree, if any.
///
/// Restricting the forwardable use to *another clone* is what keeps this sound:
/// the outer clone re-materializes a fresh copy at the use site, so the binding's
/// value only has to match `place` at that one point (the interval-stability
/// check), and the binding is dead afterward. A use that instead *stored* the
/// clone into a longer-lived aggregate (`*r = List { repr: t }`) would keep it
/// aliased past the use, where a later mutation of `place` could corrupt it — so
/// such uses are deliberately not matched.
fn find_clone_referent_use(
    body: &Body,
    stmt: StmtId,
    index: u32,
    descriptors: &[FunctionRef],
) -> Option<ExprId> {
    let mut stack = vec![NodeRef::Stmt(stmt)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node
            && let ExprKind::Call { func_id, args, .. } = &body.exprs[id].kind
            && is_array_clone(*func_id, descriptors)
            && args.len() == 1
            && let Some(arg) = args[0].expr.as_expr()
            && let ExprKind::Unary {
                op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
                expr: inner,
            } = &body.exprs[arg].kind
            && let Some(ie) = inner.as_expr()
            && let ExprKind::Local { index: i, .. } = &body.exprs[ie].kind
            && *i == index
        {
            return Some(ie);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    None
}

struct Candidate {
    /// The `let t = array_clone(&place)` statement to drop.
    binding_stmt: StmtId,
    /// The forwarded local `t`.
    target: u32,
    /// The referent expression to re-materialize at the use.
    place: ExprId,
    /// Root local of `place`, if any (`None` for a global-rooted place). Used to
    /// break clone chains: a candidate whose place reads another candidate's
    /// target would dangle once that target's binding is dropped.
    place_root_local: Option<u32>,
    /// The single `Local { t }` read node to replace.
    use_node: ExprId,
}

/// Collect every eligible forwarding site in `block`.
#[allow(clippy::too_many_arguments)]
fn collect_in_block(
    body: &Body,
    block: BlockId,
    descriptors: &[FunctionRef],
    usage: &IndexMap<u32, Usage>,
    address_taken: &IndexSet<u32>,
    fpt: &FirstParamTypes,
    type_table: &TypeTable,
    out: &mut Vec<Candidate>,
) {
    let stmts = &body.blocks[block].stmts;
    for (k, &stmt) in stmts.iter().enumerate() {
        // `is_mut` / `skip_value_copy` are irrelevant here: forwarding safety
        // comes entirely from the read-only single-use target gate and the
        // place-stability interval check below, so a mutable-declared-but-never-
        // mutated or a SROA-planted (`skip_value_copy`) binding is fair game.
        let StmtKind::Let {
            local_index, value, ..
        } = &body.stmts[stmt].kind
        else {
            continue;
        };
        let target = *local_index;
        if !usage
            .get(&target)
            .is_some_and(Usage::is_read_only_single_use)
        {
            continue;
        }
        let Some(place) = value
            .as_expr()
            .and_then(|v| clone_referent(body, v, descriptors))
        else {
            continue;
        };
        let Some(root) = place_root(body, place) else {
            continue;
        };
        // The single use must be the referent of another `array_clone`, in this
        // same block, after the binding.
        let Some((u, use_node)) = stmts.iter().enumerate().skip(k + 1).find_map(|(i, &s)| {
            find_clone_referent_use(body, s, target, descriptors).map(|node| (i, node))
        }) else {
            continue;
        };
        // The place must be unchanged across `(k, u]`.
        if stmts[k + 1..=u]
            .iter()
            .any(|&s| stmt_disturbs_place(body, s, &root, address_taken, fpt, type_table))
        {
            continue;
        }
        let place_root_local = match root {
            PlaceRoot::Local(l) => Some(l),
            PlaceRoot::Global => None,
        };
        out.push(Candidate {
            binding_stmt: stmt,
            target,
            place,
            place_root_local,
            use_node,
        });
    }
}

/// Standalone-session rule: one `apply_block` at the root forwards every
/// eligible read-only clone in the function.
struct CloneForwardRule<'a> {
    descriptors: &'a [FunctionRef],
    fpt: &'a FirstParamTypes,
    type_table: &'a TypeTable,
}

impl Rule for CloneForwardRule<'_> {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        // Run once, whole-function, at the root.
        if engine.parent_of(NodeRef::Block(block)).is_some() {
            return false;
        }
        let mut usage: IndexMap<u32, Usage> = IndexMap::default();
        collect_usage(engine.body, &mut usage);
        let mut address_taken: IndexSet<u32> = IndexSet::default();
        collect_address_taken(engine.body, &mut address_taken);

        let mut candidates = Vec::new();
        let mut stack = vec![engine.body.root];
        while let Some(b) = stack.pop() {
            collect_in_block(
                engine.body,
                b,
                self.descriptors,
                &usage,
                &address_taken,
                self.fpt,
                self.type_table,
                &mut candidates,
            );
            let stmts = engine.body.blocks[b].stmts.clone();
            for s in stmts {
                let mut kids = Vec::new();
                engine.body.for_each_child(NodeRef::Stmt(s), |c| {
                    if let NodeRef::Block(cb) = c {
                        kids.push(cb);
                    } else {
                        collect_child_blocks(engine.body, c, &mut kids);
                    }
                });
                stack.extend(kids);
            }
        }
        // Break clone chains: drop any candidate whose place reads a local that
        // another candidate forwards (and thus removes), which would leave this
        // candidate's re-materialized place dangling. The dropped candidate can
        // be picked up on a later compile if the chain reshapes.
        let targets: IndexSet<u32> = candidates.iter().map(|c| c.target).collect();
        candidates.retain(|c| c.place_root_local.is_none_or(|r| !targets.contains(&r)));
        if candidates.is_empty() {
            return false;
        }

        // Each surviving candidate's target is single-use, so their use nodes are
        // disjoint. Re-materialize each place at its use, then drop the bindings.
        let dead: IndexSet<StmtId> = candidates.iter().map(|c| c.binding_stmt).collect();
        for c in &candidates {
            let cloned = engine.clone_expr(c.place);
            let kind = engine.body.exprs[cloned].kind.clone();
            engine.replace_expr_kind(c.use_node, kind);
        }
        remove_dead_bindings(engine, engine.body.root, &dead);
        true
    }
}

/// Collect the child blocks reachable inside a node (for the block worklist).
fn collect_child_blocks(body: &Body, node: NodeRef, out: &mut Vec<BlockId>) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if let NodeRef::Block(b) = n {
            out.push(b);
            continue;
        }
        body.for_each_child(n, |c| stack.push(c));
    }
}

/// Drop the forwarded bindings from every block, recursively.
fn remove_dead_bindings(engine: &mut Engine, block: BlockId, dead: &IndexSet<StmtId>) {
    let kept: Vec<StmtId> = engine.body.blocks[block]
        .stmts
        .iter()
        .copied()
        .filter(|s| !dead.contains(s))
        .collect();
    engine.set_block_stmts(block, kept);
    let stmts = engine.body.blocks[block].stmts.clone();
    let mut child_blocks = Vec::new();
    for s in stmts {
        engine.body.for_each_child(NodeRef::Stmt(s), |c| {
            collect_child_blocks(engine.body, c, &mut child_blocks);
        });
    }
    for b in child_blocks {
        remove_dead_bindings(engine, b, dead);
    }
}

/// Runs once after the fixed-point loop (a whole-package sweep, no gate): the
/// residual clones it targets only take their flat shape after the post-loop
/// `const_object_globalization`, so there is no earlier iteration to be dirty in.
pub fn forward_redundant_clones(project: &mut NirPackage) -> bool {
    let descriptors = super::dce::build_callee_descriptors(project);
    let fpt = first_param_types(project);
    let type_table = project.type_table.clone();
    let type_table = type_table.borrow();
    let len = project.functions.len();
    let mut buffers = EngineBuffers::default();
    let mut any = false;
    for fi in 0..len {
        let mut func = project.functions[fi].borrow_mut();
        if func.body.is_none() {
            continue;
        }
        let rule = CloneForwardRule {
            descriptors: &descriptors,
            fpt: &fpt,
            type_table: &type_table,
        };
        let NirFunction { body, locals, .. } = &mut *func;
        let body = body.as_mut().expect("checked above");
        let mut engine = Engine::new(body, &mut buffers, locals);
        any |= engine.run(&[&rule]);
    }
    any
}
