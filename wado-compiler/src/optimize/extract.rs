//! Extraction: materialize pure values from the live `ValueGraph` back into the
//! `SkelTree`.
//!
//! In the live-ValueGraph design (see
//! `docs/wep-2026-06-05-nir-optimizer-architecture.md`) an expression's value is a
//! hash-consed [`ValueId`]; the extractor walks the skeleton and lowers each
//! pure operand to that value's concrete form.
//!
//! This is the literal case: an expression whose value is a constant (`Int` /
//! `Float` / `Bool` / `Char`) is replaced by that literal. The share-vs-duplicate
//! cost model for non-literal multi-use values is a later step; constants are
//! always cheaper to rematerialize than to share, so they need no cost decision.
//!
//! [`extract_const`] is the shared materialization primitive, used in
//! production by `store_load_forward`. [`ExtractLiteralRule`] (the worklist
//! rule form) is a test-only vehicle exercising the same primitive.

use crate::nir_arena::{ExprId, ExprKind, NodeRef, StmtKind};
use crate::nir_engine::Engine;
use crate::nir_value_graph::{ValueId, ValueKind};

/// Rewrite a pure expression whose `ValueGraph` representative is a literal into
/// that literal. Idempotent: an expression already holding the target literal
/// is left untouched, so the worklist retry terminates.
#[cfg(test)]
pub(super) struct ExtractLiteralRule;

#[cfg(test)]
impl crate::nir_engine::Rule for ExtractLiteralRule {
    fn apply_expr(&self, e: &mut Engine, id: ExprId) -> bool {
        // An assign target is a place, not a value — never materialize it.
        if e.is_assign_target(id) {
            return false;
        }
        let Some(vid) = e.value(id) else {
            return false;
        };
        let Some(value) = extract_const(e, vid, id) else {
            return false;
        };
        // Promote the node to the pooled constant in its parent slot. Idempotent:
        // a promoted (orphaned) node has no parent slot, so the retry reports no
        // change and the worklist terminates.
        e.replace_expr_with_value(id, value)
    }
}

/// True if `id` is the value operand of a `let` binding. Freezing it would
/// replace `let x = <arith>` with `let x = Operand::Value`, which the WIR
/// builder lowers without the `LocalSet x (value)` shape the `local.tee`
/// fusion (and other `LocalSet`-keyed WIR peepholes) match on — a code-quality
/// regression. Leave the binding's value in the skeleton; freeze arith in other
/// positions (args, returns, sub-expressions) instead.
fn is_let_value(e: &Engine, id: ExprId) -> bool {
    matches!(
        e.parent_of(NodeRef::Expr(id)),
        Some(NodeRef::Stmt(s))
            if matches!(&e.body.stmts[s].kind,
                StmtKind::Let { value, .. } if value.as_expr() == Some(id))
    )
}

/// True if `id` is a pure-arith node (`Binary` / `Cast` / pure `Unary`) — a
/// freeze candidate. Under early promotion, `FieldAccess` reads also qualify
/// (their value re-emits as a `StructGet`; reemittability + the shared `heap_ver`
/// keep it sound).
fn is_pure_arith(e: &Engine, id: ExprId, include_fields: bool) -> bool {
    matches!(
        &e.body.exprs[id].kind,
        ExprKind::Binary { .. }
            | ExprKind::Cast { .. }
            | ExprKind::Unary {
                op: crate::nir::NirUnaryOp::Neg
                    | crate::nir::NirUnaryOp::Not
                    | crate::nir::NirUnaryOp::BitNot,
                ..
            }
    ) || (include_fields && matches!(&e.body.exprs[id].kind, ExprKind::FieldAccess { .. }))
}

/// The structured-control path from the body root down to `expr`'s enclosing
/// statement: an outermost-first list of `(block, stmt_index)` pairs, where the
/// statement at `stmt_index` in `block` is the one on the path to `expr` (the
/// control construct it descends through, or the enclosing statement itself at
/// the leaf). Blocks have unique ids, so two paths share a prefix exactly when
/// the uses lie under a common chain of enclosing blocks — the basis for the
/// nearest-common-dominator placement in [`materialise_point`]. `None` if `expr`
/// has no enclosing statement (it is not inside the body).
fn block_path(e: &Engine, expr: ExprId) -> Option<Vec<(crate::nir_arena::BlockId, usize)>> {
    let mut path = Vec::new();
    let mut node = NodeRef::Expr(expr);
    // The most recent `Stmt` crossed on the way up; it is the direct child of
    // the next enclosing `Block`. Updated as the walk passes each statement, so
    // an expression-form block (`let x = if c { … }`, where a `Block` is the
    // child of an `Expr`) records the right statement at the outer block.
    let mut last_stmt: Option<crate::nir_arena::StmtId> = None;
    loop {
        match node {
            NodeRef::Stmt(s) => {
                last_stmt = Some(s);
                node = e.parent_of(node)?;
            }
            NodeRef::Block(b) => {
                let s = last_stmt?;
                let pos = e.body.blocks[b].stmts.iter().position(|&x| x == s)?;
                path.push((b, pos));
                match e.parent_of(node) {
                    Some(p) => node = p,
                    None => break,
                }
            }
            NodeRef::Expr(_) | NodeRef::Pat(_) => node = e.parent_of(node)?,
        }
    }
    path.reverse();
    Some(path)
}

/// Whether `b` is the body of a `Loop`, i.e. re-entered per iteration.
fn is_loop_body(e: &Engine, b: crate::nir_arena::BlockId) -> bool {
    matches!(
        e.parent_of(NodeRef::Block(b)),
        Some(NodeRef::Stmt(s)) if matches!(&e.body.stmts[s].kind, StmtKind::Loop { body } if *body == b)
    )
}

/// The statement before which a `let _av = <value>` materialising the uses
/// `ids` can be inserted so it **dominates all of them**: the nearest common
/// dominator block (the deepest enclosing block common to every use) with the
/// insertion taken at the earliest statement any use descends through there.
///
/// Soundness rests on structured control flow: a block runs its statements in
/// order, so a statement at or before every use's leading statement in their
/// common ancestor block precedes (dominates) each use. The uses share one
/// `ValueId`, so no heap bump separates them — the field/value is constant
/// across the span, and a load pinned at this point reproduces it for all.
/// `None` if any use lacks a path.
fn materialise_point(
    e: &Engine,
    ids: &[ExprId],
) -> Option<(crate::nir_arena::StmtId, crate::nir_arena::BlockId)> {
    let paths: Vec<Vec<(crate::nir_arena::BlockId, usize)>> = ids
        .iter()
        .map(|&id| block_path(e, id))
        .collect::<Option<_>>()?;
    // Longest common prefix of enclosing blocks. The root block is shared by
    // all, so the prefix is non-empty.
    let mut depth = 0;
    while let Some((b, _)) = paths[0].get(depth) {
        if paths.iter().all(|p| p.get(depth).map(|x| x.0) == Some(*b)) {
            depth += 1;
        } else {
            break;
        }
    }
    let common = depth.checked_sub(1)?;
    // A use nested inside a loop the chosen point sits outside of would read,
    // on every iteration, a value the `let` computed once. The value may name a
    // local the loop reassigns, so there is no shared point — refuse rather
    // than hoist. Uses all within the same loop keep their point inside it.
    if paths
        .iter()
        .any(|p| p[common + 1..].iter().any(|&(b, _)| is_loop_body(e, b)))
    {
        return None;
    }
    let block = paths[0][common].0;
    let pos = paths.iter().map(|p| p[common].1).min()?;
    let stmt = e.body.blocks[block].stmts[pos];
    Some((stmt, block))
}

/// The structured-control path from the root down to `stmt`'s enclosing chain:
/// an outermost-first list of `(block, stmt_index)`. Like [`block_path`] but
/// rooted at a statement, for the def-dominance check.
fn stmt_block_path(
    e: &Engine,
    stmt: crate::nir_arena::StmtId,
) -> Vec<(crate::nir_arena::BlockId, usize)> {
    let mut path = Vec::new();
    let mut last = stmt;
    let mut node = e.parent_of(NodeRef::Stmt(stmt));
    while let Some(n) = node {
        match n {
            NodeRef::Block(b) => {
                if let Some(pos) = e.body.blocks[b].stmts.iter().position(|&x| x == last) {
                    path.push((b, pos));
                }
                node = e.parent_of(NodeRef::Block(b));
            }
            NodeRef::Stmt(s) => {
                last = s;
                node = e.parent_of(NodeRef::Stmt(s));
            }
            NodeRef::Expr(_) | NodeRef::Pat(_) => node = e.parent_of(n),
        }
    }
    path.reverse();
    path
}

/// Whether `def_stmt` **dominates** the insertion point `before_stmt` (insert
/// *before* it): in structured control flow, `def_stmt`'s enclosing block must be
/// the deepest block common to both paths (not nested in a sibling branch
/// `before_stmt` does not enter) and `def_stmt` must sit strictly earlier there.
/// Used to admit a non-param `FieldAccess` receiver only when its single-assignment
/// def is live at the materialisation point (the receiver-availability gate).
fn def_dominates(
    e: &Engine,
    def_stmt: crate::nir_arena::StmtId,
    before_stmt: crate::nir_arena::StmtId,
) -> bool {
    let dp = stmt_block_path(e, def_stmt);
    let mp = stmt_block_path(e, before_stmt);
    let mut l = 0;
    while l < dp.len() && l < mp.len() && dp[l].0 == mp[l].0 {
        l += 1;
    }
    l > 0 && dp.len() == l && dp[l - 1].1 < mp[l - 1].1
}

/// Whether a `local.get i` placed at `before_stmt` reads the local's value: a
/// parameter is entry-defined, any other local needs its `let` to dominate the
/// point. Answers *where*, not *which version* — `multi_version_locals` settles
/// that separately.
fn leaf_available_at(
    e: &Engine,
    local: u32,
    before_stmt: crate::nir_arena::StmtId,
    param_set: &crate::hashmap::IndexSet<u32>,
) -> bool {
    if param_set.contains(&local) {
        return true;
    }
    match e.local_def(local) {
        Some(def_stmt) => def_dominates(e, def_stmt, before_stmt),
        None => false,
    }
}

/// Whether the receiver of the `FieldAccess` value `rep` is available at the
/// materialisation statement — [`leaf_available_at`] on the receiver's local.
fn receiver_available_at(
    e: &Engine,
    rep: ValueId,
    before_stmt: crate::nir_arena::StmtId,
    param_set: &crate::hashmap::IndexSet<u32>,
) -> bool {
    let ValueKind::FieldAccess { receiver, .. } = e.body.values.kind(rep) else {
        return false;
    };
    let recv = *receiver;
    let local = match e.body.values.kind(recv) {
        ValueKind::Opaque(o) => e.body.values.opaque_source(*o),
        _ => None,
    };
    let Some(crate::nir_value_graph::OpaqueSource::Local(i)) = local else {
        return false;
    };
    leaf_available_at(e, i, before_stmt, param_set)
}

/// Stamp `type_id` onto `v` and its arithmetic children so the WIR extractor
/// can recover each value's width. Arithmetic is width-uniform, so the result
/// type carries down through `Binary` / `Unary`. Returns `false` on a **width
/// conflict**: `ValueKind` is type-erased and hash-consed, so a value shared
/// between two differently-typed uses (e.g. `a+b` as both `i32` and `i64`, or a
/// `0.0` literal as `f32` and `f64`) already carries the other use's type —
/// freezing this use would extract it at the wrong width, so the caller skips
/// it. (`Cast` never appears — excluded by `value_fully_reemittable_locally`.)
#[must_use]
fn record_value_tree_types(e: &mut Engine, v: ValueId, type_id: crate::tir::TypeId) -> bool {
    use crate::nir_value_graph::ValueKind;
    match e.body.values.type_of(v) {
        Some(existing) if existing != type_id => return false,
        Some(_) => {}
        None => e.body.values.set_type(v, type_id),
    }
    match e.body.values.kind(v).clone() {
        ValueKind::Binary { lhs, rhs, .. } => {
            record_value_tree_types(e, lhs, type_id) && record_value_tree_types(e, rhs, type_id)
        }
        ValueKind::Unary { operand, .. } => record_value_tree_types(e, operand, type_id),
        // A `Select`'s arms share its result type; the condition is bool.
        ValueKind::Select { cond, then, else_ } => {
            record_value_tree_types(e, then, type_id)
                && record_value_tree_types(e, else_, type_id)
                && record_value_tree_types(e, cond, crate::tir::TypeTable::BOOL)
        }
        _ => true,
    }
}

/// Freeze re-emittable pure-arith nodes into operand values, function by
/// function. Runs **late** — after every binary-walking pass — so only WIR
/// build (the extractor) ever sees the promoted form; the orphaned arith /
/// local-read skeleton nodes become unreachable from the root and are not
/// emitted.
pub(super) fn freeze_pure_arith(
    project: &mut crate::nir_package::NirPackage,
    include_fields: bool,
    // `Early` runs before the optimize loop, on each function's freshly-built
    // (clean, un-restructured) graph. Only then is it sound to freeze a
    // *constant leaf read* (`Local` / `FieldAccess`) into its literal: born
    // here, the `Operand::Value` survives `inline` / `sroa` (both copy
    // operands), so no later pass re-contextualizes the read into a position the
    // frozen literal is wrong for. A late freeze must not — its graph carries
    // the loop's structural-edit staleness.
    phase: FreezePhase,
) -> bool {
    use crate::nir::NirFunction;
    use crate::nir_engine::EngineBuffers;
    let type_table = project.type_table.borrow();
    let first_param_types = super::alias::first_param_types(project);
    let call_immutability = super::alias::CallImmutability::new(project, &type_table);
    let pure_builtin_callees = project.pure_builtin_callee_ids();
    let mut buffers = EngineBuffers::default();
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if func.body.is_none() {
            continue;
        }
        let NirFunction {
            body,
            locals,
            params,
            address_taken_locals,
            stores_aliased_locals,
            ..
        } = &mut *func;
        let body = body.as_mut().expect("checked above");
        // Address-taken locals (`&x` / `&mut x`): excluded as `FieldAccess`
        // receivers by the receiver-stability gate. Cloned before `Engine::new`.
        let address_taken: crate::hashmap::IndexSet<u32> = address_taken_locals.clone();
        let (aliased, untrackable, mut_escaped) = super::alias::builder_alias_sets(
            body,
            locals,
            address_taken_locals,
            stores_aliased_locals,
            &type_table,
            &first_param_types,
            &call_immutability,
        );
        let param_locals: Vec<u32> = params.iter().map(|p| p.local_index).collect();
        let local_count = locals.len();
        let param_set: crate::hashmap::IndexSet<u32> = param_locals.iter().copied().collect();
        // Locals a call may mutate through a `&mut` escape (ref_1's
        // `set_bool(&mut c, …)`). A constant read of one is point-specific and the
        // build-once graph cannot keep it across the structural passes; an
        // *immutable*-`&`-escaped local (licm's `&config`) is stable and its field
        // constant freezes soundly. Keep a copy before `set_alias_sets` moves it.
        let mut_escaped_leaf = mut_escaped.clone();
        let pure_calls =
            super::alias::pure_calls(body, &type_table, &first_param_types, &call_immutability);
        let mut engine = Engine::new(body, &mut buffers, locals);
        engine.set_alias_sets(aliased, untrackable, mut_escaped);
        engine.set_value_graph_type_table(&type_table);
        engine.set_param_locals(param_locals);
        engine.set_pure_calls(pure_calls);
        engine.set_pure_builtin_callees(&pure_builtin_callees);

        // Locals a frozen value may not name, from the same predicate that
        // decides whether a `Local` read resolves at all.
        let multi_version_locals: crate::hashmap::IndexSet<u32> = (0..local_count as u32)
            .filter(|&i| !engine.local_has_one_version(i))
            .collect();

        // Field reads have no value on the maintained graph.
        let found = if include_fields {
            engine.scoped_field_values()
        } else {
            crate::nir_engine::FieldValues::default()
        };
        let field_values: crate::hashmap::IndexMap<ExprId, ValueId> =
            found.reads.iter().copied().collect();

        // Phase 1: decide every freeze on the clean, unedited graph. A value
        // query never mutates the skeleton, so the verify oracle (which fires
        // on graph queries) only compares build-vs-rebuild here — clean. (A
        // node frozen mid-walk would, inside a loop, leave the maintained
        // graph's recurrence state stale until the next query and read as a
        // spurious over-merge; deciding up front avoids that — and the
        // post-edit graph is not consumed, this being the last pass.)
        let ctx = FreezeCtx {
            type_table: &type_table,
            mut_escaped_leaf: &mut_escaped_leaf,
            multi_version_locals: &multi_version_locals,
            address_taken: &address_taken,
            param_set: &param_set,
            field_values: &field_values,
            phase,
            include_fields,
        };
        let candidates: Vec<ExprId> = engine.body.exprs.keys().collect();
        let mut to_freeze: Vec<(ExprId, ValueId)> = Vec::new();
        for id in candidates {
            if let Some(entry) = classify_candidate(&mut engine, &ctx, id) {
                to_freeze.push(entry);
            }
        }

        // Phase 2: apply. No further graph queries. Group by representative so a
        // value used by several slots can be **materialised once** (availability
        // extraction) — a single pre-header `let _av = <value>` whose uses
        // read `local.get _av` — instead of re-emitting the computation at each
        // use. `record_value_tree_types` stamps the tree's width and skips a
        // width-conflicting value; the two apply strategies then diverge on whether
        // the representative is a `FieldAccess`.
        let mut by_rep: crate::hashmap::IndexMap<ValueId, Vec<ExprId>> =
            crate::hashmap::IndexMap::default();
        for (id, rep) in to_freeze {
            by_rep.entry(rep).or_default().push(id);
        }
        for (rep, ids) in by_rep {
            let id_ty = engine.body.exprs[ids[0]].type_id;
            if !record_value_tree_types(&mut engine, rep, id_ty) {
                continue;
            }
            let is_field = matches!(engine.body.values.kind(rep), ValueKind::FieldAccess { .. });
            if is_field {
                changed |=
                    apply_field_materialise(&mut engine, rep, &ids, id_ty, &param_set, &found);
            } else {
                changed |= apply_value_freeze(&mut engine, rep, &ids, id_ty, &param_set, phase);
            }
        }
    }
    changed
}

/// Per-function analysis inputs consumed by the freeze decision. Borrowed sets
/// computed once in [`freeze_pure_arith`]'s setup; the decision reads them and
/// never mutates the skeleton.
struct FreezeCtx<'a> {
    type_table: &'a crate::tir::TypeTable,
    /// Locals a call may mutate through a retained `&mut` escape — a constant
    /// read of one is point-specific and unstable across the structural passes.
    mut_escaped_leaf: &'a crate::hashmap::IndexSet<u32>,
    /// Locals not provably holding one value: a frozen value's `local.get idx`
    /// must read the version the opaque denotes, which only a single-assignment
    /// local guarantees.
    multi_version_locals: &'a crate::hashmap::IndexSet<u32>,
    /// `&x` / `&mut x` locals — excluded as `FieldAccess` receivers.
    address_taken: &'a crate::hashmap::IndexSet<u32>,
    /// Parameter locals — entry-defined and available at every point.
    param_set: &'a crate::hashmap::IndexSet<u32>,
    /// The `FieldAccess` representative of each field read, from the scratch
    /// re-walk ([`Engine::scoped_field_values`]). Empty unless `include_fields`.
    field_values: &'a crate::hashmap::IndexMap<ExprId, ValueId>,
    phase: FreezePhase,
    include_fields: bool,
}

/// Where a freeze sits relative to the passes that relocate operands.
///
/// The anchor rule (WEP: NIR Optimizer Architecture) turns on this: a frozen
/// operand may name a local only when that local's defining statement travels
/// with the operand. A `let _av = …` does; a parameter's entry def does not. So
/// a freeze with relocation still to come anchors a local-naming value in an
/// `_av`, while the terminal freezes may name a source local directly — nothing
/// moves them afterwards.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum FreezePhase {
    /// Before the fixed-point loop, on each function's clean graph. Plants
    /// context-free values only.
    Early,
    /// Inside the fixed-point loop, so `inline` and `sroa` still follow.
    InLoop,
    /// After the loop. No pass relocates an operand from here on.
    Terminal,
}

/// Decide whether the value at `id` should be frozen, and to which
/// representative. `None` leaves it in the skeleton. Two admission paths: an
/// early constant-leaf promotion, and the pure-arith reemittability gate.
fn classify_candidate(
    engine: &mut Engine,
    ctx: &FreezeCtx,
    id: ExprId,
) -> Option<(ExprId, ValueId)> {
    if engine.is_assign_target(id) || is_let_value(engine, id) {
        return None;
    }
    // Constant-leaf promotion (early only, clean graph). A value-position
    // `Local` / `FieldAccess` read whose graph value is a constant literal is
    // frozen to that literal: context-free, width-correct (the kind carries its
    // own `TypeId`), and — born before any pass — never re-contextualized.
    // `is_place_read` excludes lvalue positions (`&mut x`, a `.field` / method
    // receiver, an assign target) where the storage, not the value, is needed.
    // The root local must not be `mut_escaped`: a write through a retained `&mut`
    // (ref_1's `set_bool(&mut c, …)`) makes the constant point-specific in a way
    // the build-once graph cannot keep across the structural passes (over-merge
    // vs a fresh build). An *immutable*-`&`-escaped root (licm's `&config`) is
    // stable — the callee cannot write through `&Config` — so its field constant
    // freezes soundly; that is the in-loop `FieldAccess` recovery.
    let leaf_root_stable = match &engine.body.exprs[id].kind {
        ExprKind::Local { index, .. } => !ctx.mut_escaped_leaf.contains(index),
        ExprKind::FieldAccess { .. } => super::arena_query::storage_root(engine.body, id)
            .is_none_or(|root| !ctx.mut_escaped_leaf.contains(&root)),
        _ => false,
    };
    if ctx.phase == FreezePhase::Early
        && leaf_root_stable
        && !is_place_read(engine, id)
        && let Some(vid) = engine.value(id)
        && crate::nir_value_graph::builder::is_const_value(&engine.body.values, vid)
    {
        return Some((id, vid));
    }
    if !is_pure_arith(engine, id, ctx.include_fields) {
        return None;
    }
    let rep = match &engine.body.exprs[id].kind {
        ExprKind::FieldAccess { .. } => ctx.field_values.get(&id).copied()?,
        _ => engine.value(id)?,
    };
    // An early freeze may plant only context-free values. A constant means the
    // same thing wherever `inline` and `sroa` copy the operand to; a value naming
    // a local does not, because those passes renumber locals and splice a callee
    // body into a caller, so the slot moves out from under it. The post-loop
    // freezes take these, after the structural passes have finished.
    if ctx.phase == FreezePhase::Early && engine.body.values.names_a_local(rep) {
        return None;
    }
    // A standalone `FieldAccess` is reemittable when its receiver is (it
    // materialises via the source-point path). For every other value,
    // `FieldAccess` is non-reemittable (it cannot be inlined), so
    // `value_fully_reemittable_locally` rejects any value nesting one — keeping
    // arith-over-`FieldAccess` from inline promotion.
    let reemittable = match engine.body.values.kind(rep) {
        ValueKind::FieldAccess { receiver, .. } => {
            let recv = *receiver;
            // Only a scalar field: an aggregate or reference field aliases a
            // mutable backing the `heap_ver` does not pin (`array_index_1`).
            let scalar_field = ctx
                .type_table
                .is_primitive_like(engine.body.exprs[id].type_id);
            // Whether a callee can mutate the receiver is deliberately not asked
            // here — `FieldValues::pinnable_before` answers it at the placement.
            let recv_src = match engine.body.values.kind(recv) {
                ValueKind::Opaque(o) => Some(*o),
                _ => None,
            }
            .and_then(|o| engine.body.values.opaque_source(o));
            let recv_stable = match recv_src {
                Some(crate::nir_value_graph::OpaqueSource::Local(i)) => {
                    let owned_enough = ctx.param_set.contains(&i)
                        || !matches!(
                            ctx.type_table.get(engine.locals()[i as usize].type_id),
                            crate::tir::ResolvedType::Ref(_) | crate::tir::ResolvedType::MutRef(_)
                        );
                    owned_enough
                        && !ctx.multi_version_locals.contains(&i)
                        && !ctx.address_taken.contains(&i)
                }
                _ => false,
            };
            scalar_field
                && recv_stable
                && engine
                    .body
                    .values
                    .value_fully_reemittable_locally(recv, ctx.multi_version_locals)
        }
        _ => engine
            .body
            .values
            .value_fully_reemittable_locally(rep, ctx.multi_version_locals),
    };
    if !reemittable {
        return None;
    }
    if engine.body.values.extraction_duplicates_work(rep) {
        return None;
    }
    Some((id, rep))
}

/// Pin a shared `FieldAccess` in a `let _av` dominating its uses and rewrite
/// each use to a skeleton `Local _av` read. A `FieldAccess` is never
/// inline-reemittable, so this is its only promotion path, and only sharing
/// pays for it — one use would trade a `struct.get` for a store and a read.
fn apply_field_materialise(
    engine: &mut Engine,
    rep: ValueId,
    ids: &[ExprId],
    id_ty: crate::tir::TypeId,
    param_set: &crate::hashmap::IndexSet<u32>,
    found: &crate::nir_engine::FieldValues,
) -> bool {
    if ids.len() < 2 {
        return false;
    }
    let Some((s, b)) = materialise_point(engine, ids) else {
        return false;
    };
    // The shared version proves the uses agree with each other, not with the
    // statement boundary the `let` lands on.
    if !found.pinnable_before(rep, s) {
        return false;
    }
    if !receiver_available_at(engine, rep, s, param_set) {
        return false;
    }
    let span = engine.body.exprs[ids[0]].span;
    let name = format!("_av_{}", engine.locals().len());
    let av = engine.alloc_local(name.clone(), id_ty, /* is_mut */ false);
    let let_stmt = engine.alloc_stmt(
        crate::nir_arena::StmtKind::Let {
            name: name.clone(),
            local_index: av,
            is_mut: false,
            is_reactive: false,
            type_id: id_ty,
            value: crate::nir_arena::Operand::Value(rep),
            skip_value_copy: true,
        },
        span,
    );
    let mut stmts = engine.body.blocks[b].stmts.clone();
    let pos = stmts.iter().position(|&x| x == s).unwrap_or(0);
    stmts.insert(pos, let_stmt);
    engine.set_block_stmts(b, stmts);
    let mut changed = false;
    for &id in ids {
        let span = engine.body.exprs[id].span;
        let lread = engine.alloc_expr(
            ExprKind::Local {
                index: av,
                name: name.clone(),
            },
            id_ty,
            span,
        );
        changed |= engine.redirect_expr(id, crate::nir_arena::Operand::Expr(lread));
    }
    changed
}

/// Whether re-emitting the value tree at `v` can trap. A materialisation runs it
/// once at a point that dominates the uses — which the uses' own guards do not
/// reach — so a trapping value has to stay where the program put it.
///
/// Conservative on `Cast`: classifying one needs the operand's source type, and
/// nothing guarantees the type-erased tree recorded it.
fn value_may_trap(pool: &crate::nir_value_graph::ValuePool, v: ValueId) -> bool {
    match pool.kind(v).clone() {
        ValueKind::Binary { op, lhs, rhs, .. } => {
            super::arena_query::binary_op_may_trap(op)
                || value_may_trap(pool, lhs)
                || value_may_trap(pool, rhs)
        }
        ValueKind::Unary { operand, .. } => value_may_trap(pool, operand),
        ValueKind::Cast { .. } => true,
        ValueKind::Select { cond, then, else_ } => {
            value_may_trap(pool, cond) || value_may_trap(pool, then) || value_may_trap(pool, else_)
        }
        ValueKind::FieldAccess { .. } | ValueKind::LoopPhi { .. } => true,
        ValueKind::Int(..)
        | ValueKind::Float(..)
        | ValueKind::Bool(_)
        | ValueKind::Char(_)
        | ValueKind::Null
        | ValueKind::Unit
        | ValueKind::Const(..)
        | ValueKind::Opaque(_) => false,
    }
}

/// Whether one shared local beats re-emitting `v` at each use. Re-emission pays
/// the tree's operations per use; the local pays one `local.set`, a `local.get`
/// per use, and a slot. A single operation over leaves does not clear that —
/// `0 - precision` is one `i32.sub`, cheaper to repeat than to store and reload
/// — so materialising wants at least two.
///
/// A stand-in for the extraction cost model, on the conservative side: refusing
/// leaves the value re-emitted, which is what every use did before it was a
/// candidate.
fn worth_materialising(pool: &crate::nir_value_graph::ValuePool, v: ValueId) -> bool {
    fn count(pool: &crate::nir_value_graph::ValuePool, v: ValueId, n: &mut u32) {
        if *n >= 2 {
            return;
        }
        match pool.kind(v).clone() {
            ValueKind::Binary { lhs, rhs, .. } => {
                *n += 1;
                count(pool, lhs, n);
                count(pool, rhs, n);
            }
            ValueKind::Unary { operand, .. } | ValueKind::Cast { operand, .. } => {
                *n += 1;
                count(pool, operand, n);
            }
            ValueKind::Select { cond, then, else_ } => {
                *n += 1;
                count(pool, cond, n);
                count(pool, then, n);
                count(pool, else_, n);
            }
            _ => {}
        }
    }
    let mut n = 0;
    count(pool, v, &mut n);
    n >= 2
}

/// Apply strategy for a non-`FieldAccess` (pure-arith / constant) representative.
/// A value used by several slots whose leaves are all available at a common
/// dominator is **materialised once** — a single `let _av = <value>` all uses
/// read via `local.get _av` — at that point, the same placement
/// [`apply_field_materialise`] uses. Every other case redirects each use to the
/// pooled representative inline.
///
/// Leaf availability bounds none of the other hazards: it says where the value
/// *can* be computed, not that the program wanted it computed there, nor that
/// computing it once is cheaper. So a trapping value is refused (hoisting
/// `a / b` out of `if b != 0` traps a program that never divides by zero), the
/// point is the nearest common dominator rather than function entry, keeping a
/// branch-only value out of every call, and a value too cheap to share stays
/// re-emitted ([`worth_materialising`]).
fn apply_value_freeze(
    engine: &mut Engine,
    rep: ValueId,
    ids: &[ExprId],
    id_ty: crate::tir::TypeId,
    param_set: &crate::hashmap::IndexSet<u32>,
    phase: FreezePhase,
) -> bool {
    let mut leaves = crate::hashmap::IndexSet::default();
    engine.body.values.collect_opaque_locals(rep, &mut leaves);
    // Sharing decides first: it is cheap, and `materialise_point` is not.
    let shareable = ids.len() > 1 && worth_materialising(&engine.body.values, rep);
    let point = shareable.then(|| materialise_point(engine, ids)).flatten();
    let anchorable = !leaves.is_empty()
        && !value_may_trap(&engine.body.values, rep)
        && point.is_some_and(|(s, _)| {
            leaves
                .iter()
                .all(|&l| leaf_available_at(engine, l, s, param_set))
        });
    // The anchor rule. With relocation still to come, a value naming a source
    // local may not be planted as-is: `inline` lands the operand where the same
    // index is assigned per call, and `canonical_local`'s one id per index then
    // has two different values sharing it. Anchoring it in an `_av` — a
    // single-assignment `let` in this body, which travels with any copy of the
    // operand — is the only sound form, so refuse when that is not available.
    // The terminal freezes need none of this: nothing moves an operand after
    // them, which is what makes naming a source local safe there.
    let must_anchor = phase == FreezePhase::InLoop && !leaves.is_empty();
    // Anchoring is how such a value is frozen, not a reason to freeze one:
    // materialising a single-use value trades its computation for a `local.set`
    // plus a `local.get` of the same thing. So the sharing gate still decides,
    // and a value that must be anchored but does not clear it stays put.
    let materialize = shareable && anchorable;
    if must_anchor && !materialize {
        return false;
    }
    let mut changed = false;
    if materialize {
        let (anchor, block) = point.expect("`anchorable` holds only with a point");
        let name = format!("_av_{}", engine.locals().len());
        let av = engine.alloc_local(name.clone(), id_ty, /* is_mut */ false);
        let read = engine
            .body
            .values
            .fresh_opaque_with_source(crate::nir_value_graph::OpaqueSource::Local(av));
        engine.body.values.set_type(read, id_ty);
        let let_stmt = engine.alloc_stmt(
            crate::nir_arena::StmtKind::Let {
                name,
                local_index: av,
                is_mut: false,
                is_reactive: false,
                type_id: id_ty,
                value: crate::nir_arena::Operand::Value(rep),
                skip_value_copy: true,
            },
            crate::token::Span::default(),
        );
        let mut stmts = engine.body.blocks[block].stmts.clone();
        let pos = stmts.iter().position(|&x| x == anchor).unwrap_or(0);
        stmts.insert(pos, let_stmt);
        engine.set_block_stmts(block, stmts);
        for &id in ids {
            changed |= engine.redirect_expr(id, crate::nir_arena::Operand::Value(read));
        }
    } else {
        for &id in ids {
            changed |= engine.redirect_expr(id, crate::nir_arena::Operand::Value(rep));
        }
    }
    changed
}

/// The constant [`Value`] for `rep` if its representative kind is a scalar
/// constant, using `at`'s NIR type for integer width. `None` for a non-constant
/// representative or when folding is disabled (no type table).
pub(super) fn extract_const(
    e: &mut Engine,
    rep: ValueId,
    at: ExprId,
) -> Option<crate::const_eval::Value> {
    if !matches!(
        e.value_kind(rep),
        ValueKind::Int(_, _) | ValueKind::Float(_, _) | ValueKind::Bool(_) | ValueKind::Char(_)
    ) {
        return None;
    }
    let vk = e.value_kind(rep).clone();
    let type_id = e.body.exprs[at].type_id;
    let prim = e
        .value_graph_type_table()
        .and_then(|tt| crate::const_eval::prim_of(type_id, tt))
        // An enum case is its discriminant, which lowers to `i32`; the type
        // itself is not a primitive, so `prim_of` cannot name its width.
        .or_else(|| {
            let tt = e.value_graph_type_table()?;
            matches!(tt.get(type_id), crate::tir::ResolvedType::Enum { .. })
                .then_some(crate::tir::PrimitiveType::I32)
        });
    crate::nir_value_graph::value_kind_to_const(&vk, prim)
}

/// Whether `expr` sits in a **place** (lvalue / reference) position, where its
/// storage location — not its value — is what's used: a constant-leaf read here
/// must not be frozen. Covers the `&` / `&mut` / deref referent, a `.field` /
/// `[index]` / method receiver, and an `Assign` LHS. Conservative (a by-value
/// receiver is also rejected), which only forgoes a promotion, never miscompiles.
pub(super) fn is_place_read(e: &Engine, expr: ExprId) -> bool {
    let Some(NodeRef::Expr(parent)) = e.parent_of(NodeRef::Expr(expr)) else {
        return false;
    };
    use crate::nir::NirUnaryOp;
    let op = crate::nir_arena::Operand::Expr(expr);
    match &e.body.exprs[parent].kind {
        ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref,
            expr: inner,
        } => *inner == op,
        ExprKind::Assign { target, .. } => *target == expr,
        ExprKind::FieldAccess { expr: recv, .. } | ExprKind::Index { expr: recv, .. } => {
            *recv == op
        }
        ExprKind::Call {
            args,
            has_receiver: true,
            ..
        } => args.first().is_some_and(|a| a.expr == op),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nir::{NirBinaryOp, NirLocal};
    use crate::nir_arena::{BlockNode, Body, ExprNode, StmtKind, StmtNode};
    use crate::nir_engine::{EngineBuffers, Rule};
    use crate::tir::TypeTable;
    use crate::token::Span;

    fn e(body: &mut Body, kind: ExprKind) -> ExprId {
        ei(body, kind, TypeTable::UNIT)
    }

    fn ei(body: &mut Body, kind: ExprKind, type_id: crate::tir::TypeId) -> ExprId {
        body.exprs.push(ExprNode {
            kind,
            type_id,
            span: Span::default(),
        })
    }

    #[test]
    fn leaves_non_constant_values_untouched() {
        // { a + b; } with no union — nothing to materialize.
        let mut body = Body::empty();
        let a = e(
            &mut body,
            ExprKind::Local {
                index: 0,
                name: "a".into(),
            },
        );
        let b = e(
            &mut body,
            ExprKind::Local {
                index: 1,
                name: "b".into(),
            },
        );
        let sum = e(
            &mut body,
            ExprKind::Binary {
                left: a.into(),
                op: NirBinaryOp::Add,
                right: b.into(),
            },
        );
        let s0 = body.stmts.push(StmtNode {
            kind: StmtKind::Expr(sum.into()),
            span: Span::default(),
        });
        body.root = body.blocks.push(BlockNode {
            stmts: vec![s0],
            span: Span::default(),
        });

        let mut buf = EngineBuffers::default();
        let mut locals: Vec<NirLocal> = Vec::new();
        let mut eng = Engine::new(&mut body, &mut buf, &mut locals);
        let rule = ExtractLiteralRule;
        let rules: Vec<&dyn Rule> = vec![&rule];
        let changed = eng.run(&rules);
        assert!(!changed);
        assert!(matches!(eng.body.exprs[sum].kind, ExprKind::Binary { .. }));
    }

    /// A read `Local` statement, returning the statement id and the value-read
    /// expr id (the materialisation use).
    fn read_stmt(body: &mut Body, name: &str) -> (crate::nir_arena::StmtId, ExprId) {
        let r = ei(
            body,
            ExprKind::Local {
                index: 0,
                name: name.into(),
            },
            TypeTable::I32,
        );
        let s = body.stmts.push(StmtNode {
            kind: StmtKind::Expr(r.into()),
            span: Span::default(),
        });
        (s, r)
    }

    fn block(body: &mut Body, stmts: Vec<crate::nir_arena::StmtId>) -> crate::nir_arena::BlockId {
        body.blocks.push(BlockNode {
            stmts,
            span: Span::default(),
        })
    }

    /// `materialise_point` over uses in one block picks the earliest use's
    /// statement (the straight-line dominance chain).
    #[test]
    fn materialise_point_same_block() {
        let mut body = Body::empty();
        let (s0, u0) = read_stmt(&mut body, "a");
        let (s1, _filler) = read_stmt(&mut body, "f");
        let (s2, u2) = read_stmt(&mut body, "a");
        body.root = block(&mut body, vec![s0, s1, s2]);
        let mut buf = EngineBuffers::default();
        let mut locals: Vec<NirLocal> = Vec::new();
        let eng = Engine::new(&mut body, &mut buf, &mut locals);
        // Uses at positions 0 and 2 -> insert before position 0.
        let (s, b) = materialise_point(&eng, &[u2, u0]).unwrap();
        assert_eq!(b, eng.body.root);
        assert_eq!(s, s0);
    }

    /// Uses in the two arms of one `if` materialise before the `if` — the
    /// nearest common dominator — not inside either branch.
    #[test]
    fn materialise_point_sibling_branches() {
        let mut body = Body::empty();
        let cond = ei(
            &mut body,
            ExprKind::Local {
                index: 9,
                name: "c".into(),
            },
            TypeTable::BOOL,
        );
        let (ts, tu) = read_stmt(&mut body, "a");
        let then_b = block(&mut body, vec![ts]);
        let (es, eu) = read_stmt(&mut body, "a");
        let else_b = block(&mut body, vec![es]);
        let if_s = body.stmts.push(StmtNode {
            kind: StmtKind::If {
                condition: cond.into(),
                then_block: then_b,
                else_block: Some(else_b),
            },
            span: Span::default(),
        });
        let (lead_s, _lead) = read_stmt(&mut body, "lead");
        body.root = block(&mut body, vec![lead_s, if_s]);
        let mut buf = EngineBuffers::default();
        let mut locals: Vec<NirLocal> = Vec::new();
        let eng = Engine::new(&mut body, &mut buf, &mut locals);
        let (s, b) = materialise_point(&eng, &[tu, eu]).unwrap();
        // Common dominator is the root block, at the `if` statement (position 1),
        // not inside either branch.
        assert_eq!(b, eng.body.root);
        assert_eq!(s, if_s);
    }

    /// One use in a branch and one before it: the common dominator is the root,
    /// placed at the earlier (outer) statement so it dominates the nested use.
    #[test]
    fn materialise_point_outer_and_nested() {
        let mut body = Body::empty();
        let cond = ei(
            &mut body,
            ExprKind::Local {
                index: 9,
                name: "c".into(),
            },
            TypeTable::BOOL,
        );
        let (ts, tu) = read_stmt(&mut body, "a");
        let then_b = block(&mut body, vec![ts]);
        let if_s = body.stmts.push(StmtNode {
            kind: StmtKind::If {
                condition: cond.into(),
                then_block: then_b,
                else_block: None,
            },
            span: Span::default(),
        });
        let (outer_s, outer_u) = read_stmt(&mut body, "a");
        body.root = block(&mut body, vec![outer_s, if_s]);
        let mut buf = EngineBuffers::default();
        let mut locals: Vec<NirLocal> = Vec::new();
        let eng = Engine::new(&mut body, &mut buf, &mut locals);
        let (s, b) = materialise_point(&eng, &[tu, outer_u]).unwrap();
        assert_eq!(b, eng.body.root);
        assert_eq!(s, outer_s);
    }

    /// A use inside a loop and one outside have no shared materialisation
    /// point: the common dominator is outside the loop, and a `let` placed
    /// there is evaluated once while the in-loop use reads it every iteration.
    /// The value may name a local the loop reassigns, so refuse rather than
    /// hoist. (`String::substr_bytes` under "Measured dead ends" is this shape
    /// arriving via `inline`.)
    #[test]
    fn materialise_point_refuses_to_hoist_out_of_a_loop() {
        let mut body = Body::empty();
        let (in_s, in_u) = read_stmt(&mut body, "a");
        let loop_body = block(&mut body, vec![in_s]);
        let loop_s = body.stmts.push(StmtNode {
            kind: StmtKind::Loop { body: loop_body },
            span: Span::default(),
        });
        let (out_s, out_u) = read_stmt(&mut body, "a");
        body.root = block(&mut body, vec![out_s, loop_s]);
        let mut locals: Vec<NirLocal> = Vec::new();
        let mut buf = EngineBuffers::default();
        let eng = Engine::new(&mut body, &mut buf, &mut locals);
        assert_eq!(materialise_point(&eng, &[in_u, out_u]), None);
    }

    /// Uses all inside the same loop body still materialise, inside the loop —
    /// re-evaluated per iteration, which is what makes them correct.
    #[test]
    fn materialise_point_stays_inside_a_loop() {
        let mut body = Body::empty();
        let (s0, u0) = read_stmt(&mut body, "a");
        let (s1, _f) = read_stmt(&mut body, "f");
        let (s2, u2) = read_stmt(&mut body, "a");
        let loop_body = block(&mut body, vec![s0, s1, s2]);
        let loop_s = body.stmts.push(StmtNode {
            kind: StmtKind::Loop { body: loop_body },
            span: Span::default(),
        });
        body.root = block(&mut body, vec![loop_s]);
        let mut locals: Vec<NirLocal> = Vec::new();
        let mut buf = EngineBuffers::default();
        let eng = Engine::new(&mut body, &mut buf, &mut locals);
        let (s, b) = materialise_point(&eng, &[u2, u0]).unwrap();
        assert_eq!(b, loop_body);
        assert_eq!(s, s0);
    }

    /// Two uses in the same nested branch materialise inside that branch (the
    /// dominator is the branch block, not the root).
    #[test]
    fn materialise_point_shared_branch() {
        let mut body = Body::empty();
        let cond = ei(
            &mut body,
            ExprKind::Local {
                index: 9,
                name: "c".into(),
            },
            TypeTable::BOOL,
        );
        let (b0, bu0) = read_stmt(&mut body, "a");
        let (b1, _f) = read_stmt(&mut body, "f");
        let (b2, bu2) = read_stmt(&mut body, "a");
        let then_b = block(&mut body, vec![b0, b1, b2]);
        let if_s = body.stmts.push(StmtNode {
            kind: StmtKind::If {
                condition: cond.into(),
                then_block: then_b,
                else_block: None,
            },
            span: Span::default(),
        });
        body.root = block(&mut body, vec![if_s]);
        let mut buf = EngineBuffers::default();
        let mut locals: Vec<NirLocal> = Vec::new();
        let eng = Engine::new(&mut body, &mut buf, &mut locals);
        let (s, b) = materialise_point(&eng, &[bu2, bu0]).unwrap();
        assert_eq!(b, then_b);
        assert_eq!(s, b0);
    }
}
