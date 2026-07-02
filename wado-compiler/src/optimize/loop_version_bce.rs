//! Loop-versioned bounds-check elimination.
//!
//! `condition_implication` removes a check `i < B` only when a dominating
//! guard *statically* implies it. When a loop guard bounds `i` by an
//! unrelated loop-invariant `H` — `for i = 0; i <= H; i += 1 { a[i] = v }`
//! checked against `B = a.used`, with the `H`/`B` relation living at the
//! call site — no static proof exists: the check is semantically live.
//! This pass versions the loop on the runtime residual instead:
//!
//! ```text
//! if H < B { <loop clone, checks deleted> } else { <original loop> }
//! ```
//!
//! Soundness is per-iteration transitivity: the guard gives `i <= H` (`i`
//! unmodified between guard and check, `H` loop-invariant), the residual
//! gives `H < B` (`B` loop-invariant), hence `i < B` — the check's panic
//! arm is dead in the fast clone. The `<` guard variant proves `i <= H-1`,
//! so its residual is `H <= B`. All three locals must share one integer
//! type so the comparisons agree on signedness. The slow arm is the
//! original loop, so assert timing and message stay bit-identical when the
//! residual fails.
//!
//! A versioned fast arm whose body is exactly `a[i] = CONST; i += 1` then
//! collapses to a single `builtin::array_fill` (`try_fill_idiom`) — the
//! bulk `array.fill` replaces per-element `array.set` bounds checks with
//! one range check. The residual also proves `H + 1` cannot overflow,
//! making the fill count exact. Dropped loop-local `let` temps are
//! re-materialized with their final values, so post-loop reads (if any)
//! observe the same state.
//!
//! Runs once after the optimization loop, following
//! `cond_impl_post_promote` (so only statically-unprovable checks remain),
//! and pairs with `BranchPruneRule` + `ElideRule` to sweep the deleted
//! checks' residue. Only leaf loops (no nested loop) are versioned, so
//! cloning never compounds.

use crate::nir::{FunctionRef, NirBinaryOp, NirFunction, NirUnaryOp};
use crate::nir_arena::{ArenaCallArg, BlockId, ExprKind, NodeRef, Operand, StmtId, StmtKind};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::ValueKind;
use crate::tir::TypeTable;
use crate::token::Span;

use super::condition_implication::{
    Binds, BoundKey, build_copy_bindings, eliminate_condition, ge_check_operands, is_panic_block,
    node_modifies, opaque_local, parse_break_guard_head, parse_cmp, parse_var_offset, resolve,
    resolve_panic_ids, stmt_modifies,
};
use super::const_branch_prune::{BranchPruneRule, PruneMode};
use super::dce::{build_callee_descriptors, callee_descriptor};
use super::elide_local::ElideRule;
use crate::module_source::ModuleSource;

/// A versionable loop: guard `var CMP bound` with in-body panic checks
/// `var < check_bound`, all three distinct same-typed locals, `bound` /
/// `check_bound` loop-invariant.
struct Plan {
    /// Block holding the `Loop` statement.
    parent: BlockId,
    /// The `Loop` statement to version.
    loop_stmt: StmtId,
    /// The loop body block.
    loop_body: BlockId,
    /// Index of the guard statement within the loop body.
    guard_idx: usize,
    /// Guard variable `i`.
    var: u32,
    /// Guard bound local `H`.
    bound: u32,
    /// `true` for `i <= H` (residual `H < B`), `false` for `i < H`
    /// (residual `H <= B`).
    guard_le: bool,
    /// Check bound local `B`.
    check_bound: u32,
}

/// The two blocks [`apply_version`] produces for a versioned loop: the version
/// `if`'s then-block (holding exactly the fast `Loop` stmt) and the fast loop's
/// body. The guard fields the fill idiom needs live on the paired [`Plan`].
struct FastArm {
    then_block: BlockId,
    fast_body: BlockId,
}

/// Version eligible loops in every function. Returns whether anything changed.
pub(super) fn version_loops(project: &mut NirPackage) -> bool {
    let fill_id = project.intern_extern(&FunctionRef {
        module_source: ModuleSource::builtin(),
        name: "array_fill".to_string(),
        monomorph_info: None,
        method_info: None,
    });
    let descriptors = build_callee_descriptors(project);
    let panic_ids = resolve_panic_ids(project);
    let pure_builtin_callees = project.pure_builtin_callee_ids();
    let type_table = project.type_table.borrow();
    let mut buffers = EngineBuffers::default();
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        let Some(body_ref) = func.body.as_ref() else {
            continue;
        };
        if !body_contains_loop(body_ref) {
            continue;
        }
        let stores_aliased = func.stores_aliased_locals.clone();
        let NirFunction { body, locals, .. } = &mut *func;
        let body = body.as_mut().expect("checked above");
        let mut engine = Engine::new(body, &mut buffers, locals);
        engine.set_value_graph_type_table(&type_table);
        engine.set_panic_callee_ids(&panic_ids);
        engine.set_pure_builtin_callees(&pure_builtin_callees);

        let binds = build_copy_bindings(engine.body);
        let mut loops: Vec<(BlockId, StmtId, BlockId)> = Vec::new();
        collect_loops(engine.body, engine.body.root, &mut loops);
        let plans: Vec<Plan> = loops
            .into_iter()
            .filter_map(|(parent, loop_stmt, loop_body)| {
                analyze_loop(&engine, &binds, &type_table, parent, loop_stmt, loop_body)
            })
            .collect();
        if plans.is_empty() {
            continue;
        }

        let fast_arms: Vec<FastArm> = plans
            .iter()
            .map(|plan| apply_version(&mut engine, &binds, plan))
            .collect();

        // The splice invalidates licm's loop-entry snapshots (cloned loops
        // have no entry; the original moved under a new guard). An absent
        // entry is sound; clear like `inline` does for non-preserving splices.
        if let Some(vg) = engine.body.value_graph.as_mut() {
            vg.loop_entry_values.clear();
        }

        // Sweep the deleted checks' residue: the `if false { panic }` shells
        // and any now-write-only condition temps.
        let elide_rule = ElideRule::new(&stores_aliased);
        let prune_rule = BranchPruneRule::new(PruneMode::Fixpoint);
        let rules: [&dyn Rule; 2] = [&prune_rule, &elide_rule];
        engine.run(&rules);

        // Fill idiom over the cleaned fast arms; sweep again if it fired.
        let mut filled = false;
        for (plan, arm) in plans.iter().zip(&fast_arms) {
            filled |= try_fill_idiom(&mut engine, &binds, &descriptors, fill_id, plan, arm);
        }
        if filled {
            engine.run(&rules);
        }
        changed = true;
    }
    changed
}

/// Whether any statement in `body` is a `Loop` (cheap pre-filter).
fn body_contains_loop(body: &crate::nir_arena::Body) -> bool {
    body.stmts
        .iter()
        .any(|(_, s)| matches!(s.kind, StmtKind::Loop { .. }))
}

/// Collect every `Loop` statement reachable from `block` with its parent
/// block, recursing through nested statement and expression blocks.
fn collect_loops(
    body: &crate::nir_arena::Body,
    block: BlockId,
    out: &mut Vec<(BlockId, StmtId, BlockId)>,
) {
    for i in 0..body.blocks[block].stmts.len() {
        let s = body.blocks[block].stmts[i];
        if let StmtKind::Loop { body: lb } = &body.stmts[s].kind {
            out.push((block, s, *lb));
        }
        // Recurse into every nested block (loop bodies, if arms, expression
        // blocks) via the generic child walk.
        let mut stack = vec![NodeRef::Stmt(s)];
        while let Some(n) = stack.pop() {
            if let NodeRef::Block(b) = n {
                collect_loops(body, b, out);
                continue;
            }
            body.for_each_child(n, |c| stack.push(c));
        }
    }
}

/// Whether the subtree under `block` contains a `Loop` statement.
fn subtree_contains_loop(body: &crate::nir_arena::Body, block: BlockId) -> bool {
    let mut stack = vec![NodeRef::Block(block)];
    while let Some(n) = stack.pop() {
        if let NodeRef::Stmt(s) = n
            && matches!(body.stmts[s].kind, StmtKind::Loop { .. })
        {
            return true;
        }
        body.for_each_child(n, |c| stack.push(c));
    }
    false
}

/// Parse the loop-head guard `if !(var CMP bound) { break }` (skipping
/// leading `let`s), through both the skeleton and promoted-operand forms.
/// Returns `(guard_idx, var, bound_local, guard_le)`.
fn parse_loop_guard(
    engine: &Engine,
    binds: &Binds,
    loop_body: BlockId,
) -> Option<(usize, u32, u32, bool)> {
    let (guard_idx, cond) = parse_break_guard_head(engine, loop_body)?;
    let inner = peel_not(engine, binds, cond)?;
    let (var, off, bound, op) = parse_cmp(engine, binds, inner)?;
    if off != 0 {
        return None;
    }
    let BoundKey::Local(h) = bound else {
        return None;
    };
    let guard_le = match op {
        NirBinaryOp::LtEq => true,
        NirBinaryOp::Lt => false,
        NirBinaryOp::Add
        | NirBinaryOp::Sub
        | NirBinaryOp::Mul
        | NirBinaryOp::Div
        | NirBinaryOp::Mod
        | NirBinaryOp::Eq
        | NirBinaryOp::NotEq
        | NirBinaryOp::Gt
        | NirBinaryOp::GtEq
        | NirBinaryOp::And
        | NirBinaryOp::Or
        | NirBinaryOp::BitAnd
        | NirBinaryOp::BitOr
        | NirBinaryOp::BitXor
        | NirBinaryOp::Shl
        | NirBinaryOp::Shr
        | NirBinaryOp::RefEq
        | NirBinaryOp::RefNotEq => return None,
    };
    Some((guard_idx, var, h, guard_le))
}

/// Peel a logical `!` in either representation, returning the inner operand.
fn peel_not(engine: &Engine, binds: &Binds, op: Operand) -> Option<Operand> {
    match resolve(engine, binds, op) {
        Operand::Expr(e) => match &engine.body.exprs[e].kind {
            ExprKind::Unary {
                op: NirUnaryOp::Not,
                expr,
            } => Some(*expr),
            _ => None,
        },
        Operand::Value(v) => match engine.body.values.kind(v) {
            ValueKind::Unary {
                op: NirUnaryOp::Not,
                operand,
                ..
            } => Some(Operand::Value(*operand)),
            _ => None,
        },
    }
}

/// Collect check holders `if (var >= B) { panic }` (or `!(var < B)`) nested
/// anywhere in `node`, keyed by their bound local `B`.
fn collect_checks_in_node(
    engine: &Engine,
    binds: &Binds,
    node: NodeRef,
    var: u32,
    out: &mut Vec<(NodeRef, Operand, u32)>,
) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        let cand = match n {
            NodeRef::Stmt(s) => match &engine.body.stmts[s].kind {
                StmtKind::If {
                    condition,
                    then_block,
                    else_block: None,
                } => Some((*condition, *then_block)),
                _ => None,
            },
            NodeRef::Expr(e) => match &engine.body.exprs[e].kind {
                ExprKind::If {
                    condition,
                    then_branch,
                    else_branch: None,
                } => Some((*condition, *then_branch)),
                _ => None,
            },
            NodeRef::Block(_) | NodeRef::Pat(_) => None,
        };
        if let Some((cond, then_b)) = cand
            && is_panic_block(engine, then_b)
            && let Some((left, right)) = ge_check_operands(engine, binds, cond)
            && let Some((cvar, cj)) = parse_var_offset(engine, binds, left)
            && cvar == var
            && cj == 0
            && let Some(b) = parse_raw_local(engine, right)
        {
            out.push((n, cond, b));
        }
        engine.body.for_each_child(n, |c| stack.push(c));
    }
}

/// Parse an operand as a direct local read, without resolving through copy
/// bindings. The check bound must stay the loop-invariant local itself
/// (typically a licm-hoisted `arr.used` preheader local): the residual
/// re-reads it before the loop, and a `binds`-resolved form (the underlying
/// field read) would compare a different representation. A local present as
/// a `let` initializer target is single-assignment by [`build_copy_bindings`]'
/// definition; in-loop re-bindings are rejected by `subtree_redefines`.
fn parse_raw_local(engine: &Engine, op: Operand) -> Option<u32> {
    match op {
        Operand::Expr(e) => match &engine.body.exprs[e].kind {
            ExprKind::Local { index, .. } => Some(*index),
            _ => None,
        },
        Operand::Value(v) => opaque_local(engine, v),
    }
}

/// Analyze one loop for versioning. See the module docs for the conditions.
fn analyze_loop(
    engine: &Engine,
    binds: &Binds,
    type_table: &TypeTable,
    parent: BlockId,
    loop_stmt: StmtId,
    loop_body: BlockId,
) -> Option<Plan> {
    // Leaf loops only: versioning a loop that contains another loop would
    // clone the inner loop as well, compounding code size.
    if subtree_contains_loop(engine.body, loop_body) {
        return None;
    }
    let (guard_idx, var, h, guard_le) = parse_loop_guard(engine, binds, loop_body)?;
    // Scan statements after the guard while `var` / `H` are unmodified —
    // within that window the guard fact `var <= H` still holds.
    let mut checks: Vec<(NodeRef, Operand, u32)> = Vec::new();
    let stmts = engine.body.blocks[loop_body].stmts.clone();
    for &s in stmts.iter().skip(guard_idx + 1) {
        if stmt_modifies(engine, s, var, BoundKey::Local(h)) {
            break;
        }
        collect_checks_in_node(engine, binds, NodeRef::Stmt(s), var, &mut checks);
    }
    let b = checks.first()?.2;
    if b == h {
        // Same bound as the guard: statically decidable, not our case.
        return None;
    }
    // One consistent signedness for guard, check, and residual comparisons.
    let locals = engine.locals();
    let ty = locals.get(var as usize)?.type_id;
    if locals.get(h as usize)?.type_id != ty
        || locals.get(b as usize)?.type_id != ty
        || !type_table.is_integer(ty)
    {
        return None;
    }
    // `H` and `B` must be loop-invariant: the residual is evaluated once at
    // the version point. `var` re-bindings would break the guard-to-check
    // window scan the same way.
    for &s in &stmts {
        if node_modifies(engine, NodeRef::Stmt(s), h, BoundKey::Local(b)) {
            return None;
        }
    }
    if subtree_redefines(engine, loop_body, &[var, h, b]) {
        return None;
    }
    Some(Plan {
        parent,
        loop_stmt,
        loop_body,
        guard_idx,
        var,
        bound: h,
        guard_le,
        check_bound: b,
    })
}

/// Build a `Local` read expression for `index`.
fn local_read(engine: &mut Engine, index: u32, span: Span) -> Operand {
    let ty = engine.locals()[index as usize].type_id;
    let name = engine.locals()[index as usize].name.clone();
    let e = engine.alloc_expr(ExprKind::Local { index, name }, ty, span);
    Operand::Expr(e)
}

/// Apply one versioning plan: clone the loop, delete the implied checks in
/// the clone, and replace the loop with
/// `if <residual> { fast } else { original }`.
fn apply_version(engine: &mut Engine, binds: &Binds, plan: &Plan) -> FastArm {
    let span = engine.body.stmts[plan.loop_stmt].span;
    let fast_body = engine.clone_block(plan.loop_body);
    // `fast_body` is a structural clone of `plan.loop_body`, so the guard/check
    // layout `analyze_loop` recorded on `plan` holds verbatim — no re-parse.
    eliminate_checks_in_fast(engine, binds, plan, fast_body);

    let fast_stmt = engine.alloc_stmt(StmtKind::Loop { body: fast_body }, span);
    let slow_stmt = engine.alloc_stmt(
        StmtKind::Loop {
            body: plan.loop_body,
        },
        span,
    );
    let then_block = engine.alloc_block(vec![fast_stmt], span);
    let else_block = engine.alloc_block(vec![slow_stmt], span);

    let h_read = local_read(engine, plan.bound, span);
    let b_read = local_read(engine, plan.check_bound, span);
    let op = if plan.guard_le {
        NirBinaryOp::Lt
    } else {
        NirBinaryOp::LtEq
    };
    let residual = engine.alloc_expr(
        ExprKind::Binary {
            left: h_read,
            op,
            right: b_read,
        },
        TypeTable::BOOL,
        span,
    );
    let if_stmt = engine.alloc_stmt(
        StmtKind::If {
            condition: Operand::Expr(residual),
            then_block,
            else_block: Some(else_block),
        },
        span,
    );
    let mut stmts = engine.body.blocks[plan.parent].stmts.clone();
    let pos = stmts
        .iter()
        .position(|&s| s == plan.loop_stmt)
        .expect("versioned loop stmt must be in its parent block");
    stmts[pos] = if_stmt;
    engine.set_block_stmts(plan.parent, stmts);

    FastArm {
        then_block,
        fast_body,
    }
}

/// In the fast clone, drive every implied check to `false` (the paired
/// `BranchPruneRule` removes the dead panic arms), and constify the
/// single-purpose `let __cond = <cmp>` temp feeding each check — in the fast
/// arm the comparison is provably constant, so this is exact for any reader.
/// `fast_body` is a clone of `plan.loop_body`, so `plan`'s guard layout applies.
fn eliminate_checks_in_fast(engine: &mut Engine, binds: &Binds, plan: &Plan, fast_body: BlockId) {
    let (var, h) = (plan.var, plan.bound);
    let mut checks: Vec<(NodeRef, Operand, u32)> = Vec::new();
    let stmts = engine.body.blocks[fast_body].stmts.clone();
    for &s in stmts.iter().skip(plan.guard_idx + 1) {
        if stmt_modifies(engine, s, var, BoundKey::Local(h)) {
            break;
        }
        collect_checks_in_node(engine, binds, NodeRef::Stmt(s), var, &mut checks);
    }
    for (holder, cond, b) in checks {
        if b != plan.check_bound {
            continue;
        }
        constify_check_temp(engine, cond, fast_body);
        eliminate_condition(engine, holder, cond);
    }
}

/// If the check condition reads a `let`-bound temp (`!__cond` or `__cond`),
/// replace that temp's initializer in the fast clone with the constant the
/// comparison provably evaluates to there (`!c` panics ⇒ `c` is `true`;
/// `c` panics ⇒ `c` is `false`), deleting the per-iteration compare.
fn constify_check_temp(engine: &mut Engine, cond: Operand, fast_body: BlockId) {
    let Operand::Expr(ce) = cond else {
        return;
    };
    let (temp, konst) = match &engine.body.exprs[ce].kind {
        ExprKind::Unary {
            op: NirUnaryOp::Not,
            expr,
        } => match expr.as_expr().map(|e| &engine.body.exprs[e].kind) {
            Some(ExprKind::Local { index, .. }) => (*index, true),
            _ => return,
        },
        ExprKind::Local { index, .. } => (*index, false),
        _ => return,
    };
    // Find the temp's `let` inside the fast clone and overwrite its value.
    let mut stack = vec![NodeRef::Block(fast_body)];
    while let Some(n) = stack.pop() {
        if let NodeRef::Stmt(s) = n
            && let StmtKind::Let {
                local_index, value, ..
            } = &engine.body.stmts[s].kind
            && *local_index == temp
            && super::arena_query::is_pure_operand(engine.body, *value)
        {
            let v = engine
                .body
                .values
                .alloc_unshared(ValueKind::Bool(konst), TypeTable::BOOL);
            if let StmtKind::Let { value, .. } = &mut engine.body.stmts[s].kind {
                *value = Operand::Value(v);
            }
            engine.enqueue(NodeRef::Stmt(s));
            return;
        }
        engine.body.for_each_child(n, |c| stack.push(c));
    }
}

/// Whether the subtree under `block` re-binds any of `locals` via `let`, or
/// contains any `LetDestructure` (whose pattern could bind one). A per-
/// iteration re-binding would invalidate the loop-invariance the residual
/// relies on; `node_modifies` only tracks `Assign` / `&mut`, so `let`
/// re-definitions need this separate scan. (Fresh NIR mints a new local per
/// shadowing `let`, so this is a defensive guard against synthesized IR.)
fn subtree_redefines(engine: &Engine, block: BlockId, locals: &[u32]) -> bool {
    let mut stack = vec![NodeRef::Block(block)];
    while let Some(n) = stack.pop() {
        if let NodeRef::Stmt(s) = n {
            match &engine.body.stmts[s].kind {
                StmtKind::Let { local_index, .. } => {
                    if locals.contains(local_index) {
                        return true;
                    }
                }
                StmtKind::LetDestructure { .. } => return true,
                StmtKind::Expr(_)
                | StmtKind::Return { .. }
                | StmtKind::If { .. }
                | StmtKind::Loop { .. }
                | StmtKind::Break { .. }
                | StmtKind::Continue
                | StmtKind::LabeledBlock { .. } => {}
            }
        }
        engine.body.for_each_child(n, |c| stack.push(c));
    }
    false
}

/// The constant-fill idiom over a cleaned fast arm:
///
/// ```text
/// loop { if !(i CMP H) break; [pure lets]; array_set(A, i, CONST); i += 1 }
/// ```
///
/// becomes
///
/// ```text
/// if i CMP H {
///     array_fill(A, i, CONST, <count>);
///     [lets re-materialized with final values];
///     i = <final>;
/// }
/// ```
///
/// where `<count>` is `H + 1 - i` for `<=` (no overflow: the fast arm's
/// residual proves `H < B`) and `H - i` for `<`. The wrapping `if` preserves
/// the zero-iteration case exactly (no fill, no local updates).
fn try_fill_idiom(
    engine: &mut Engine,
    binds: &Binds,
    descriptors: &[FunctionRef],
    fill_id: crate::nir::FuncId,
    plan: &Plan,
    arm: &FastArm,
) -> bool {
    // The fill form needs the guard at index 0 (no leading lets); `plan` carries
    // the guard layout from analysis, and `arm.fast_body` is its structural clone.
    let (var, h, guard_le) = (plan.var, plan.bound, plan.guard_le);
    if plan.guard_idx != 0 {
        return false;
    }
    // The then-block must still hold exactly the fast loop statement.
    let then_stmts = engine.body.blocks[arm.then_block].stmts.clone();
    let [loop_stmt] = then_stmts[..] else {
        return false;
    };
    if !matches!(
        engine.body.stmts[loop_stmt].kind,
        StmtKind::Loop { body } if body == arm.fast_body
    ) {
        return false;
    }
    let stmts = engine.body.blocks[arm.fast_body].stmts.clone();
    // Shape: guard first, then pure lets, one array_set, and `i += 1` last.
    let n = stmts.len();
    if n < 3 {
        return false;
    }
    // Increment: `i = i + 1` as the last statement.
    let StmtKind::Expr(incr_op) = &engine.body.stmts[stmts[n - 1]].kind else {
        return false;
    };
    let Some(incr_e) = incr_op.as_expr() else {
        return false;
    };
    let ExprKind::Assign { target, value } = &engine.body.exprs[incr_e].kind else {
        return false;
    };
    if !matches!(&engine.body.exprs[*target].kind, ExprKind::Local { index, .. } if *index == var) {
        return false;
    }
    if parse_var_offset(engine, binds, *value) != Some((var, 1)) {
        return false;
    }
    // The array_set call just before the increment.
    let StmtKind::Expr(call_op) = &engine.body.stmts[stmts[n - 2]].kind else {
        return false;
    };
    let Some(call_e) = call_op.as_expr() else {
        return false;
    };
    let ExprKind::Call { func_id, args, .. } = &engine.body.exprs[call_e].kind else {
        return false;
    };
    let d = callee_descriptor(descriptors, *func_id);
    let is_array_set = d.builtin_name().as_deref() == Some("builtin::array_set")
        || d.monomorphized_builtin_name().as_deref() == Some("builtin::array_set");
    if !is_array_set || args.len() != 3 {
        return false;
    }
    let (arr_op, arr_is_mut) = (args[0].expr, args[0].is_mut);
    let idx_op = args[1].expr;
    let val_op = args[2].expr;
    // Index must be exactly `i` (offset 0).
    if parse_var_offset(engine, binds, idx_op) != Some((var, 0)) {
        return false;
    }
    // Value must be a constant (loop-invariant by construction, and safe to
    // re-emit once).
    let Operand::Value(val_v) = val_op else {
        return false;
    };
    if !matches!(
        engine.body.values.kind(val_v),
        ValueKind::Int(..) | ValueKind::Float(..) | ValueKind::Bool(_) | ValueKind::Char(_)
    ) {
        return false;
    }
    // Array operand: a (`&mut`) read of a local defined outside the loop —
    // pure, so evaluating it once instead of per iteration is exact.
    let arr_local = match arr_op {
        Operand::Expr(e) => match &engine.body.exprs[e].kind {
            ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr,
            } => match expr.as_expr().map(|ie| &engine.body.exprs[ie].kind) {
                Some(ExprKind::Local { index, .. }) => *index,
                _ => return false,
            },
            ExprKind::Local { index, .. } => *index,
            _ => return false,
        },
        Operand::Value(_) => return false,
    };
    // Middle statements: pure `let`s whose finals we can re-materialize —
    // `let t = i` (final: last iterated `i`) or `let t = <const value>`.
    enum TempFinal {
        LastVar(u32),
        Const(u32, Operand),
    }
    let mut temps: Vec<TempFinal> = Vec::new();
    for &s in &stmts[1..n - 2] {
        let StmtKind::Let {
            local_index, value, ..
        } = &engine.body.stmts[s].kind
        else {
            return false;
        };
        if *local_index == arr_local {
            // The array local must predate the loop for the once-evaluation
            // to be exact.
            return false;
        }
        if !super::arena_query::is_pure_operand(engine.body, *value) {
            return false;
        }
        if parse_var_offset(engine, binds, *value) == Some((*local_index, 0)) {
            // Self-referential resolution artifact; treat as opaque.
            return false;
        }
        if parse_var_offset(engine, binds, *value) == Some((var, 0)) {
            temps.push(TempFinal::LastVar(*local_index));
        } else if matches!(value, Operand::Value(v) if matches!(
            engine.body.values.kind(*v),
            ValueKind::Int(..) | ValueKind::Float(..) | ValueKind::Bool(_) | ValueKind::Char(_)
        )) {
            temps.push(TempFinal::Const(*local_index, *value));
        } else {
            return false;
        }
    }
    let span = engine.body.stmts[loop_stmt].span;
    let ty = engine.locals()[var as usize].type_id;
    let one = engine.body.values.int_typed(1, ty);

    // `H + 1` (for `<=`) / `H` (for `<`) — the exclusive upper end.
    let upper = |engine: &mut Engine| -> Operand {
        let h_read = local_read(engine, h, span);
        if guard_le {
            let e = engine.alloc_expr(
                ExprKind::Binary {
                    left: h_read,
                    op: NirBinaryOp::Add,
                    right: Operand::Value(one),
                },
                ty,
                span,
            );
            Operand::Expr(e)
        } else {
            h_read
        }
    };

    let upper_for_len = upper(engine);
    let i_read = local_read(engine, var, span);
    let count = engine.alloc_expr(
        ExprKind::Binary {
            left: upper_for_len,
            op: NirBinaryOp::Sub,
            right: i_read,
        },
        ty,
        span,
    );
    let offset = local_read(engine, var, span);
    let fill_call = engine.alloc_expr(
        ExprKind::Call {
            func_id: fill_id,
            type_args: Vec::new(),
            args: vec![
                ArenaCallArg {
                    expr: arr_op,
                    is_mut: arr_is_mut,
                },
                ArenaCallArg {
                    expr: offset,
                    is_mut: false,
                },
                ArenaCallArg {
                    expr: val_op,
                    is_mut: false,
                },
                ArenaCallArg {
                    expr: Operand::Expr(count),
                    is_mut: false,
                },
            ],
        },
        TypeTable::UNIT,
        span,
    );
    let fill_stmt = engine.alloc_stmt(StmtKind::Expr(Operand::Expr(fill_call)), span);

    let mut body_stmts = vec![fill_stmt];
    // Re-materialize temp finals: `t = i` observed `i`'s last iterated value
    // (`H` for `<=`, `H - 1` for `<`); consts keep their value.
    for t in &temps {
        match t {
            TempFinal::LastVar(l) => {
                let final_val = if guard_le {
                    local_read(engine, h, span)
                } else {
                    let h_read = local_read(engine, h, span);
                    let e = engine.alloc_expr(
                        ExprKind::Binary {
                            left: h_read,
                            op: NirBinaryOp::Sub,
                            right: Operand::Value(one),
                        },
                        ty,
                        span,
                    );
                    Operand::Expr(e)
                };
                body_stmts.push(alloc_local_set(engine, *l, final_val, span));
            }
            TempFinal::Const(l, v) => {
                body_stmts.push(alloc_local_set(engine, *l, *v, span));
            }
        }
    }
    // `i`'s final value: `H + 1` for `<=`, `H` for `<`.
    let i_final = upper(engine);
    body_stmts.push(alloc_local_set(engine, var, i_final, span));

    // `if i CMP H { ... }` preserves the zero-iteration case.
    let i_read2 = local_read(engine, var, span);
    let h_read2 = local_read(engine, h, span);
    let enter = engine.alloc_expr(
        ExprKind::Binary {
            left: i_read2,
            op: if guard_le {
                NirBinaryOp::LtEq
            } else {
                NirBinaryOp::Lt
            },
            right: h_read2,
        },
        TypeTable::BOOL,
        span,
    );
    let body_block = engine.alloc_block(body_stmts, span);
    let fill_if = engine.alloc_stmt(
        StmtKind::If {
            condition: Operand::Expr(enter),
            then_block: body_block,
            else_block: None,
        },
        span,
    );
    engine.set_block_stmts(arm.then_block, vec![fill_if]);
    true
}

/// A `Let` statement re-binding local `l` to `value` (locals are
/// function-scoped slots, so a second `let` is a plain re-definition).
fn alloc_local_set(engine: &mut Engine, l: u32, value: Operand, span: Span) -> StmtId {
    let name = engine.locals()[l as usize].name.clone();
    let ty = engine.locals()[l as usize].type_id;
    let is_mut = engine.locals()[l as usize].is_mut;
    engine.alloc_stmt(
        StmtKind::Let {
            name,
            local_index: l,
            is_mut,
            is_reactive: false,
            type_id: ty,
            value,
            skip_value_copy: true,
        },
        span,
    )
}
