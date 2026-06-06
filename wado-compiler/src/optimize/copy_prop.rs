//! Copy propagation optimization for Wado NIR.
//!
//! Eliminates trivial copy bindings like `let x = y`, `let x = 42`,
//! `let x = &y`, or `let x = &mut y` by propagating the source value to all
//! uses of the target variable. See `can_propagate_copy` for the safety gates.
//!
//! Ported off the `Body ↔ tree` bridge (Phase 4 stage C; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`): the binding / usage
//! analysis and the substitute-and-remove rewrite read and mutate the arena
//! `Body` directly.

use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::{NirFunction, NirUnaryOp};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, ExprNode, NodeRef, StmtId, StmtKind};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};

#[derive(Debug, Clone)]
struct CopyBinding {
    target_local: u32,
    source: CopySource,
    type_id: TypeId,
}

#[derive(Debug, Clone)]
enum CopySource {
    Local {
        index: u32,
        name: String,
    },
    IntLiteral {
        value: u64,
        repr: String,
    },
    FloatLiteral {
        value: f64,
        repr: String,
    },
    BoolLiteral(bool),
    CharLiteral(char),
    Ref {
        index: u32,
        name: String,
        inner_type_id: TypeId,
    },
    MutRef {
        index: u32,
        name: String,
        inner_type_id: TypeId,
    },
}

#[derive(Debug, Default)]
struct LocalUsage {
    read_count: u32,
    is_assigned: bool,
    has_field_mutation: bool,
    address_taken: bool,
}

/// If `expr` is `builtin::copy_value::<T>(inner)`, return `inner`; else `expr`.
fn unwrap_copy_value(body: &Body, expr: ExprId) -> ExprId {
    if let ExprKind::Call { func, args, .. } = &body.exprs[expr].kind
        && func.module_source.is_core_builtin()
        && func.name == "copy_value"
        && args.len() == 1
    {
        return args[0].expr;
    }
    expr
}

fn analyze_copy_binding(body: &Body, stmt: StmtId) -> Option<CopyBinding> {
    let StmtKind::Let {
        local_index,
        value,
        skip_value_copy,
        ..
    } = &body.stmts[stmt].kind
    else {
        return None;
    };
    let (local_index, value, skip_value_copy) = (*local_index, *value, *skip_value_copy);
    if skip_value_copy {
        return None;
    }
    let value = unwrap_copy_value(body, value);
    let value_type = body.exprs[value].type_id;

    let source = match &body.exprs[value].kind {
        ExprKind::Local { index, name } => CopySource::Local {
            index: *index,
            name: name.clone(),
        },
        ExprKind::IntLiteral { value, repr } => CopySource::IntLiteral {
            value: *value,
            repr: repr.clone(),
        },
        ExprKind::FloatLiteral { value, repr } => CopySource::FloatLiteral {
            value: *value,
            repr: repr.clone(),
        },
        ExprKind::BoolLiteral(b) => CopySource::BoolLiteral(*b),
        ExprKind::CharLiteral(c) => CopySource::CharLiteral(*c),
        ExprKind::Unary { op, expr: inner }
            if matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef) =>
        {
            let inner = *inner;
            let is_ref = matches!(op, NirUnaryOp::Ref);
            if let ExprKind::Local { index, name } = &body.exprs[inner].kind {
                let inner_type_id = body.exprs[inner].type_id;
                if is_ref {
                    CopySource::Ref {
                        index: *index,
                        name: name.clone(),
                        inner_type_id,
                    }
                } else {
                    CopySource::MutRef {
                        index: *index,
                        name: name.clone(),
                        inner_type_id,
                    }
                }
            } else {
                return None;
            }
        }
        _ => return None,
    };

    Some(CopyBinding {
        target_local: local_index,
        source,
        type_id: value_type,
    })
}

struct AnalysisResult {
    bindings: Vec<CopyBinding>,
    usage: IndexMap<u32, LocalUsage>,
}

type FirstParamTypes = IndexMap<(ModuleSource, String), TypeId>;

fn analyze_function_body(
    body: &Body,
    type_table: &TypeTable,
    first_param_types: &FirstParamTypes,
) -> AnalysisResult {
    let mut result = AnalysisResult {
        bindings: Vec::new(),
        usage: IndexMap::default(),
    };
    analyze_block(body, body.root, &mut result, type_table, first_param_types);
    result
}

fn analyze_block(
    body: &Body,
    block: BlockId,
    result: &mut AnalysisResult,
    type_table: &TypeTable,
    fpt: &FirstParamTypes,
) {
    let stmts = body.blocks[block].stmts.clone();
    for stmt in stmts {
        if let Some(binding) = analyze_copy_binding(body, stmt) {
            result.bindings.push(binding);
        }
        analyze_stmt(body, stmt, result, type_table, fpt);
    }
}

fn analyze_stmt(
    body: &Body,
    stmt: StmtId,
    result: &mut AnalysisResult,
    type_table: &TypeTable,
    fpt: &FirstParamTypes,
) {
    let mut kids = Vec::new();
    body.for_each_child(NodeRef::Stmt(stmt), |c| kids.push(c));
    for c in kids {
        match c {
            NodeRef::Expr(e) => analyze_expr(body, e, result, type_table, fpt),
            NodeRef::Block(b) => analyze_block(body, b, result, type_table, fpt),
            _ => {}
        }
    }
}

fn analyze_expr(
    body: &Body,
    id: ExprId,
    result: &mut AnalysisResult,
    type_table: &TypeTable,
    fpt: &FirstParamTypes,
) {
    match &body.exprs[id].kind {
        ExprKind::Local { index, .. } => {
            result.usage.entry(*index).or_default().read_count += 1;
        }
        ExprKind::Assign { target, value } => {
            let (target, value) = (*target, *value);
            if let ExprKind::Local { index, .. } = &body.exprs[target].kind {
                result.usage.entry(*index).or_default().is_assigned = true;
            }
            if let ExprKind::FieldAccess { expr: inner, .. } = &body.exprs[target].kind
                && let ExprKind::Local { index, .. } = &body.exprs[*inner].kind
            {
                result.usage.entry(*index).or_default().has_field_mutation = true;
            }
            analyze_expr(body, target, result, type_table, fpt);
            analyze_expr(body, value, result, type_table, fpt);
        }
        ExprKind::Unary { op, expr: inner } => {
            let (op, inner) = (*op, *inner);
            if matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef)
                && let ExprKind::Local { index, .. } = &body.exprs[inner].kind
            {
                let index = *index;
                result.usage.entry(index).or_default().address_taken = true;
                if matches!(op, NirUnaryOp::MutRef) {
                    result.usage.entry(index).or_default().has_field_mutation = true;
                }
            }
            analyze_expr(body, inner, result, type_table, fpt);
        }
        ExprKind::Call { args, .. } => {
            let arg_data: Vec<(ExprId, bool)> = args.iter().map(|a| (a.expr, a.is_mut)).collect();
            for (arg, is_mut) in arg_data {
                if is_mut && may_mutate_through_arg(body, arg, type_table) {
                    mark_potentially_mutated_local(body, arg, result);
                }
                analyze_expr(body, arg, result, type_table, fpt);
            }
        }
        ExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } => {
            let receiver = *receiver;
            let func_key = (func.module_source.clone(), func.name.clone());
            let arg_data: Vec<(ExprId, bool)> = args.iter().map(|a| (a.expr, a.is_mut)).collect();
            let receiver_is_mut_ref = may_mutate_caller_state(body, receiver, type_table);
            let func_first_param_is_mut_ref = fpt
                .get(&func_key)
                .is_some_and(|&tp| matches!(type_table.get(tp), ResolvedType::MutRef(_)));
            if receiver_is_mut_ref || func_first_param_is_mut_ref {
                mark_potentially_mutated_local(body, receiver, result);
            }
            analyze_expr(body, receiver, result, type_table, fpt);
            for (arg, is_mut) in arg_data {
                if is_mut && may_mutate_through_arg(body, arg, type_table) {
                    mark_potentially_mutated_local(body, arg, result);
                }
                analyze_expr(body, arg, result, type_table, fpt);
            }
        }
        _ => {
            let mut kids = Vec::new();
            body.for_each_child(NodeRef::Expr(id), |c| kids.push(c));
            for c in kids {
                match c {
                    NodeRef::Expr(e) => analyze_expr(body, e, result, type_table, fpt),
                    NodeRef::Block(b) => analyze_block(body, b, result, type_table, fpt),
                    _ => {}
                }
            }
        }
    }
}

fn mark_potentially_mutated_local(body: &Body, expr: ExprId, result: &mut AnalysisResult) {
    match &body.exprs[expr].kind {
        ExprKind::Local { index, .. } => {
            result.usage.entry(*index).or_default().has_field_mutation = true;
        }
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::Index { expr: inner, .. } => {
            mark_potentially_mutated_local(body, *inner, result);
        }
        _ => {}
    }
}

fn may_mutate_caller_state(body: &Body, expr: ExprId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(body.exprs[expr].type_id),
        ResolvedType::MutRef(_)
    )
}

fn may_mutate_through_arg(body: &Body, expr: ExprId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(body.exprs[expr].type_id),
        ResolvedType::MutRef(_)
    )
}

fn needs_value_copy(type_id: TypeId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(type_id),
        ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. }
    )
}

fn can_propagate_copy(
    binding: &CopyBinding,
    usage: &IndexMap<u32, LocalUsage>,
    type_table: &TypeTable,
) -> bool {
    let Some(target_usage) = usage.get(&binding.target_local) else {
        return true;
    };
    if target_usage.is_assigned {
        return false;
    }
    if target_usage.has_field_mutation && needs_value_copy(binding.type_id, type_table) {
        return false;
    }
    if target_usage.address_taken {
        return false;
    }

    match &binding.source {
        CopySource::Local { index, .. } => {
            let source_usage = usage.get(index);
            if let Some(su) = source_usage
                && su.is_assigned
            {
                return false;
            }
            let is_value_type = needs_value_copy(binding.type_id, type_table);
            if is_value_type
                && target_usage.read_count == 1
                && let Some(su) = source_usage
                && su.has_field_mutation
            {
                return false;
            }
            let single_use_value_copy = is_value_type && target_usage.read_count == 1;
            if let Some(su) = source_usage
                && !single_use_value_copy
                && su.address_taken
            {
                return false;
            }
            if is_value_type
                && !single_use_value_copy
                && let Some(su) = source_usage
                && (su.read_count > 1 || su.address_taken)
            {
                return false;
            }
            true
        }
        CopySource::IntLiteral { .. }
        | CopySource::FloatLiteral { .. }
        | CopySource::BoolLiteral(_)
        | CopySource::CharLiteral(_) => true,
        CopySource::Ref { index, .. } | CopySource::MutRef { index, .. } => {
            if target_usage.read_count != 1 {
                return false;
            }
            if let Some(su) = usage.get(index)
                && su.is_assigned
            {
                return false;
            }
            true
        }
    }
}

fn apply_in_block(
    body: &mut Body,
    block: BlockId,
    substitutions: &IndexMap<u32, CopySource>,
    dead_locals: &IndexSet<u32>,
) {
    let kept: Vec<StmtId> = body.blocks[block]
        .stmts
        .iter()
        .copied()
        .filter(|s| match &body.stmts[*s].kind {
            StmtKind::Let { local_index, .. } => !dead_locals.contains(local_index),
            _ => true,
        })
        .collect();
    body.blocks[block].stmts = kept;

    let stmts = body.blocks[block].stmts.clone();
    for stmt in stmts {
        apply_in_node(body, NodeRef::Stmt(stmt), substitutions, dead_locals);
    }
}

fn apply_in_node(
    body: &mut Body,
    node: NodeRef,
    substitutions: &IndexMap<u32, CopySource>,
    dead_locals: &IndexSet<u32>,
) {
    match node {
        NodeRef::Expr(id) => apply_in_expr(body, id, substitutions, dead_locals),
        NodeRef::Block(b) => apply_in_block(body, b, substitutions, dead_locals),
        NodeRef::Stmt(s) => {
            let mut kids = Vec::new();
            body.for_each_child(NodeRef::Stmt(s), |c| kids.push(c));
            for c in kids {
                apply_in_node(body, c, substitutions, dead_locals);
            }
        }
        NodeRef::Pat(_) => {}
    }
}

fn apply_in_expr(
    body: &mut Body,
    id: ExprId,
    substitutions: &IndexMap<u32, CopySource>,
    dead_locals: &IndexSet<u32>,
) {
    let sub = if let ExprKind::Local { index, .. } = &body.exprs[id].kind {
        substitutions.get(index).cloned()
    } else {
        None
    };
    if let Some(source) = sub {
        match source {
            CopySource::Local { index, name } => {
                body.exprs[id].kind = ExprKind::Local { index, name };
            }
            CopySource::IntLiteral { value, repr } => {
                body.exprs[id].kind = ExprKind::IntLiteral { value, repr };
            }
            CopySource::FloatLiteral { value, repr } => {
                body.exprs[id].kind = ExprKind::FloatLiteral { value, repr };
            }
            CopySource::BoolLiteral(b) => {
                body.exprs[id].kind = ExprKind::BoolLiteral(b);
            }
            CopySource::CharLiteral(c) => {
                body.exprs[id].kind = ExprKind::CharLiteral(c);
            }
            CopySource::Ref {
                index,
                name,
                inner_type_id,
            } => emit_ref(body, id, NirUnaryOp::Ref, index, name, inner_type_id),
            CopySource::MutRef {
                index,
                name,
                inner_type_id,
            } => emit_ref(body, id, NirUnaryOp::MutRef, index, name, inner_type_id),
        }
        return;
    }

    let mut kids = Vec::new();
    body.for_each_child(NodeRef::Expr(id), |c| kids.push(c));
    for c in kids {
        apply_in_node(body, c, substitutions, dead_locals);
    }
}

/// Replace expression `id` with `&src` / `&mut src` (the propagated ref source),
/// keeping `id`'s own `type_id` / span.
fn emit_ref(
    body: &mut Body,
    id: ExprId,
    op: NirUnaryOp,
    index: u32,
    name: String,
    inner_type_id: TypeId,
) {
    let span = body.exprs[id].span;
    let inner = body.exprs.push(ExprNode {
        kind: ExprKind::Local { index, name },
        type_id: inner_type_id,
        span,
    });
    body.exprs[id].kind = ExprKind::Unary { op, expr: inner };
}

fn propagate_copies_in_function(
    func: &mut NirFunction,
    type_table: &TypeTable,
    first_param_types: &FirstParamTypes,
) -> bool {
    if func.body.is_none() {
        return false;
    }
    let mut ever_changed = false;

    loop {
        let analysis = {
            let body = func.body.as_ref().unwrap();
            analyze_function_body(body, type_table, first_param_types)
        };
        if analysis.bindings.is_empty() {
            break;
        }
        let eliminable: Vec<CopyBinding> = analysis
            .bindings
            .into_iter()
            .filter(|b| can_propagate_copy(b, &analysis.usage, type_table))
            .collect();
        if eliminable.is_empty() {
            break;
        }
        let target_set: IndexSet<u32> = eliminable.iter().map(|b| b.target_local).collect();
        let mut substitutions: IndexMap<u32, CopySource> = IndexMap::default();
        let mut has_deferred = false;
        for binding in eliminable {
            let source_conflicts = match &binding.source {
                CopySource::Local { index, .. }
                | CopySource::Ref { index, .. }
                | CopySource::MutRef { index, .. } => target_set.contains(index),
                _ => false,
            };
            if source_conflicts {
                has_deferred = true;
            } else {
                substitutions.insert(binding.target_local, binding.source);
            }
        }
        if substitutions.is_empty() {
            break;
        }
        let dead_locals: IndexSet<u32> = substitutions.keys().copied().collect();
        let body = func.body.as_mut().unwrap();
        let root = body.root;
        apply_in_block(body, root, &substitutions, &dead_locals);
        ever_changed = true;
        if !has_deferred {
            break;
        }
    }

    ever_changed
}

pub fn propagate_copies(project: &mut NirPackage) -> bool {
    let mut changed = false;
    let type_table = project.type_table.borrow();
    let mut first_param_types: FirstParamTypes = IndexMap::default();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if let Some(first_param) = func.params.first() {
            let key = (func.module_source.clone(), func.name.clone());
            first_param_types.insert(key, first_param.type_id);
        }
    }
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        changed |= propagate_copies_in_function(&mut func, &type_table, &first_param_types);
    }
    changed
}
