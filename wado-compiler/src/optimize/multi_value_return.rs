//! Multi-value return ABI classification (NIR-level).
//!
//! Decides which aggregate-returning user functions should use the
//! multi-value Wasm return ABI (each tuple element / struct field a separate
//! Wasm result) instead of the default heap-struct ABI. The decision is
//! recorded on [`NirFunction.return_abi`] and consumed by `wir_build`.
//!
//! ## Eligibility
//!
//! A function `f` is a candidate when its return type is a 2..=4-arity tuple
//! or user struct (all field types eligible), every `Return` produces a fresh
//! aggregate literal of that shape, and every call site is `let __tmp =
//! Call(f); …` whose only uses of `__tmp` are `FieldAccess(Local(__tmp),
//! name)`. See the per-function comments below for the exact gates.
//!
//! Ported off the `Body ↔ tree` bridge (Phase 4 stage C; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`): a pure classification
//! pass — every body walk is read-only and the only mutation sets
//! `return_abi` — so it reads the arena `Body` directly.

use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::{FunctionKind, NirFunction, NirStruct, ReturnAbi};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef, StmtId, StmtKind};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};

/// Per-candidate body-only info: the per-field NIR types and field names.
#[derive(Clone, Debug)]
struct CandidateInfo {
    result_types: Vec<TypeId>,
    field_names: Vec<String>,
    field_name_set: IndexSet<String>,
}

/// If `expr` is a direct `Call(f)` / `MethodCall(f)` whose callee is a
/// candidate, return that candidate's index.
fn candidate_call_idx(
    body: &Body,
    expr: ExprId,
    candidate_names: &IndexMap<(String, ModuleSource), usize>,
) -> Option<usize> {
    let func = match &body.exprs[expr].kind {
        ExprKind::Call { func, .. } | ExprKind::MethodCall { func, .. } => func,
        _ => return None,
    };
    candidate_names
        .get(&(func.name.clone(), func.module_source.clone()))
        .copied()
}

/// Walk the argument expressions of a (Method)Call so tracked-local escapes in
/// the args invalidate the right candidate (the call's own ABI is accepted).
fn walk_call_args_for_uses(
    body: &Body,
    expr: ExprId,
    candidate_names: &IndexMap<(String, ModuleSource), usize>,
    candidates: &IndexMap<usize, CandidateInfo>,
    invalid: &mut IndexSet<usize>,
    tracked: &IndexMap<u32, usize>,
) {
    let args: Vec<ExprId> = match &body.exprs[expr].kind {
        ExprKind::Call { args, .. } => args.iter().map(|a| a.expr).collect(),
        ExprKind::MethodCall { receiver, args, .. } => std::iter::once(*receiver)
            .chain(args.iter().map(|a| a.expr))
            .collect(),
        _ => return,
    };
    for a in args {
        walk_expr_for_uses(body, a, candidate_names, candidates, invalid, tracked);
    }
}

/// Classify aggregate-returning functions and set `return_abi` on those whose
/// every return statement and call site permit the multi-value ABI.
pub fn classify_multi_value_returns(project: &mut NirPackage) {
    let type_table = project.type_table.borrow();
    let structs = &project.structs;

    // Phase 1: candidate identification — body-only checks.
    let mut candidates: IndexMap<usize, CandidateInfo> = IndexMap::default();
    for (idx, func_rc) in project.functions.iter().enumerate() {
        let func = func_rc.borrow();
        if let Some(info) = candidate_info(&func, &type_table, structs) {
            candidates.insert(idx, info);
        }
    }
    if candidates.is_empty() {
        return;
    }

    // Phase 2: call-site validation across every function body.
    let candidate_names: IndexMap<(String, ModuleSource), usize> = candidates
        .keys()
        .map(|&idx| {
            let func = project.functions[idx].borrow();
            ((func.name.clone(), func.module_source.clone()), idx)
        })
        .collect();

    let mut invalid: IndexSet<usize> = IndexSet::default();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        let Some(body) = &func.body else {
            continue;
        };
        validate_uses_in_block(body, body.root, &candidate_names, &candidates, &mut invalid);
    }

    // Phase 3: apply. Drop disqualified candidates, set ReturnAbi on the rest.
    drop(type_table);
    for (idx, info) in candidates {
        if invalid.contains(&idx) {
            continue;
        }
        let mut func = project.functions[idx].borrow_mut();
        func.return_abi = ReturnAbi::MultiValue {
            result_types: info.result_types,
            field_names: info.field_names,
        };
    }
}

/// Body-only candidate check.
fn candidate_info(
    func: &NirFunction,
    type_table: &TypeTable,
    structs: &[NirStruct],
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
    if func.is_trait_method() || func.is_closure_call() {
        return None;
    }

    let body = func.body.as_ref()?;
    let return_type = func.return_type;

    let (result_types, field_names, is_struct_shape) =
        aggregate_field_info(return_type, type_table, structs)?;
    if !(2..=4).contains(&result_types.len()) {
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

fn aggregate_field_info(
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
        name,
        module_source,
        ..
    } = resolved
    {
        let s = structs
            .iter()
            .find(|s| s.name == *name && s.module_source == *module_source)?;
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
struct ExpectedShape {
    arity: usize,
    struct_type: Option<TypeId>,
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

fn all_returns_match_shape(body: &Body, block: BlockId, expected: &ExpectedShape) -> bool {
    body.blocks[block]
        .stmts
        .iter()
        .all(|&stmt| stmt_returns_match(body, stmt, expected))
}

fn stmt_returns_match(body: &Body, stmt: StmtId, expected: &ExpectedShape) -> bool {
    match &body.stmts[stmt].kind {
        StmtKind::Return { value: None } => false,
        StmtKind::Return { value: Some(v) } => expr_returns_match(body, *v, expected),
        StmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            all_returns_match_shape(body, *then_block, expected)
                && else_block.is_none_or(|b| all_returns_match_shape(body, b, expected))
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            all_returns_match_shape(body, *b, expected)
        }
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
            nested_returns_in_expr_match(body, *value, expected)
        }
        StmtKind::Expr(e) => nested_returns_in_expr_match(body, *e, expected),
        StmtKind::Break { value, .. } => {
            value.is_none_or(|v| nested_returns_in_expr_match(body, v, expected))
        }
        StmtKind::Continue => true,
    }
}

fn nested_returns_in_expr_match(body: &Body, expr: ExprId, expected: &ExpectedShape) -> bool {
    let mut stmts = Vec::new();
    collect_stmts(body, NodeRef::Expr(expr), &mut stmts);
    stmts.iter().all(|&s| stmt_returns_match(body, s, expected))
}

fn expr_returns_match(body: &Body, expr: ExprId, expected: &ExpectedShape) -> bool {
    if body.exprs[expr].type_id == TypeTable::NEVER {
        return true;
    }
    match &body.exprs[expr].kind {
        ExprKind::TupleLiteral { elements } => {
            expected.struct_type.is_none() && elements.len() == expected.arity
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
        ExprKind::Match { arms, .. } => {
            let bodies: Vec<ExprId> = arms.iter().map(|a| a.body).collect();
            bodies
                .iter()
                .all(|&b| expr_returns_match(body, b, expected))
        }
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

fn all_break_values_match_shape(body: &Body, block: BlockId, expected: &ExpectedShape) -> bool {
    body.blocks[block]
        .stmts
        .iter()
        .all(|&stmt| stmt_break_values_match(body, stmt, expected))
}

fn stmt_break_values_match(body: &Body, stmt: StmtId, expected: &ExpectedShape) -> bool {
    match &body.stmts[stmt].kind {
        StmtKind::Break { value: Some(v), .. } => {
            let v = *v;
            expr_returns_match(body, v, expected) && expr_break_values_match(body, v, expected)
        }
        StmtKind::Break { value: None, .. } | StmtKind::Continue => true,
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let (condition, then_block, else_block) = (*condition, *then_block, *else_block);
            expr_break_values_match(body, condition, expected)
                && all_break_values_match_shape(body, then_block, expected)
                && else_block.is_none_or(|b| all_break_values_match_shape(body, b, expected))
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            all_break_values_match_shape(body, *b, expected)
        }
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
            expr_break_values_match(body, *value, expected)
        }
        StmtKind::Expr(e) | StmtKind::Return { value: Some(e) } => {
            expr_break_values_match(body, *e, expected)
        }
        StmtKind::Return { value: None } => true,
    }
}

fn expr_break_values_match(body: &Body, expr: ExprId, expected: &ExpectedShape) -> bool {
    let mut stmts = Vec::new();
    collect_stmts(body, NodeRef::Expr(expr), &mut stmts);
    stmts
        .iter()
        .all(|&s| stmt_break_values_match(body, s, expected))
}

fn block_tail_returns_match(body: &Body, block: BlockId, expected: &ExpectedShape) -> bool {
    let Some(&last) = body.blocks[block].stmts.last() else {
        return false;
    };
    match &body.stmts[last].kind {
        StmtKind::Expr(e) => expr_returns_match(body, *e, expected),
        StmtKind::Break { value: Some(v), .. } => expr_returns_match(body, *v, expected),
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
    candidate_names: &IndexMap<(String, ModuleSource), usize>,
    candidates: &IndexMap<usize, CandidateInfo>,
    invalid: &mut IndexSet<usize>,
) {
    let mut tracked: IndexMap<u32, usize> = IndexMap::default();
    for &stmt in &body.blocks[block].stmts {
        validate_stmt(
            body,
            stmt,
            candidate_names,
            candidates,
            invalid,
            &mut tracked,
        );
    }
}

fn validate_stmt(
    body: &Body,
    stmt: StmtId,
    candidate_names: &IndexMap<(String, ModuleSource), usize>,
    candidates: &IndexMap<usize, CandidateInfo>,
    invalid: &mut IndexSet<usize>,
    tracked: &mut IndexMap<u32, usize>,
) {
    match &body.stmts[stmt].kind {
        StmtKind::Let {
            local_index,
            value,
            is_mut,
            ..
        } => {
            let (local_index, value, is_mut) = (*local_index, *value, *is_mut);
            if !is_mut && let Some(candidate_idx) = candidate_call_idx(body, value, candidate_names)
            {
                tracked.insert(local_index, candidate_idx);
                walk_call_args_for_uses(body, value, candidate_names, candidates, invalid, tracked);
                return;
            }
            walk_expr_for_uses(body, value, candidate_names, candidates, invalid, tracked);
        }
        StmtKind::Expr(e) | StmtKind::Return { value: Some(e) } => {
            walk_expr_for_uses(body, *e, candidate_names, candidates, invalid, tracked);
        }
        StmtKind::Return { value: None } | StmtKind::Continue => {}
        StmtKind::Break { value, .. } => {
            if let Some(v) = value {
                walk_expr_for_uses(body, *v, candidate_names, candidates, invalid, tracked);
            }
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let (condition, then_block, else_block) = (*condition, *then_block, *else_block);
            walk_expr_for_uses(
                body,
                condition,
                candidate_names,
                candidates,
                invalid,
                tracked,
            );
            let mut inner = tracked.clone();
            for &s in &body.blocks[then_block].stmts.clone() {
                validate_stmt(body, s, candidate_names, candidates, invalid, &mut inner);
            }
            if let Some(eb) = else_block {
                let mut inner = tracked.clone();
                for &s in &body.blocks[eb].stmts.clone() {
                    validate_stmt(body, s, candidate_names, candidates, invalid, &mut inner);
                }
            }
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            let b = *b;
            let mut inner = tracked.clone();
            for &s in &body.blocks[b].stmts.clone() {
                validate_stmt(body, s, candidate_names, candidates, invalid, &mut inner);
            }
        }
        StmtKind::LetDestructure { value, .. } => {
            walk_expr_for_uses(body, *value, candidate_names, candidates, invalid, tracked);
        }
    }
}

fn walk_expr_for_uses(
    body: &Body,
    expr: ExprId,
    candidate_names: &IndexMap<(String, ModuleSource), usize>,
    candidates: &IndexMap<usize, CandidateInfo>,
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
            if let ExprKind::Local { index, .. } = &body.exprs[source].kind
                && let Some(&candidate_idx) = tracked.get(index)
                && let Some(info) = candidates.get(&candidate_idx)
                && info.field_name_set.contains(field_name)
            {
                return;
            }
            walk_expr_for_uses(body, source, candidate_names, candidates, invalid, tracked);
        }
        ExprKind::Local { index, .. } => {
            if let Some(&candidate_idx) = tracked.get(index) {
                invalid.insert(candidate_idx);
            }
        }
        ExprKind::Call { func, args, .. } => {
            if let Some(&candidate_idx) =
                candidate_names.get(&(func.name.clone(), func.module_source.clone()))
            {
                invalid.insert(candidate_idx);
            }
            let args: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            for a in args {
                walk_expr_for_uses(body, a, candidate_names, candidates, invalid, tracked);
            }
        }
        ExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } => {
            if let Some(&candidate_idx) =
                candidate_names.get(&(func.name.clone(), func.module_source.clone()))
            {
                invalid.insert(candidate_idx);
            }
            let receiver = *receiver;
            let args: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            walk_expr_for_uses(
                body,
                receiver,
                candidate_names,
                candidates,
                invalid,
                tracked,
            );
            for a in args {
                walk_expr_for_uses(body, a, candidate_names, candidates, invalid, tracked);
            }
        }
        ExprKind::CmRawCall { args, .. } => {
            let args = args.clone();
            for a in args {
                walk_expr_for_uses(body, a, candidate_names, candidates, invalid, tracked);
            }
        }
        ExprKind::IndirectCall { callee, args } => {
            let callee = *callee;
            let args = args.clone();
            walk_expr_for_uses(body, callee, candidate_names, candidates, invalid, tracked);
            for a in args {
                walk_expr_for_uses(body, a, candidate_names, candidates, invalid, tracked);
            }
        }
        ExprKind::Block(b) | ExprKind::LabeledBlock { block: b, .. } => {
            let b = *b;
            let mut inner = tracked.clone();
            for &s in &body.blocks[b].stmts.clone() {
                validate_stmt(body, s, candidate_names, candidates, invalid, &mut inner);
            }
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let (condition, then_branch, else_branch) = (*condition, *then_branch, *else_branch);
            walk_expr_for_uses(
                body,
                condition,
                candidate_names,
                candidates,
                invalid,
                tracked,
            );
            let mut inner = tracked.clone();
            for &s in &body.blocks[then_branch].stmts.clone() {
                validate_stmt(body, s, candidate_names, candidates, invalid, &mut inner);
            }
            if let Some(eb) = else_branch {
                let mut inner = tracked.clone();
                for &s in &body.blocks[eb].stmts.clone() {
                    validate_stmt(body, s, candidate_names, candidates, invalid, &mut inner);
                }
            }
        }
        ExprKind::Match { expr: scrut, arms } => {
            let scrut = *scrut;
            let arm_data: Vec<(Option<ExprId>, ExprId)> =
                arms.iter().map(|a| (a.guard, a.body)).collect();
            walk_expr_for_uses(body, scrut, candidate_names, candidates, invalid, tracked);
            for (guard, arm_body) in arm_data {
                if let Some(g) = guard {
                    walk_expr_for_uses(body, g, candidate_names, candidates, invalid, tracked);
                }
                walk_expr_for_uses(
                    body,
                    arm_body,
                    candidate_names,
                    candidates,
                    invalid,
                    tracked,
                );
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
            walk_expr_for_uses(
                body,
                scrutinee,
                candidate_names,
                candidates,
                invalid,
                tracked,
            );
            for arm in arms {
                let mut inner = tracked.clone();
                for &s in &body.blocks[arm].stmts.clone() {
                    validate_stmt(body, s, candidate_names, candidates, invalid, &mut inner);
                }
            }
            let mut inner = tracked.clone();
            for &s in &body.blocks[default].stmts.clone() {
                validate_stmt(body, s, candidate_names, candidates, invalid, &mut inner);
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
                walk_expr_for_uses(body, e, candidate_names, candidates, invalid, tracked);
            }
        }
    }
}
