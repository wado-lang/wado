//! Loop-versioned bounds-check elimination, for the checks
//! `condition_implication` cannot statically prove: the loop becomes
//! `if H < B { <clone, checks deleted> } else { <original> }`, sound by
//! per-iteration transitivity from the guard's `i <= H`. A fast arm that is
//! exactly `a[i] = CONST; i += 1` then collapses to one [`try_fill_idiom`].
//!
//! Where the entry value reads as no constant, the check's floor joins the
//! residual too: `i >= FLOOR` at the version point, carried to every later
//! value by a step the residual proves non-negative and clear of the wrap.

use std::ops::ControlFlow;

use crate::nir::{FunctionRef, NirBinaryOp, NirFunction, NirUnaryOp};
use crate::nir_arena::{ArenaCallArg, BlockId, Body, ExprKind, NodeRef, Operand, StmtId, StmtKind};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::ValueKind;
use crate::tir::{TypeId, TypeTable};
use crate::token::Span;

use super::alias::{CallImmutability, builder_alias_sets, first_param_types};
use super::arena_query::{block_contains_loop, has_break_to};
use super::condition_implication::{
    Binds, BoundKey, Conjunct, InductionStep, build_copy_bindings, capture_block_binding,
    check_conjuncts, eliminate_condition, induction_entry, induction_step, negated_operand,
    node_modifies, panic_guard_check, parse_break_guard_head, parse_cmp, parse_var_offset,
    peel_capture_block, resolve_panic_ids, stmt_modifies,
};
use super::const_branch_prune::{BranchPruneRule, PruneMode};
use super::dce::{DescriptorCache, callee_descriptor};
use super::elide_local::ElideRule;
use crate::module_source::ModuleSource;
use crate::nir::FuncId;
use crate::nir_arena;
use crate::optimize::arena_query::{
    binary_parts, expr_node_may_trap_typed, is_pure_operand, local_written_by,
    mentions_local_except, operand_local,
};
use crate::optimize::mod_ref::compute_fn_effects;

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
    /// The floor the checks demand, when it is the residual's to establish.
    floor: Option<RuntimeFloor>,
}

/// A floor the residual tests at the version point, with the step that carries
/// it: non-negative, and leaving `H` a step below `type_max` so none wraps.
struct RuntimeFloor {
    demanded: i64,
    step: InductionStep,
    type_max: u64,
}

/// The two blocks [`apply_version`] produces for a versioned loop: the version
/// `if`'s then-block (holding exactly the fast `Loop` stmt) and the fast loop's
/// body. The guard fields the fill idiom needs live on the paired [`Plan`].
struct FastArm {
    version_if: StmtId,
    then_block: BlockId,
    fast_body: BlockId,
}

/// Version eligible loops in every function. Returns whether anything changed.
pub(super) fn version_loops(project: &mut NirPackage, cache: &mut DescriptorCache) -> bool {
    let fill_id = project.intern_extern(&FunctionRef {
        module_source: ModuleSource::builtin(),
        name: "array_fill".to_string(),
        monomorph_info: None,
        method_info: None,
    });
    let descriptors = cache.descriptors(project);
    let panic_ids = resolve_panic_ids(project);
    let pure_builtin_callees = project.pure_builtin_callee_ids();
    // Only a body with a loop is versioned, so a program with none pays nothing.
    let effects = if project
        .functions
        .iter()
        .any(|f| f.borrow().body.as_ref().is_some_and(body_contains_loop))
    {
        compute_fn_effects(project)
    } else {
        Vec::new()
    };
    let type_table = project.type_table.borrow();
    let first_param_types = first_param_types(project);
    let call_immutability = CallImmutability::new(project, &type_table);
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
        let NirFunction {
            body,
            locals,
            address_taken_locals,
            stores_aliased_locals,
            ..
        } = &mut *func;
        let body = body.as_mut().expect("checked above");
        let (aliased, untrackable, mut_escaped) = builder_alias_sets(
            body,
            locals,
            address_taken_locals,
            stores_aliased_locals,
            &type_table,
            &first_param_types,
            &call_immutability,
        );
        let mut engine = Engine::new(body, &mut buffers, locals);
        engine.set_alias_sets(aliased, untrackable, mut_escaped);
        engine.set_value_graph_type_table(&type_table);
        engine.set_panic_callee_ids(&panic_ids);
        engine.set_pure_builtin_callees(&pure_builtin_callees);

        let binds = build_copy_bindings(&engine);
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
        let elide_rule = ElideRule::new(&stores_aliased, &effects);
        let prune_rule = BranchPruneRule::new(PruneMode::Fixpoint);
        let rules: [&dyn Rule; 2] = [&prune_rule, &elide_rule];
        engine.run(&rules);

        // Fill idiom over the cleaned fast arms; sweep again if it fired.
        let mut filled = false;
        for (plan, arm) in plans.iter().zip(&fast_arms) {
            filled |= try_fill_idiom(&mut engine, &binds, descriptors, fill_id, plan, arm);
        }
        if filled {
            engine.run(&rules);
        }
        changed = true;
    }
    changed
}

/// Whether any statement in `body` is a `Loop` (cheap pre-filter).
fn body_contains_loop(body: &nir_arena::Body) -> bool {
    body.stmts
        .iter()
        .any(|(_, s)| matches!(s.kind, StmtKind::Loop { .. }))
}

/// Collect every `Loop` statement reachable from `block` with its parent
/// block, recursing through nested statement and expression blocks.
fn collect_loops(
    body: &nir_arena::Body,
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
        body.walk_nodes_under::<()>(NodeRef::Stmt(s), |n| {
            if let NodeRef::Block(b) = n {
                collect_loops(body, b, out);
                return ControlFlow::Continue(false);
            }
            ControlFlow::Continue(true)
        });
    }
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
    let inner = negated_operand(engine, binds, cond)?;
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

/// A versionable in-body check over the guard variable.
struct Check {
    holder: NodeRef,
    cond: Operand,
    /// The check's bound local `B`.
    bound: u32,
    /// The strongest constant floor the check demands of `var`, or `i64::MIN`
    /// when it demands none.
    floor: i64,
}

/// Collect every versionable check nested anywhere in `node`.
fn collect_checks_in_node(
    engine: &Engine,
    binds: &Binds,
    node: NodeRef,
    var: u32,
    out: &mut Vec<Check>,
) {
    engine.body.for_each_node_under(node, |n| {
        if let Some(cond) = panic_guard_check(engine, n)
            && let Some(check) = parse_versionable_check(engine, binds, n, cond, var)
        {
            out.push(check);
        }
    });
}

/// Read one panic guard as a versionable check: exactly one upper-bound
/// conjunct `var < B` over the guard variable at offset 0, and any number of
/// constant floors under that same variable — the shape `assert 0 <= i < n`
/// lowers to. Any other conjunct refuses the check, so versioning never deletes
/// a condition it has only half proved.
fn parse_versionable_check(
    engine: &Engine,
    binds: &Binds,
    holder: NodeRef,
    cond: Operand,
    var: u32,
) -> Option<Check> {
    let mut bound = None;
    let mut floor = i64::MIN;
    for part in check_conjuncts(engine, binds, cond)? {
        match part {
            Conjunct::Lt(left, right) => {
                let (cvar, cj) = parse_var_offset(engine, binds, left)?;
                if cvar != var || cj != 0 || bound.is_some() {
                    return None;
                }
                bound = Some(parse_raw_local(engine, binds, right)?);
            }
            Conjunct::AtLeast(fvar, foff, f) => {
                if fvar != var || foff != 0 {
                    return None;
                }
                floor = floor.max(f);
            }
        }
    }
    Some(Check {
        holder,
        cond,
        bound: bound?,
        floor,
    })
}

/// Parse an operand as a direct local read, without resolving through copy
/// bindings. The check bound must stay the loop-invariant local itself
/// (typically a licm-hoisted `arr.used` preheader local): the residual
/// re-reads it before the loop, and a `binds`-resolved form (the underlying
/// field read) would compare a different representation. A local present as
/// a `let` initializer target is single-assignment by [`build_copy_bindings`]'
/// definition; in-loop re-bindings are rejected by `subtree_redefines`.
fn parse_raw_local(engine: &Engine, binds: &Binds, op: Operand) -> Option<u32> {
    operand_local(engine.body, peel_capture_block(engine, binds, op))
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
    if block_contains_loop(engine.body, loop_body) {
        return None;
    }
    let (guard_idx, var, h, guard_le) = parse_loop_guard(engine, binds, loop_body)?;
    // Scan statements after the guard while `var` / `H` are unmodified —
    // within that window the guard fact `var <= H` still holds.
    let mut checks: Vec<Check> = Vec::new();
    let stmts = engine.body.blocks[loop_body].stmts.clone();
    for &s in stmts.iter().skip(guard_idx + 1) {
        if stmt_modifies(engine, s, var, BoundKey::Local(h)) {
            break;
        }
        collect_checks_in_node(engine, binds, NodeRef::Stmt(s), var, &mut checks);
    }
    // A floor is answered from the fast arm's own facts, so it must hold for
    // every collected check before any of them is deleted.
    let demanded = checks.iter().map(|c| c.floor).max()?;
    let b = checks.first()?.bound;
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
    let floor = match demanded {
        i64::MIN => None,
        _ => match induction_entry(engine, binds, parent, loop_stmt, loop_body, var) {
            Some(entry) if entry >= demanded => None,
            // A constant entry below the floor is a loop no fast arm can take.
            Some(_) => return None,
            None => Some(runtime_floor(
                engine, binds, type_table, loop_body, var, ty, demanded,
            )?),
        },
    };
    let step_local = floor.as_ref().and_then(|f| match f.step {
        InductionStep::Local(s) => Some(s),
        InductionStep::Const(..) => None,
    });
    if step_local.is_some_and(|s| locals.get(s as usize).map(|l| l.type_id) != Some(ty)) {
        return None;
    }
    // `H`, `B` and the step must be loop-invariant: the residual is evaluated
    // once at the version point. `var` re-bindings would break the
    // guard-to-check window scan the same way.
    for &s in &stmts {
        let node = NodeRef::Stmt(s);
        if node_modifies(engine, node, h, BoundKey::Local(b))
            || step_local.is_some_and(|sl| node_modifies(engine, node, sl, BoundKey::Local(b)))
        {
            return None;
        }
    }
    if subtree_redefines(engine, loop_body, &[var, h, b, step_local.unwrap_or(h)]) {
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
        floor,
    })
}

/// The floor terms for a loop whose entry reads as no constant: a step that
/// never lowers `var`, and the type maximum the no-wrap term compares against.
fn runtime_floor(
    engine: &Engine,
    binds: &Binds,
    type_table: &TypeTable,
    loop_body: BlockId,
    var: u32,
    ty: TypeId,
    demanded: i64,
) -> Option<RuntimeFloor> {
    // The residual spells the floor as raw constant bits, which only a
    // non-negative value reads the same in every integer width.
    if demanded < 0 {
        return None;
    }
    let step = induction_step(engine, binds, loop_body, var)?;
    if let InductionStep::Const(k) = step
        && k < 0
    {
        return None;
    }
    let max = type_table.primitive_head(ty)?.int_max()?;
    let type_max = u64::try_from(max).expect("a scalar integer maximum fits u64");
    Some(RuntimeFloor {
        demanded,
        step,
        type_max,
    })
}

/// Build a `Local` read expression for `index`.
fn local_read(engine: &mut Engine, index: u32, span: Span) -> Operand {
    let ty = engine.locals()[index as usize].type_id;
    let name = engine.locals()[index as usize].name.clone();
    let e = engine.alloc_expr(ExprKind::Local { index, name }, ty, span);
    Operand::Expr(e)
}

fn alloc_binary(
    engine: &mut Engine,
    left: Operand,
    op: NirBinaryOp,
    right: Operand,
    ty: TypeId,
    span: Span,
) -> Operand {
    let e = engine.alloc_expr(ExprKind::Binary { left, op, right }, ty, span);
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
    let mut residual = alloc_binary(engine, h_read, op, b_read, TypeTable::BOOL, span);
    if let Some(floor) = &plan.floor {
        residual = and_floor_terms(engine, plan, floor, residual, span);
    }
    let if_stmt = engine.alloc_stmt(
        StmtKind::If {
            condition: residual,
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
        version_if: if_stmt,
        then_block,
        fast_body,
    }
}

/// Conjoin the floor terms onto `residual`: `var >= demanded`, and the
/// `step >= 0` / `H <= MAX - step` pair that says `var` rises without wrapping.
fn and_floor_terms(
    engine: &mut Engine,
    plan: &Plan,
    floor: &RuntimeFloor,
    residual: Operand,
    span: Span,
) -> Operand {
    let ty = engine.locals()[plan.var as usize].type_id;
    let pred = |e: &mut Engine, l, op, r| alloc_binary(e, l, op, r, TypeTable::BOOL, span);

    let var_read = local_read(engine, plan.var, span);
    let demanded = engine.const_operand(ValueKind::Int(floor.demanded as u64, ty), ty);
    let at_least = pred(engine, var_read, NirBinaryOp::GtEq, demanded);
    let mut out = pred(engine, residual, NirBinaryOp::And, at_least);

    let headroom = match floor.step {
        // The residual already puts the last `i + 1` at or under `B`, so no wrap.
        InductionStep::Const(k) if k <= 1 => return out,
        InductionStep::Const(k) => {
            let bits = floor.type_max.wrapping_sub(k as u64);
            engine.const_operand(ValueKind::Int(bits, ty), ty)
        }
        InductionStep::Local(s) => {
            let step_read = local_read(engine, s, span);
            let zero = engine.const_operand(ValueKind::Int(0, ty), ty);
            let rising = pred(engine, step_read, NirBinaryOp::GtEq, zero);
            out = pred(engine, out, NirBinaryOp::And, rising);
            let max = engine.const_operand(ValueKind::Int(floor.type_max, ty), ty);
            let step_read = local_read(engine, s, span);
            alloc_binary(engine, max, NirBinaryOp::Sub, step_read, ty, span)
        }
    };
    let h_read = local_read(engine, plan.bound, span);
    let no_wrap = pred(engine, h_read, NirBinaryOp::LtEq, headroom);
    pred(engine, out, NirBinaryOp::And, no_wrap)
}

/// In the fast clone, drive every implied check to `false` (the paired
/// `BranchPruneRule` removes the dead panic arms), and constify the
/// single-purpose `let $cond = <cmp>` temp feeding each check — in the fast
/// arm the comparison is provably constant, so this is exact for any reader.
/// `fast_body` is a clone of `plan.loop_body`, so `plan`'s guard layout applies.
fn eliminate_checks_in_fast(engine: &mut Engine, binds: &Binds, plan: &Plan, fast_body: BlockId) {
    let (var, h) = (plan.var, plan.bound);
    let mut checks: Vec<Check> = Vec::new();
    let stmts = engine.body.blocks[fast_body].stmts.clone();
    for &s in stmts.iter().skip(plan.guard_idx + 1) {
        if stmt_modifies(engine, s, var, BoundKey::Local(h)) {
            break;
        }
        collect_checks_in_node(engine, binds, NodeRef::Stmt(s), var, &mut checks);
    }
    for check in checks {
        // As in `condition_implication`: the elimination drops the condition
        // expression, so a condition that writes must stay.
        if check.bound != plan.check_bound || !is_pure_operand(engine.body, check.cond) {
            continue;
        }
        constify_check_temp(engine, binds, check.cond, plan.var, fast_body);
        eliminate_condition(engine, check.holder, check.cond);
    }
}

/// If the check condition reads a `let`-bound temp, replace that temp's
/// initializer in the fast clone with the constant the comparison provably
/// evaluates to (`!c` panics ⇒ `c` is `true`), deleting the per-iteration
/// compare. Overwritten only when the initializer structurally *is* that
/// comparison, a function-scoped slot being re-bindable.
fn constify_check_temp(
    engine: &mut Engine,
    binds: &Binds,
    cond: Operand,
    var: u32,
    fast_body: BlockId,
) {
    let negated = negated_operand(engine, binds, cond);
    let Some(temp) = operand_local(engine.body, negated.unwrap_or(cond)) else {
        return;
    };
    let konst = negated.is_some();
    // Find the temp's comparison `let` inside the fast clone and overwrite it.
    let mut stack = vec![NodeRef::Block(fast_body)];
    while let Some(n) = stack.pop() {
        if let NodeRef::Stmt(s) = n
            && let StmtKind::Let {
                local_index, value, ..
            } = &engine.body.stmts[s].kind
            && *local_index == temp
            && is_pure_operand(engine.body, *value)
            && is_check_comparison(engine, *value, var)
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

/// Whether `value` is a bounds-check comparison over `var` — a relational
/// operator (`<`, `<=`, `>`, `>=`) with `var` as one operand — in either the
/// skeleton (`ExprKind::Binary`) or promoted-operand (`ValueKind::Binary`)
/// form. The eliminated check's condition is exactly such a comparison, so this
/// confirms the `let` being constified is that comparison and not an unrelated
/// binding reusing the temp's local slot.
fn is_check_comparison(engine: &Engine, value: Operand, var: u32) -> bool {
    let Some((left, op, right)) = binary_parts(engine.body, value) else {
        return false;
    };
    let reads_var = |op| operand_local(engine.body, op) == Some(var);
    matches!(
        op,
        NirBinaryOp::Lt | NirBinaryOp::LtEq | NirBinaryOp::Gt | NirBinaryOp::GtEq
    ) && (reads_var(left) || reads_var(right))
}

/// Whether the subtree under `block` re-binds any of `locals` via `let`, or
/// contains any `LetDestructure` (whose pattern could bind one). A per-
/// iteration re-binding would invalidate the loop-invariance the residual
/// relies on; `node_modifies` only tracks `Assign` / `&mut`, so `let`
/// re-definitions need this separate scan. (Fresh NIR mints a new local per
/// shadowing `let`, so this is a defensive guard against synthesized IR.)
fn subtree_redefines(engine: &Engine, block: BlockId, locals: &[u32]) -> bool {
    engine
        .body
        .find_in_live_node_under(NodeRef::Block(block), |n| {
            let NodeRef::Stmt(s) = n else { return None };
            match &engine.body.stmts[s].kind {
                StmtKind::Let { local_index, .. } => locals.contains(local_index).then_some(()),
                StmtKind::LetDestructure { .. } => Some(()),
                StmtKind::Expr(_)
                | StmtKind::Return { .. }
                | StmtKind::If { .. }
                | StmtKind::Loop { .. }
                | StmtKind::Break { .. }
                | StmtKind::Continue
                | StmtKind::LabeledBlock { .. } => None,
            }
        })
        .is_some()
}

/// Collapse a cleaned fast arm — `loop { if !(i CMP H) break; [local writes];
/// array_set(A, i, CONST); i += 1 }` — into one `array_fill` followed by those
/// writes replayed once at the last iterated `i`. The count is `H + 1 - i` for
/// `<=` (the residual proving no overflow) and `H - i` for `<`; the wrapping
/// `if` preserves the zero-iteration case exactly.
fn try_fill_idiom(
    engine: &mut Engine,
    binds: &Binds,
    descriptors: &[FunctionRef],
    fill_id: FuncId,
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
    // Middle statements are replayed verbatim at the last iterated `i`, so a
    // read of a local the sequence writes at or after it would see the wrong
    // iteration's value.
    let middle: Vec<StmtId> = stmts[1..n - 2].to_vec();
    let reserved = [var, arr_local, h];
    let mut effects: Vec<Effects> = Vec::new();
    for &s in &middle {
        let Some(e) = replayable_effects(engine, s, &reserved) else {
            return false;
        };
        effects.push(e);
    }
    for (i, e) in effects.iter().enumerate() {
        let carried = |r: &u32| effects[i..].iter().any(|w| w.writes.contains(r));
        if e.reads.iter().any(carried) {
            return false;
        }
    }
    // A branch-guarded write is not the last iteration's to make, so it
    // survives only where nothing can tell which iteration made it last: the
    // slow arm alone names the local, and the version `if` runs once.
    let mut conditional = effects.iter().flat_map(|e| &e.conditional).peekable();
    if conditional.peek().is_some()
        && (block_repeats(engine.body, plan.parent)
            || conditional.any(|&l| !confined_to(engine, l, arm.version_if)))
    {
        return false;
    }
    let span = engine.body.stmts[loop_stmt].span;
    let ty = engine.locals()[var as usize].type_id;
    let one = engine.body.values.int_typed(1, ty);

    // `H + 1` (for `<=`) / `H` (for `<`) — the exclusive upper end.
    let upper = |engine: &mut Engine| -> Operand {
        let h_read = local_read(engine, h, span);
        if guard_le {
            alloc_binary(
                engine,
                h_read,
                NirBinaryOp::Add,
                Operand::Value(one),
                ty,
                span,
            )
        } else {
            h_read
        }
    };

    let upper_for_len = upper(engine);
    let i_read = local_read(engine, var, span);
    let count = alloc_binary(engine, upper_for_len, NirBinaryOp::Sub, i_read, ty, span);
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
                    expr: count,
                    is_mut: false,
                },
            ],
            has_receiver: false,
        },
        TypeTable::UNIT,
        span,
    );
    let fill_stmt = engine.alloc_stmt(StmtKind::Expr(Operand::Expr(fill_call)), span);

    let mut body_stmts = vec![fill_stmt];
    // The fill consumed every iteration's store; what the middle statements
    // leave behind is the last iteration's, so set `i` to the value that
    // iteration saw (`H` for `<=`, `H - 1` for `<`) and run them once.
    if !middle.is_empty() {
        let last_iterated = if guard_le {
            local_read(engine, h, span)
        } else {
            let h_read = local_read(engine, h, span);
            alloc_binary(
                engine,
                h_read,
                NirBinaryOp::Sub,
                Operand::Value(one),
                ty,
                span,
            )
        };
        body_stmts.push(alloc_local_set(engine, var, last_iterated, span));
        body_stmts.extend(middle);
    }
    // `i`'s final value: `H + 1` for `<=`, `H` for `<`.
    let i_final = upper(engine);
    body_stmts.push(alloc_local_set(engine, var, i_final, span));

    // `if i CMP H { ... }` preserves the zero-iteration case.
    let i_read2 = local_read(engine, var, span);
    let h_read2 = local_read(engine, h, span);
    let op = if guard_le {
        NirBinaryOp::LtEq
    } else {
        NirBinaryOp::Lt
    };
    let enter = alloc_binary(engine, i_read2, op, h_read2, TypeTable::BOOL, span);
    let body_block = engine.alloc_block(body_stmts, span);
    let fill_if = engine.alloc_stmt(
        StmtKind::If {
            condition: enter,
            then_block: body_block,
            else_block: None,
        },
        span,
    );
    engine.set_block_stmts(arm.then_block, vec![fill_if]);
    true
}

/// The locals one middle statement reads and writes.
struct Effects {
    reads: Vec<u32>,
    writes: Vec<u32>,
    /// The writes a branch guards, which the last iteration need not have made.
    conditional: Vec<u32>,
}

/// What `stmt` reads and writes, or `None` when running it once cannot stand
/// for the loop's last iteration. A trap is the effect of the iteration that
/// raises it, so a statement that may trap is not replayable either.
fn replayable_effects(engine: &Engine, stmt: StmtId, reserved: &[u32]) -> Option<Effects> {
    let body = &engine.body;
    let types = engine.value_graph_type_table();
    let mut eff = Effects {
        reads: Vec::new(),
        writes: Vec::new(),
        conditional: Vec::new(),
    };
    // A block that binds a local and yields it reads that local only to yield
    // it, so the read is its own write and not a value carried from before.
    let mut captured: Vec<u32> = Vec::new();
    let mut roots = vec![NodeRef::Stmt(stmt)];
    while let Some(root) = roots.pop() {
        let rejected = body.walk_nodes_under::<()>(root, |n| {
            if !pooled_operands_are_literal(body, n) {
                return ControlFlow::Break(());
            }
            match n {
                NodeRef::Block(_) => {}
                // A pattern binds locals of its own, which no walk over
                // expressions accounts for.
                NodeRef::Pat(_) => return ControlFlow::Break(()),
                NodeRef::Stmt(s) => match &body.stmts[s].kind {
                    StmtKind::Let { local_index, .. } => eff.writes.push(*local_index),
                    StmtKind::If { .. } => collect_writes(body, n, &mut eff.conditional),
                    StmtKind::LabeledBlock { label, .. } if has_break_to(body, n, label) => {
                        collect_writes(body, n, &mut eff.conditional);
                    }
                    StmtKind::Expr(_) | StmtKind::LabeledBlock { .. } => {}
                    _ => return ControlFlow::Break(()),
                },
                NodeRef::Expr(e) => match &body.exprs[e].kind {
                    ExprKind::Local { index, .. } => eff.reads.push(*index),
                    ExprKind::Unary {
                        op: NirUnaryOp::MutRef,
                        expr,
                    } => {
                        let Some(ExprKind::Local { index, .. }) =
                            expr.as_expr().map(|ie| &body.exprs[ie].kind)
                        else {
                            return ControlFlow::Break(());
                        };
                        eff.writes.push(*index);
                        return ControlFlow::Continue(false);
                    }
                    ExprKind::Assign { target, value } => {
                        let ExprKind::Local { index, .. } = &body.exprs[*target].kind else {
                            return ControlFlow::Break(());
                        };
                        eff.writes.push(*index);
                        if let Some(ve) = value.as_expr() {
                            roots.push(NodeRef::Expr(ve));
                        }
                        return ControlFlow::Continue(false);
                    }
                    ExprKind::LabeledBlock { label, .. } => {
                        if let Some((l, _)) = capture_block_binding(engine, Operand::Expr(e))
                            && local_read_count(body, NodeRef::Stmt(stmt), l) == 1
                        {
                            captured.push(l);
                        }
                        // A block nothing breaks out of runs to its end, so its
                        // writes are as unconditional as the block is.
                        if has_break_to(body, n, label) {
                            collect_writes(body, n, &mut eff.conditional);
                        }
                    }
                    ExprKind::If { .. } | ExprKind::Match { .. } | ExprKind::Switch { .. } => {
                        collect_writes(body, n, &mut eff.conditional);
                    }
                    // `&&` / `||` short-circuit, so the right side runs only on
                    // the iterations the left side let through.
                    ExprKind::Binary {
                        op: NirBinaryOp::And | NirBinaryOp::Or,
                        right,
                        ..
                    } => {
                        if let Some(re) = right.as_expr() {
                            collect_writes(body, NodeRef::Expr(re), &mut eff.conditional);
                        }
                    }
                    ExprKind::Dead
                    | ExprKind::GlobalVarSet { .. }
                    | ExprKind::Call { .. }
                    | ExprKind::CmRawCall { .. }
                    | ExprKind::IndirectCall { .. }
                    | ExprKind::ClosureToCanonical { .. } => return ControlFlow::Break(()),
                    _ if expr_node_may_trap_typed(body, e, types) => return ControlFlow::Break(()),
                    _ => {}
                },
            }
            ControlFlow::Continue(true)
        });
        if rejected.is_some() {
            return None;
        }
    }
    if eff.writes.iter().any(|w| reserved.contains(w)) {
        return None;
    }
    eff.reads.retain(|r| !captured.contains(r));
    Some(eff)
}

/// Whether every pooled operand of `node` is a literal. Anything else is a
/// promoted expression, which hides reads and writes from a skeleton walk.
fn pooled_operands_are_literal(body: &Body, node: NodeRef) -> bool {
    let mut literal = true;
    body.for_each_operand(node, |op| {
        if let Operand::Value(v) = op {
            literal &= matches!(
                body.values.kind(v),
                ValueKind::Int(..) | ValueKind::Float(..) | ValueKind::Bool(_) | ValueKind::Char(_)
            );
        }
    });
    literal
}

/// Every local written anywhere under `node`.
fn collect_writes(body: &Body, node: NodeRef, out: &mut Vec<u32>) {
    body.for_each_node_under(node, |n| {
        if let Some(l) = local_written_by(body, n) {
            out.push(l);
        }
        if let NodeRef::Stmt(s) = n
            && let StmtKind::Let { local_index, .. } = &body.stmts[s].kind
        {
            out.push(*local_index);
        }
    });
}

/// Whether local `l` is named nowhere in the body but under `version_if`. The
/// slow arm is then its only other reader, and an arm the fast one replaced
/// does not run.
fn confined_to(engine: &Engine, l: u32, version_if: StmtId) -> bool {
    let body = &engine.body;
    let root = NodeRef::Block(body.root);
    !mentions_local_except(body, root, Some(NodeRef::Stmt(version_if)), l)
}

/// Whether `block` sits inside a loop, and so may run more than once.
fn block_repeats(body: &Body, block: BlockId) -> bool {
    body.find_in_nodes_under(NodeRef::Block(body.root), |n| {
        let NodeRef::Stmt(s) = n else { return None };
        let StmtKind::Loop { body: inner } = &body.stmts[s].kind else {
            return None;
        };
        block_under(body, *inner, block).then_some(())
    })
    .is_some()
}

/// Whether `block` is `root` or nested under it.
fn block_under(body: &Body, root: BlockId, block: BlockId) -> bool {
    root == block
        || body
            .find_in_nodes_under(NodeRef::Block(root), |n| {
                (n == NodeRef::Block(block)).then_some(())
            })
            .is_some()
}

/// How many times `root` reads local `l`. An assignment's target names a place
/// rather than reading one, so it does not count.
fn local_read_count(body: &Body, root: NodeRef, l: u32) -> usize {
    let mut count = 0;
    body.walk_nodes_under::<()>(root, |n| {
        if let NodeRef::Expr(e) = n {
            match &body.exprs[e].kind {
                ExprKind::Local { index, .. } if *index == l => count += 1,
                ExprKind::Assign { value, .. } => {
                    if let Some(ve) = value.as_expr() {
                        count += local_read_count(body, NodeRef::Expr(ve), l);
                    }
                    return ControlFlow::Continue(false);
                }
                _ => {}
            }
        }
        ControlFlow::Continue(true)
    });
    count
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
