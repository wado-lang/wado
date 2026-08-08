//! Multi-value return ABI classification (NIR-level).
//!
//! Decides which aggregate-returning user functions should use the
//! multi-value Wasm return ABI (each tuple element / struct field a separate
//! Wasm result) instead of the default heap-struct ABI. The decision is
//! recorded on [`NirFunction.return_abi`] and consumed by `wir_build`.
//!
//! ## Eligibility
//!
//! A function `f` is a candidate when its return type is a tuple or user
//! struct of 2..=[`MAX_RESULTS`] fields (all field types eligible), every
//! `Return` produces a fresh aggregate literal of that shape, and every call
//! site binds it as `let __tmp = Call(f); …` — directly, or at the tail of a
//! block that hoists the receiver first — whose only uses of `__tmp` are
//! `FieldAccess(Local(__tmp), name)`. See the per-function comments below for
//! the exact gates.
//!
//! Ported off the `Body ↔ tree` bridge (Phase 4 stage C; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`): a pure classification
//! pass — every body walk is read-only and the only mutation sets
//! `return_abi` — so it reads the arena `Body` directly.

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{FuncId, FunctionKind, NirFunction, NirStruct, ReturnAbi};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef, Operand, StmtId, StmtKind};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};

/// Widest result vector the ABI is applied to. Matches
/// `wir_optimize::sroa_variant_return::layout::MAX_PER_CASE_RESULT_FIELDS`, so
/// a variant scalarized into `[tag, per-case slots…]` is never rejected here by
/// an arity bound the layout that produced it does not share.
const MAX_RESULTS: usize = 8;

/// Per-candidate body-only info: the per-field NIR types and field names.
#[derive(Clone, Debug)]
struct CandidateInfo {
    result_types: Vec<TypeId>,
    field_names: Vec<String>,
    field_name_set: IndexSet<String>,
}

/// A direct call in tail position: its callee, the call's own type, and the
/// node. Looks through a block tail, the shape `let_block_flatten` leaves when
/// it hoists a receiver in front of the call — and hands back the statements
/// the block runs first.
///
/// Those statements are ordinary code and the caller has to deal with them: a
/// `return` validates them, `wir_build` emits them ahead of the bind. Skipping
/// them let a whole-aggregate use in the prefix pass unseen, so its callee took
/// the multi-value ABI while the use still read a local the split had left
/// unassigned.
///
/// `wir_build::translate::try_emit_multi_value_let` shares this recogniser, so
/// the shape this pass accepts at a `let` is by construction the shape the
/// lowering can split.
pub(crate) fn block_tail_call(
    body: &Body,
    op: Operand,
    prefix: &mut Vec<StmtId>,
) -> Option<(FuncId, TypeId, ExprId)> {
    let e = op.as_expr()?;
    match &body.exprs[e].kind {
        ExprKind::Call { func_id, .. } => Some((*func_id, body.exprs[e].type_id, e)),
        ExprKind::Block(b) => {
            let (&last, lead) = body.blocks[*b].stmts.split_last()?;
            match &body.stmts[last].kind {
                StmtKind::Expr(inner) => {
                    prefix.extend_from_slice(lead);
                    block_tail_call(body, *inner, prefix)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Walk the argument expressions of a (Method)Call so tracked-local escapes in
/// the args invalidate the right candidate (the call's own ABI is accepted).
fn walk_call_args_for_uses(
    body: &Body,
    expr: ExprId,
    cx: &UseCx<'_>,
    invalid: &mut IndexSet<usize>,
    tracked: &IndexMap<u32, usize>,
) {
    let args: Vec<ExprId> = match &body.exprs[expr].kind {
        ExprKind::Call { args, .. } => args.iter().filter_map(|a| a.expr.as_expr()).collect(),
        _ => return,
    };
    for a in args {
        walk_expr_for_uses(body, a, cx, invalid, tracked);
    }
}

/// Classify aggregate-returning functions and set `return_abi` on those whose
/// every return statement and call site permit the multi-value ABI.
pub fn classify_multi_value_returns(project: &mut NirPackage) -> bool {
    let type_table = project.type_table.borrow();
    let structs = &project.structs;

    // The tail-call rule couples caller and callee candidacy, so the candidate
    // set is refuted to a fix-point: optimistic (assume every aggregate-returning
    // function takes the ABI, then withdraw), so a mutually tail-recursive group
    // survives. Each round strictly shrinks `tail_ok`, so it terminates.
    let mut tail_ok: IndexMap<FuncId, TypeId> = project
        .functions
        .iter()
        .filter_map(|f| {
            let f = f.borrow();
            Some((f.id?, f.return_type))
        })
        .collect();

    let candidates = loop {
        // Phase 1: candidate identification — body-only checks.
        let mut candidates: IndexMap<usize, CandidateInfo> = IndexMap::default();
        for (idx, func_rc) in project.functions.iter().enumerate() {
            let func = func_rc.borrow();
            if let Some(info) = candidate_info(&func, &type_table, structs, &tail_ok) {
                candidates.insert(idx, info);
            }
        }
        if candidates.is_empty() {
            return false;
        }

        // Phase 2: call-site validation across every function body.
        let candidate_ids: IndexMap<FuncId, usize> = candidates
            .keys()
            .filter_map(|&idx| {
                let func = project.functions[idx].borrow();
                func.id.map(|id| (id, idx))
            })
            .collect();

        let mut invalid: IndexSet<usize> = IndexSet::default();
        for func_rc in &project.functions {
            let func = func_rc.borrow();
            let Some(body) = &func.body else {
                continue;
            };
            // A `return g(x)` is only a pass-through when this function has the
            // same N results to pass it through; otherwise the call has no
            // aggregate to bind and stays an escape.
            //
            // Read from `tail_ok`, never from this round's candidate set: a
            // candidate can still be invalidated *later in this same round*, and
            // sparing `g` on behalf of a caller that then does not take the ABI
            // leaves the caller pushing N results through a one-ref signature.
            // At the fixed point `tail_ok` is exactly the surviving set, so the
            // two sides of the rule agree.
            let passes_through = func.id.and_then(|id| tail_ok.get(&id)).copied();
            validate_uses_in_block(
                body,
                body.root,
                &candidate_ids,
                &candidates,
                passes_through,
                &mut invalid,
            );
        }
        // Global initializers are arena bodies too; scan them so a candidate consumed
        // there is validated with the same let-tracking logic (matching dae / drve's
        // coverage). Benign today — lower hoisting keeps aggregate builders out of
        // initializers — but the coverage is now uniform across every body.
        for global in &project.globals {
            let body = global.init.slot_expr().body();
            validate_uses_in_block(
                body,
                body.root,
                &candidate_ids,
                &candidates,
                None,
                &mut invalid,
            );
        }

        // Intersected with the assumed set, so the chain strictly decreases and
        // the loop terminates. Both halves of the tail-call rule read `tail_ok`,
        // so applying exactly it keeps caller and callee decisions in step: a
        // survivor outside it could hold a `return g(x)` whose `g` this round
        // withdrew.
        let survivors: IndexMap<FuncId, TypeId> = candidates
            .keys()
            .filter(|&&idx| !invalid.contains(&idx))
            .filter_map(|&idx| {
                let func = project.functions[idx].borrow();
                let id = func.id?;
                tail_ok.contains_key(&id).then_some((id, func.return_type))
            })
            .collect();
        if survivors.len() == tail_ok.len() {
            break candidates
                .into_iter()
                .filter(|(idx, _)| {
                    !invalid.contains(idx)
                        && project.functions[*idx]
                            .borrow()
                            .id
                            .is_some_and(|id| tail_ok.contains_key(&id))
                })
                .collect::<Vec<_>>();
        }
        tail_ok = survivors;
    };

    // Phase 3: apply.
    drop(type_table);
    let mut changed = false;
    for (idx, info) in candidates {
        let mut func = project.functions[idx].borrow_mut();
        func.return_abi = ReturnAbi::MultiValue {
            result_types: info.result_types,
            field_names: info.field_names,
        };
        changed = true;
    }
    changed
}

/// Body-only candidate check.
fn candidate_info(
    func: &NirFunction,
    type_table: &TypeTable,
    structs: &[NirStruct],
    tail_ok: &IndexMap<FuncId, TypeId>,
) -> Option<CandidateInfo> {
    if !matches!(func.kind, FunctionKind::Regular) || func.is_dispatch_wrapper {
        return None;
    }
    if func.is_export || func.is_cm_export || func.is_cm_binding {
        return None;
    }
    if func.is_async {
        return None;
    }
    if func.has_real_type_params() || !func.impl_type_params.is_empty() {
        return None;
    }
    // A trait method is not excluded: after monomorphization it is an ordinary
    // direct-call target, and the gates above already cover every way a
    // function's address escapes a direct call.
    if func.is_closure_call() {
        return None;
    }

    let body = func.body.as_ref()?;
    let return_type = func.return_type;

    let (result_types, field_names, is_struct_shape) =
        aggregate_field_info(return_type, type_table, structs)?;
    if !(2..=MAX_RESULTS).contains(&result_types.len()) {
        return None;
    }
    for &t in &result_types {
        if !is_eligible_field_type(t, type_table) {
            return None;
        }
    }

    let expected = ExpectedShape {
        arity: result_types.len(),
        struct_type: if is_struct_shape {
            Some(return_type)
        } else {
            None
        },
        return_type,
        tail_ok,
        tail_call_lowerable: true,
    };
    if !all_returns_match_shape(body, body.root, &expected) {
        return None;
    }

    let field_name_set: IndexSet<String> = field_names.iter().cloned().collect();
    Some(CandidateInfo {
        result_types,
        field_names,
        field_name_set,
    })
}

pub(super) fn aggregate_field_info(
    return_type: TypeId,
    type_table: &TypeTable,
    structs: &[NirStruct],
) -> Option<(Vec<TypeId>, Vec<String>, bool)> {
    if let Some(elems) = type_table.as_tuple(return_type) {
        let names: Vec<String> = (0..elems.len()).map(|i| i.to_string()).collect();
        return Some((elems, names, false));
    }
    let resolved = type_table.get(return_type);
    if let ResolvedType::Struct {
        decl_name,
        module_source,
        type_args,
    } = resolved
    {
        let name = type_table.struct_rendered_name(decl_name, type_args);
        let s = structs
            .iter()
            .find(|s| s.name == name && s.module_source == *module_source)?;
        if !s.type_params.is_empty() {
            return None;
        }
        let result_types: Vec<TypeId> = s.fields.iter().map(|f| f.type_id).collect();
        let field_names: Vec<String> = s.fields.iter().map(|f| f.name.clone()).collect();
        return Some((result_types, field_names, true));
    }
    None
}

fn is_eligible_field_type(type_id: TypeId, type_table: &TypeTable) -> bool {
    match type_table.get(type_id) {
        ResolvedType::Primitive(_)
        | ResolvedType::Struct { .. }
        | ResolvedType::Enum { .. }
        | ResolvedType::Resource { .. }
        | ResolvedType::Variant { .. }
        | ResolvedType::GenericInstance { .. }
        | ResolvedType::GenericResource { .. }
        | ResolvedType::Newtype { .. }
        | ResolvedType::Flags { .. }
        | ResolvedType::BuiltinArray(_)
        | ResolvedType::Ref(_)
        | ResolvedType::MutRef(_)
        | ResolvedType::Reactive(_) => true,
        ResolvedType::Unit
        | ResolvedType::Never
        | ResolvedType::Function { .. }
        | ResolvedType::TypeParam { .. }
        | ResolvedType::TypePack { .. }
        | ResolvedType::AssocTypeProjection { .. }
        | ResolvedType::Unknown
        | ResolvedType::Error => false,
    }
}

#[derive(Clone, Copy)]
struct ExpectedShape<'a> {
    arity: usize,
    struct_type: Option<TypeId>,
    /// The function's own return type, so a tail call can check that its callee
    /// returns the same aggregate.
    return_type: TypeId,
    /// Callees already taking this ABI, by return type. A `return g(x)` whose
    /// `g` is one of them leaves its N results on the stack for us, so the
    /// caller never builds the aggregate either.
    tail_ok: &'a IndexMap<FuncId, TypeId>,
    /// Whether a tail call is lowerable *in this position*. `wir_build` turns a
    /// labeled-block exit into a `Return` only for a `StructNew`
    /// (`rewrite_struct_new_br_to_return`), so `break L: g(x)` would clear the
    /// block's result type and then leave `g`'s N results stranded on the stack.
    /// Accepting the shape somewhere the lowering cannot follow is the one way
    /// this pass can produce invalid Wasm, so break positions clear it.
    tail_call_lowerable: bool,
}

impl ExpectedShape<'_> {
    /// The same shape as seen from a `break` value, where a tail call is not
    /// lowerable.
    fn in_break_position(&self) -> Self {
        Self {
            tail_call_lowerable: false,
            ..*self
        }
    }
}

// -----------------------------------------------------------------------
// Return-shape validation
// -----------------------------------------------------------------------

/// Collect every `Stmt` node in the subtree rooted at `node`.
fn collect_stmts(body: &Body, node: NodeRef, out: &mut Vec<StmtId>) {
    if let NodeRef::Stmt(s) = node {
        out.push(s);
    }
    body.for_each_child(node, |c| collect_stmts(body, c, out));
}

fn all_returns_match_shape(body: &Body, block: BlockId, expected: &ExpectedShape<'_>) -> bool {
    body.blocks[block]
        .stmts
        .iter()
        .all(|&stmt| stmt_returns_match(body, stmt, expected))
}

fn stmt_returns_match(body: &Body, stmt: StmtId, expected: &ExpectedShape<'_>) -> bool {
    match &body.stmts[stmt].kind {
        StmtKind::Return { value: None } => false,
        StmtKind::Return { value: Some(v) } => expr_returns_match_operand(body, *v, expected),
        // The condition too — a `?` in it desugars to a `return`, and the shape
        // check has to see it like any other. Same gap as
        // `sroa_variant_return::stmt_returns_scalarizable` had.
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            nested_returns_in_expr_match_operand(body, *condition, expected)
                && all_returns_match_shape(body, *then_block, expected)
                && else_block.is_none_or(|b| all_returns_match_shape(body, b, expected))
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            all_returns_match_shape(body, *b, expected)
        }
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
            nested_returns_in_expr_match_operand(body, *value, expected)
        }
        StmtKind::Expr(e) => nested_returns_in_expr_match_operand(body, *e, expected),
        StmtKind::Break { value, .. } => {
            value.is_none_or(|v| nested_returns_in_expr_match_operand(body, v, expected))
        }
        StmtKind::Continue => true,
    }
}

fn expr_returns_match_operand(body: &Body, op: Operand, expected: &ExpectedShape<'_>) -> bool {
    op.as_expr()
        .is_some_and(|e| expr_returns_match(body, e, expected))
}
fn nested_returns_in_expr_match_operand(
    body: &Body,
    op: Operand,
    expected: &ExpectedShape<'_>,
) -> bool {
    op.as_expr()
        .is_none_or(|e| nested_returns_in_expr_match(body, e, expected))
}
fn expr_break_values_match_operand(body: &Body, op: Operand, expected: &ExpectedShape<'_>) -> bool {
    op.as_expr()
        .is_none_or(|e| expr_break_values_match(body, e, expected))
}

fn nested_returns_in_expr_match(body: &Body, expr: ExprId, expected: &ExpectedShape<'_>) -> bool {
    let mut stmts = Vec::new();
    collect_stmts(body, NodeRef::Expr(expr), &mut stmts);
    stmts.iter().all(|&s| stmt_returns_match(body, s, expected))
}

fn expr_returns_match(body: &Body, expr: ExprId, expected: &ExpectedShape<'_>) -> bool {
    if body.exprs[expr].type_id == TypeTable::NEVER {
        return true;
    }
    match &body.exprs[expr].kind {
        ExprKind::TupleLiteral { elements } => {
            expected.struct_type.is_none() && elements.len() == expected.arity
        }
        // `return g(x)` where `g` returns this same aggregate under this same
        // ABI: its results are already on the stack in our result order.
        ExprKind::Call { func_id, .. } => {
            expected.tail_call_lowerable
                && expected.tail_ok.get(func_id) == Some(&expected.return_type)
        }
        ExprKind::StructLiteral {
            struct_type,
            fields,
            ..
        } => match expected.struct_type {
            Some(t) => *struct_type == t && fields.len() == expected.arity,
            None => false,
        },
        ExprKind::Block(b) => {
            let b = *b;
            all_returns_match_shape(body, b, expected)
                && block_tail_returns_match(body, b, expected)
        }
        ExprKind::LabeledBlock { block: b, .. } => {
            let b = *b;
            all_returns_match_shape(body, b, expected)
                && block_tail_returns_match(body, b, expected)
                && all_break_values_match_shape(body, b, expected)
        }
        ExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            let (then_branch, else_branch) = (*then_branch, *else_branch);
            all_returns_match_shape(body, then_branch, expected)
                && block_tail_returns_match(body, then_branch, expected)
                && else_branch.is_none_or(|b| {
                    all_returns_match_shape(body, b, expected)
                        && block_tail_returns_match(body, b, expected)
                })
        }
        ExprKind::Match { arms, .. } => arms.iter().all(|a| {
            // A promoted-value arm body (e.g. a `String` literal) is a leaf, not
            // a nested match-return, so it never satisfies the shape.
            a.body
                .as_expr()
                .is_some_and(|b| expr_returns_match(body, b, expected))
        }),
        ExprKind::Switch { arms, default, .. } => {
            let arms = arms.clone();
            let default = *default;
            arms.iter().all(|&arm| {
                all_returns_match_shape(body, arm, expected)
                    && block_tail_returns_match(body, arm, expected)
            }) && all_returns_match_shape(body, default, expected)
                && block_tail_returns_match(body, default, expected)
        }
        _ => false,
    }
}

fn all_break_values_match_shape(body: &Body, block: BlockId, expected: &ExpectedShape<'_>) -> bool {
    body.blocks[block]
        .stmts
        .iter()
        .all(|&stmt| stmt_break_values_match(body, stmt, expected))
}

fn stmt_break_values_match(body: &Body, stmt: StmtId, expected: &ExpectedShape<'_>) -> bool {
    match &body.stmts[stmt].kind {
        StmtKind::Break { value: Some(v), .. } => {
            let v = *v;
            let expected = &expected.in_break_position();
            expr_returns_match_operand(body, v, expected)
                && expr_break_values_match_operand(body, v, expected)
        }
        StmtKind::Break { value: None, .. } | StmtKind::Continue => true,
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let (condition, then_block, else_block) = (*condition, *then_block, *else_block);
            expr_break_values_match_operand(body, condition, expected)
                && all_break_values_match_shape(body, then_block, expected)
                && else_block.is_none_or(|b| all_break_values_match_shape(body, b, expected))
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            all_break_values_match_shape(body, *b, expected)
        }
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
            expr_break_values_match_operand(body, *value, expected)
        }
        StmtKind::Expr(e) => expr_break_values_match_operand(body, *e, expected),
        StmtKind::Return { value: Some(e) } => expr_break_values_match_operand(body, *e, expected),
        StmtKind::Return { value: None } => true,
    }
}

fn expr_break_values_match(body: &Body, expr: ExprId, expected: &ExpectedShape<'_>) -> bool {
    let mut stmts = Vec::new();
    collect_stmts(body, NodeRef::Expr(expr), &mut stmts);
    stmts
        .iter()
        .all(|&s| stmt_break_values_match(body, s, expected))
}

fn block_tail_returns_match(body: &Body, block: BlockId, expected: &ExpectedShape<'_>) -> bool {
    let Some(&last) = body.blocks[block].stmts.last() else {
        return false;
    };
    match &body.stmts[last].kind {
        StmtKind::Expr(e) => expr_returns_match_operand(body, *e, expected),
        StmtKind::Break { value: Some(v), .. } => {
            expr_returns_match_operand(body, *v, &expected.in_break_position())
        }
        StmtKind::Return { .. } => true,
        _ => false,
    }
}

// -----------------------------------------------------------------------
// Call-site validation
// -----------------------------------------------------------------------

fn validate_uses_in_block(
    body: &Body,
    block: BlockId,
    candidate_ids: &IndexMap<FuncId, usize>,
    candidates: &IndexMap<usize, CandidateInfo>,
    passes_through: Option<TypeId>,
    invalid: &mut IndexSet<usize>,
) {
    let mut tracked: IndexMap<u32, usize> = IndexMap::default();
    let settled = super::sroa_variant_return::settled_locals(body);
    let cx = UseCx {
        candidate_ids,
        candidates,
        passes_through,
        settled: &settled,
    };
    for &stmt in &body.blocks[block].stmts {
        validate_stmt(body, stmt, &cx, invalid, &mut tracked);
    }
}

/// Per-body context for the call-site walk.
#[derive(Clone, Copy)]
struct UseCx<'a> {
    candidate_ids: &'a IndexMap<FuncId, usize>,
    candidates: &'a IndexMap<usize, CandidateInfo>,
    /// This body's return type when it is itself a candidate — the one case in
    /// which a bare `Call` needs no `let` to bind it, because the results go
    /// straight out as our own.
    passes_through: Option<TypeId>,
    /// Locals bound once and never assigned, so a `let mut` over one of them
    /// binds a call result as safely as a plain `let`.
    settled: &'a IndexSet<u32>,
}

fn validate_stmt(
    body: &Body,
    stmt: StmtId,
    cx: &UseCx<'_>,
    invalid: &mut IndexSet<usize>,
    tracked: &mut IndexMap<u32, usize>,
) {
    match &body.stmts[stmt].kind {
        StmtKind::Let {
            local_index, value, ..
        } => {
            let (local_index, value) = (*local_index, *value);
            // The binding must be the local's sole definition, `mut` or not —
            // see `sroa_variant_return::settled_locals`, whose rule this shares
            // because that pass hands this one its tuple-returning functions.
            //
            // The call may sit at a block tail with its receiver hoisted in
            // front; those statements are ordinary code and are validated here.
            // `wir_build::try_emit_multi_value_let` splits the same shape off
            // the same recogniser, so neither side can accept what the other
            // refuses.
            let mut prefix: Vec<StmtId> = Vec::new();
            if cx.settled.contains(&local_index)
                && let Some((func_id, _, call)) = block_tail_call(body, value, &mut prefix)
                && let Some(&candidate_idx) = cx.candidate_ids.get(&func_id)
            {
                for s in prefix {
                    validate_stmt(body, s, cx, invalid, tracked);
                }
                tracked.insert(local_index, candidate_idx);
                walk_call_args_for_uses(body, call, cx, invalid, tracked);
                return;
            }
            walk_expr_for_uses_operand(body, value, cx, invalid, tracked);
        }
        StmtKind::Expr(e) => {
            walk_expr_for_uses_operand(body, *e, cx, invalid, tracked);
        }
        StmtKind::Return { value: Some(e) } => {
            let e = *e;
            // `return g(x)` in a function that returns the same aggregate: `g`
            // leaves its N results on the stack and they are ours, so the call
            // needs no `let` to bind.
            if let Some(ours) = cx.passes_through {
                validate_tail_return(body, e, ours, cx, invalid, tracked);
                return;
            }
            walk_expr_for_uses_operand(body, e, cx, invalid, tracked);
        }
        StmtKind::Return { value: None } | StmtKind::Continue => {}
        StmtKind::Break { value, .. } => {
            if let Some(v) = value {
                walk_expr_for_uses_operand(body, *v, cx, invalid, tracked);
            }
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let (condition, then_block, else_block) = (*condition, *then_block, *else_block);
            walk_expr_for_uses_operand(body, condition, cx, invalid, tracked);
            let mut inner = tracked.clone();
            for &s in &body.blocks[then_block].stmts.clone() {
                validate_stmt(body, s, cx, invalid, &mut inner);
            }
            if let Some(eb) = else_block {
                let mut inner = tracked.clone();
                for &s in &body.blocks[eb].stmts.clone() {
                    validate_stmt(body, s, cx, invalid, &mut inner);
                }
            }
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            let b = *b;
            let mut inner = tracked.clone();
            for &s in &body.blocks[b].stmts.clone() {
                validate_stmt(body, s, cx, invalid, &mut inner);
            }
        }
        StmtKind::LetDestructure { value, .. } => {
            walk_expr_for_uses_operand(body, *value, cx, invalid, tracked);
        }
    }
}

/// Walk a `return`ed value, sparing a pass-through call in every tail position
/// [`expr_returns_match`] accepts it — a block or labeled-block tail, an `If`
/// branch tail, a `Match` arm body, a `Switch` arm tail — and recursively,
/// since those nest. Everything the value reaches off that spine is an ordinary
/// use.
///
/// The two halves have to agree: a position the return side admits but this one
/// walks as an escape invalidates the callee, and the fix-point then withdraws
/// the caller as well, so both keep the heap-tuple ABI. Break values are the
/// deliberate exception — see [`ExpectedShape::tail_call_lowerable`], which
/// refuses them on the return side too.
fn validate_tail_return(
    body: &Body,
    op: Operand,
    ours: TypeId,
    cx: &UseCx<'_>,
    invalid: &mut IndexSet<usize>,
    tracked: &mut IndexMap<u32, usize>,
) {
    let Some(e) = op.as_expr() else {
        return;
    };
    match &body.exprs[e].kind {
        ExprKind::Call { func_id, .. }
            if body.exprs[e].type_id == ours && cx.candidate_ids.contains_key(func_id) =>
        {
            walk_call_args_for_uses(body, e, cx, invalid, tracked);
        }
        ExprKind::Block(b) | ExprKind::LabeledBlock { block: b, .. } => {
            validate_tail_block(body, *b, ours, cx, invalid, tracked);
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let (condition, then_branch, else_branch) = (*condition, *then_branch, *else_branch);
            walk_expr_for_uses_operand(body, condition, cx, invalid, tracked);
            for b in [Some(then_branch), else_branch].into_iter().flatten() {
                let mut inner = tracked.clone();
                validate_tail_block(body, b, ours, cx, invalid, &mut inner);
            }
        }
        ExprKind::Match { expr: scrut, arms } => {
            let (scrut, arms) = (*scrut, arms.clone());
            walk_expr_for_uses_operand(body, scrut, cx, invalid, tracked);
            for arm in arms {
                let mut inner = tracked.clone();
                if let Some(guard) = arm.guard {
                    walk_expr_for_uses_operand(body, guard, cx, invalid, &inner);
                }
                validate_tail_return(body, arm.body, ours, cx, invalid, &mut inner);
            }
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            let (scrutinee, arms, default) = (*scrutinee, arms.clone(), *default);
            walk_expr_for_uses_operand(body, scrutinee, cx, invalid, tracked);
            for b in arms.into_iter().chain([default]) {
                let mut inner = tracked.clone();
                validate_tail_block(body, b, ours, cx, invalid, &mut inner);
            }
        }
        _ => walk_expr_for_uses(body, e, cx, invalid, tracked),
    }
}

/// The tail position of `block`: everything ahead of the last statement is
/// ordinary code, and the last one continues the tail walk when it carries the
/// block's value.
fn validate_tail_block(
    body: &Body,
    block: BlockId,
    ours: TypeId,
    cx: &UseCx<'_>,
    invalid: &mut IndexSet<usize>,
    tracked: &mut IndexMap<u32, usize>,
) {
    let stmts = body.blocks[block].stmts.clone();
    let Some((&last, lead)) = stmts.split_last() else {
        return;
    };
    for &s in lead {
        validate_stmt(body, s, cx, invalid, tracked);
    }
    match &body.stmts[last].kind {
        StmtKind::Expr(v) | StmtKind::Return { value: Some(v) } => {
            let v = *v;
            validate_tail_return(body, v, ours, cx, invalid, tracked);
        }
        _ => validate_stmt(body, last, cx, invalid, tracked),
    }
}

fn walk_expr_for_uses_operand(
    body: &Body,
    op: Operand,
    cx: &UseCx<'_>,
    invalid: &mut IndexSet<usize>,
    tracked: &IndexMap<u32, usize>,
) {
    if let Some(e) = op.as_expr() {
        walk_expr_for_uses(body, e, cx, invalid, tracked);
    }
}

fn walk_expr_for_uses(
    body: &Body,
    expr: ExprId,
    cx: &UseCx<'_>,
    invalid: &mut IndexSet<usize>,
    tracked: &IndexMap<u32, usize>,
) {
    match &body.exprs[expr].kind {
        ExprKind::FieldAccess {
            expr: source,
            field_name,
            ..
        } => {
            let source = *source;
            // A promoted `Operand::Value` source falls through to the operand walk.
            if let Some(source_e) = source.as_expr()
                && let ExprKind::Local { index, .. } = &body.exprs[source_e].kind
                && let Some(&candidate_idx) = tracked.get(index)
                && let Some(info) = cx.candidates.get(&candidate_idx)
                && info.field_name_set.contains(field_name)
            {
                return;
            }
            walk_expr_for_uses_operand(body, source, cx, invalid, tracked);
        }
        ExprKind::Local { index, .. } => {
            if let Some(&candidate_idx) = tracked.get(index) {
                invalid.insert(candidate_idx);
            }
        }
        ExprKind::Call { func_id, args, .. } => {
            if let Some(&candidate_idx) = cx.candidate_ids.get(func_id) {
                invalid.insert(candidate_idx);
            }
            let args: Vec<ExprId> = args.iter().filter_map(|a| a.expr.as_expr()).collect();
            for a in args {
                walk_expr_for_uses(body, a, cx, invalid, tracked);
            }
        }
        ExprKind::CmRawCall { args, .. } => {
            let args = args.clone();
            for a in args {
                walk_expr_for_uses_operand(body, a, cx, invalid, tracked);
            }
        }
        ExprKind::IndirectCall { callee, args } => {
            let callee = *callee;
            let args = args.clone();
            walk_expr_for_uses_operand(body, callee, cx, invalid, tracked);
            for a in args {
                walk_expr_for_uses_operand(body, a, cx, invalid, tracked);
            }
        }
        ExprKind::Block(b) | ExprKind::LabeledBlock { block: b, .. } => {
            let b = *b;
            let mut inner = tracked.clone();
            for &s in &body.blocks[b].stmts.clone() {
                validate_stmt(body, s, cx, invalid, &mut inner);
            }
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let (condition, then_branch, else_branch) = (*condition, *then_branch, *else_branch);
            walk_expr_for_uses_operand(body, condition, cx, invalid, tracked);
            let mut inner = tracked.clone();
            for &s in &body.blocks[then_branch].stmts.clone() {
                validate_stmt(body, s, cx, invalid, &mut inner);
            }
            if let Some(eb) = else_branch {
                let mut inner = tracked.clone();
                for &s in &body.blocks[eb].stmts.clone() {
                    validate_stmt(body, s, cx, invalid, &mut inner);
                }
            }
        }
        ExprKind::Match { expr: scrut, arms } => {
            let scrut = *scrut;
            let arm_data: Vec<(Option<Operand>, Operand)> =
                arms.iter().map(|a| (a.guard, a.body)).collect();
            walk_expr_for_uses_operand(body, scrut, cx, invalid, tracked);
            for (guard, arm_body) in arm_data {
                if let Some(g) = guard {
                    walk_expr_for_uses_operand(body, g, cx, invalid, tracked);
                }
                walk_expr_for_uses_operand(body, arm_body, cx, invalid, tracked);
            }
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            let scrutinee = *scrutinee;
            let arms = arms.clone();
            let default = *default;
            walk_expr_for_uses_operand(body, scrutinee, cx, invalid, tracked);
            for arm in arms {
                let mut inner = tracked.clone();
                for &s in &body.blocks[arm].stmts.clone() {
                    validate_stmt(body, s, cx, invalid, &mut inner);
                }
            }
            let mut inner = tracked.clone();
            for &s in &body.blocks[default].stmts.clone() {
                validate_stmt(body, s, cx, invalid, &mut inner);
            }
        }
        _ => {
            // Pure value-producing expressions: walk every child so nested
            // `Local(tracked)` references are observed by the bare-Local arm.
            let mut kids = Vec::new();
            body.for_each_child(NodeRef::Expr(expr), |c| {
                if let NodeRef::Expr(e) = c {
                    kids.push(e);
                }
            });
            for e in kids {
                walk_expr_for_uses(body, e, cx, invalid, tracked);
            }
        }
    }
}
