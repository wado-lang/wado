//! Async export `task return` expansion.
//!
//! Walks the body of an `export async fn` and replaces each
//! `task return value;` with the inline CM `task-return` call sequence:
//! flatten the value to CM ABI flat slots and emit a `task-return` raw call.
//!
//! For non-async or non-export contexts (test world), `task return` is
//! stripped to a no-op so it never reaches monomorphize.

use std::cell::RefCell;
use std::rc::Rc;

use crate::cm_abi;
use crate::component_model::WasiRegistry;
use crate::hashmap::IndexMap;
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::tir::{
    ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction, TirLocal, TirMatchArm, TirModule,
    TirPattern, TirStmt, TirStmtKind, TypeTable,
};

use crate::synthesis::common::{
    alloc_local, assign, cast, cm_raw_call, expr_stmt, i32_const, let_mut_stmt, let_stmt,
    local_ref, synth_span,
};

use super::export_adapter::{synthesize_lower_to_flat, synthesize_variant_lower_to_flat};
use super::types::{
    LiftContext, cm_val_type_to_type_id, cm_zero, find_variant_decl, flat_types_from_type_id,
};

/// Expand `TaskReturn` stmts in an `export async fn` user function into inline CM calls.
///
/// Walks the function body and replaces each `TirStmtKind::TaskReturn { value }` with
/// the flat lowering + `cm_raw_call("task-return", flat_args)` sequence.
/// New locals are appended to the function's `locals` and `local_count` is updated.
pub(super) fn expand_task_returns_in_func(
    user_func: &Rc<RefCell<TirFunction>>,
    flat_return_types: &[cm_abi::CmValType],
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table: &Rc<RefCell<TypeTable>>,
    wasi_registry: &WasiRegistry,
    cm_package: &str,
    interner: &RefCell<ModuleSourceInterner>,
) {
    let mut func = user_func.borrow_mut();
    let mut next_local = func.local_count;
    let mut extra_locals: Vec<TirLocal> = Vec::new();
    // Take the body out to avoid simultaneous mutable/immutable borrows of func
    let Some(mut body) = func.body.take() else {
        return;
    };
    expand_task_return_in_block(
        &mut body,
        flat_return_types,
        &mut next_local,
        &mut extra_locals,
        tir_modules,
        type_table,
        wasi_registry,
        cm_package,
        interner,
    );
    func.body = Some(body);
    func.local_count = next_local;
    func.locals.extend(extra_locals);
}

/// Replace every `task return` statement in a function body with a no-op (`Continue`).
///
/// Used in the test world where `export async fn` bodies are not exported and will
/// be removed by DCE. The statements must not reach `monomorphize` intact.
pub(super) fn strip_task_returns_in_func(user_func: &Rc<RefCell<TirFunction>>) {
    let mut func = user_func.borrow_mut();
    let Some(mut body) = func.body.take() else {
        return;
    };
    strip_task_returns_in_block(&mut body);
    func.body = Some(body);
}

fn strip_task_returns_in_block(blk: &mut TirBlock) {
    for stmt in &mut blk.stmts {
        if matches!(&stmt.kind, TirStmtKind::TaskReturn { .. }) {
            stmt.kind = TirStmtKind::Continue;
        } else {
            strip_task_returns_in_stmt(stmt);
        }
    }
}

fn strip_task_returns_in_stmt(stmt: &mut TirStmt) {
    match &mut stmt.kind {
        TirStmtKind::If {
            then_block,
            else_block,
            ..
        }
        | TirStmtKind::IfLet {
            then_block,
            else_block,
            ..
        } => {
            strip_task_returns_in_block(then_block);
            if let Some(else_blk) = else_block {
                strip_task_returns_in_block(else_blk);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            strip_task_returns_in_block(body);
        }
        _ => {}
    }
}

fn expand_task_return_in_block(
    blk: &mut TirBlock,
    flat_return_types: &[cm_abi::CmValType],
    next_local: &mut u32,
    locals: &mut Vec<TirLocal>,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table: &Rc<RefCell<TypeTable>>,
    wasi_registry: &WasiRegistry,
    cm_package: &str,
    interner: &RefCell<ModuleSourceInterner>,
) {
    let stmts = std::mem::take(&mut blk.stmts);
    let mut new_stmts: Vec<TirStmt> = Vec::with_capacity(stmts.len());
    for mut stmt in stmts {
        if matches!(&stmt.kind, TirStmtKind::TaskReturn { .. }) {
            if let TirStmtKind::TaskReturn { value } =
                std::mem::replace(&mut stmt.kind, TirStmtKind::Continue)
            {
                let expanded = generate_inline_task_return(
                    value,
                    flat_return_types,
                    next_local,
                    locals,
                    tir_modules,
                    type_table,
                    wasi_registry,
                    cm_package,
                    interner,
                );
                new_stmts.extend(expanded);
            }
        } else {
            expand_task_return_in_stmt(
                &mut stmt,
                flat_return_types,
                next_local,
                locals,
                tir_modules,
                type_table,
                wasi_registry,
                cm_package,
                interner,
            );
            new_stmts.push(stmt);
        }
    }
    blk.stmts = new_stmts;
}

fn expand_task_return_in_stmt(
    stmt: &mut TirStmt,
    flat_return_types: &[cm_abi::CmValType],
    next_local: &mut u32,
    locals: &mut Vec<TirLocal>,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table: &Rc<RefCell<TypeTable>>,
    wasi_registry: &WasiRegistry,
    cm_package: &str,
    interner: &RefCell<ModuleSourceInterner>,
) {
    match &mut stmt.kind {
        TirStmtKind::If {
            then_block,
            else_block,
            ..
        }
        | TirStmtKind::IfLet {
            then_block,
            else_block,
            ..
        } => {
            expand_task_return_in_block(
                then_block,
                flat_return_types,
                next_local,
                locals,
                tir_modules,
                type_table,
                wasi_registry,
                cm_package,
                interner,
            );
            if let Some(blk) = else_block {
                expand_task_return_in_block(
                    blk,
                    flat_return_types,
                    next_local,
                    locals,
                    tir_modules,
                    type_table,
                    wasi_registry,
                    cm_package,
                    interner,
                );
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            expand_task_return_in_block(
                body,
                flat_return_types,
                next_local,
                locals,
                tir_modules,
                type_table,
                wasi_registry,
                cm_package,
                interner,
            );
        }
        _ => {}
    }
}

/// Generate the inline task-return sequence for `task return value`.
///
/// For `Result<T, E>` values, generates:
/// - Ok arm: flatten T → call task-return(0, ...`flat_ok_values`)
/// - Err arm: flatten E → call task-return(1, ...`flat_err_values`)
///
/// For other types, flattens `value` to its CM ABI flat slots and emits
/// `task-return(...flat_values)`. For unit-returning exports, the value
/// is evaluated for its side effects and `task-return()` is emitted.
fn generate_inline_task_return(
    value: TirExpr,
    flat_return_types: &[cm_abi::CmValType],
    next_local: &mut u32,
    locals: &mut Vec<TirLocal>,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table: &Rc<RefCell<TypeTable>>,
    wasi_registry: &WasiRegistry,
    cm_package: &str,
    interner: &RefCell<ModuleSourceInterner>,
) -> Vec<TirStmt> {
    let lift_ctx = LiftContext {
        wasi_registry,
        type_table,
        cm_package,
        interner,
    };
    let mut stmts: Vec<TirStmt> = Vec::new();
    let value_type_id = value.type_id;

    let tt = type_table.borrow();
    let is_result = matches!(
        tt.get(value_type_id),
        ResolvedType::GenericInstance { name, .. } if name == "Result"
    );

    if is_result && !flat_return_types.is_empty() {
        let (ok_type_id, err_type_id) = match tt.get(value_type_id) {
            ResolvedType::GenericInstance { type_args, .. } if type_args.len() == 2 => {
                (type_args[0], type_args[1])
            }
            _ => panic!("Expected Result<T, E> type"),
        };
        let items = tt.compiler_items();
        let (_, _, ok_case_name, ok_case_index) =
            items.require_variant_case(crate::compiler_item::CompilerItem::ResultOk);
        let ok_case_name = ok_case_name.to_string();
        let (_, _, err_case_name, err_case_index) =
            items.require_variant_case(crate::compiler_item::CompilerItem::ResultErr);
        let err_case_name = err_case_name.to_string();
        drop(tt);

        // Store result in local
        let result_local = alloc_local(next_local, locals, value_type_id);
        stmts.push(let_stmt("__task_ret", result_local, value_type_id, value));

        // Allocate mutable flat value locals (initialized to zero)
        let flat_locals: Vec<(u32, String)> = flat_return_types
            .iter()
            .enumerate()
            .map(|(i, &vt)| {
                let type_id = cm_val_type_to_type_id(vt);
                let local = alloc_local(next_local, locals, type_id);
                let name = format!("__tv_{i}");
                stmts.push(let_mut_stmt(&name, local, type_id, cm_zero(vt)));
                (local, name)
            })
            .collect();

        let task_return_args: Vec<TirExpr> = flat_locals
            .iter()
            .zip(flat_return_types.iter())
            .map(|((local, name), &vt)| local_ref(*local, name, cm_val_type_to_type_id(vt)))
            .collect();

        let tt = type_table.borrow();
        let ok_flat_types = flat_types_from_type_id(ok_type_id, tir_modules, &tt);
        drop(tt);

        // Allocate the Ok payload binding local unconditionally so the
        // Match arm pattern can reference it, even when `ok_flat_types`
        // is empty (no flat slots to populate). When empty, the binding
        // is bound but never read, which is fine — `wir_build` won't
        // emit a spurious read.
        let ok_payload_local = alloc_local(next_local, locals, ok_type_id);
        let ok_payload_name = format!("__ok_val_{ok_payload_local}");

        // === Ok arm body ===
        let mut ok_stmts: Vec<TirStmt> = Vec::new();
        ok_stmts.push(expr_stmt(assign(
            local_ref(
                flat_locals[0].0,
                &flat_locals[0].1,
                cm_val_type_to_type_id(flat_return_types[0]),
            ),
            i32_const(0),
        )));
        if !ok_flat_types.is_empty() {
            let ok_lowered = synthesize_lower_to_flat(
                local_ref(ok_payload_local, &ok_payload_name, ok_type_id),
                ok_type_id,
                next_local,
                &mut ok_stmts,
                locals,
                tir_modules,
                lift_ctx,
            );
            for (i, flat_val) in ok_lowered.iter().enumerate() {
                if 1 + i < flat_locals.len() {
                    let target_type = cm_val_type_to_type_id(flat_return_types[1 + i]);
                    let source_type = cm_val_type_to_type_id(flat_val.cm_type);
                    let mut val = local_ref(flat_val.index, "__flat", source_type);
                    if flat_val.cm_type != flat_return_types[1 + i] {
                        val = cast(val, target_type);
                    }
                    ok_stmts.push(expr_stmt(assign(
                        local_ref(flat_locals[1 + i].0, &flat_locals[1 + i].1, target_type),
                        val,
                    )));
                }
            }
        }
        ok_stmts.push(expr_stmt(cm_raw_call(
            "task-return",
            task_return_args.clone(),
            TypeTable::UNIT,
        )));
        // No return here: task.return is a cooperative yield, not a function exit.
        // Execution continues after task.return so user code after `task return` runs.

        // === Err arm body ===
        let err_payload_local = alloc_local(next_local, locals, err_type_id);
        let err_payload_name = format!("__err_val_{err_payload_local}");
        let mut err_stmts: Vec<TirStmt> = Vec::new();
        err_stmts.push(expr_stmt(assign(
            local_ref(
                flat_locals[0].0,
                &flat_locals[0].1,
                cm_val_type_to_type_id(flat_return_types[0]),
            ),
            i32_const(1),
        )));
        let err_resolved = type_table.borrow().get(err_type_id).clone();
        if let ResolvedType::Variant { name, .. } = &err_resolved {
            if let Some(variant_decl) = find_variant_decl(name, tir_modules) {
                synthesize_variant_lower_to_flat(
                    err_payload_local,
                    err_type_id,
                    &variant_decl,
                    &flat_locals[1..],
                    &flat_return_types[1..],
                    next_local,
                    &mut err_stmts,
                    locals,
                    tir_modules,
                    lift_ctx,
                );
            } else if flat_locals.len() > 1 {
                err_stmts.push(expr_stmt(assign(
                    local_ref(
                        flat_locals[1].0,
                        &flat_locals[1].1,
                        cm_val_type_to_type_id(flat_return_types[1]),
                    ),
                    local_ref(err_payload_local, &err_payload_name, err_type_id),
                )));
            }
        } else {
            let err_lowered = synthesize_lower_to_flat(
                local_ref(err_payload_local, &err_payload_name, err_type_id),
                err_type_id,
                next_local,
                &mut err_stmts,
                locals,
                tir_modules,
                lift_ctx,
            );
            for (i, flat_val) in err_lowered.iter().enumerate() {
                if 1 + i < flat_locals.len() {
                    let target_type = cm_val_type_to_type_id(flat_return_types[1 + i]);
                    let source_type = cm_val_type_to_type_id(flat_val.cm_type);
                    let mut val = local_ref(flat_val.index, "__flat", source_type);
                    if flat_val.cm_type != flat_return_types[1 + i] {
                        val = cast(val, target_type);
                    }
                    err_stmts.push(expr_stmt(assign(
                        local_ref(flat_locals[1 + i].0, &flat_locals[1 + i].1, target_type),
                        val,
                    )));
                }
            }
        }
        err_stmts.push(expr_stmt(cm_raw_call(
            "task-return",
            task_return_args,
            TypeTable::UNIT,
        )));
        // No return here: see comment in ok_stmts above.

        // Combine Ok/Err arms in a single TIR `Match` over the
        // materialised result. Each arm's pattern binds the payload
        // local that the arm body reads from, replacing the previous
        // `let __ok_val = variant_payload(result, Ok)` /
        // `let __err_val = variant_payload(result, Err)` extractions.
        let span = synth_span();
        let ok_arm = TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type: value_type_id,
                variant_name: ok_case_name,
                bindings: vec![TirPattern::Binding {
                    name: ok_payload_name,
                    local_index: ok_payload_local,
                    type_id: ok_type_id,
                }],
                payload_type: ok_type_id,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Block(TirBlock::new(ok_stmts, span)),
                TypeTable::UNIT,
                span,
            ),
            span,
        };
        let err_arm = TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type: value_type_id,
                variant_name: err_case_name,
                bindings: vec![TirPattern::Binding {
                    name: err_payload_name,
                    local_index: err_payload_local,
                    type_id: err_type_id,
                }],
                payload_type: err_type_id,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Block(TirBlock::new(err_stmts, span)),
                TypeTable::UNIT,
                span,
            ),
            span,
        };
        let match_expr = TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(local_ref(result_local, "__task_ret", value_type_id)),
                arms: vec![ok_arm, err_arm],
            },
            TypeTable::UNIT,
            span,
        );
        stmts.push(TirStmt::new(TirStmtKind::Expr(match_expr), span));
        // Suppress unused-variable warning for the case names since the
        // pattern paths now read them directly above (no separate
        // variant_test argument).
        let _ = (ok_case_index, err_case_index);
    } else {
        drop(tt);
        // Non-Result task return — flatten the value and forward the flat
        // slots verbatim. Unit-returning exports (`flat_return_types`
        // empty) collapse to `task-return()` with the value evaluated for
        // its side effects.
        if flat_return_types.is_empty() {
            // Evaluate `value` for side effects, then signal completion
            // with no flat payload.
            stmts.push(expr_stmt(value));
            stmts.push(expr_stmt(cm_raw_call(
                "task-return",
                vec![],
                TypeTable::UNIT,
            )));
        } else {
            let lowered = synthesize_lower_to_flat(
                value,
                value_type_id,
                next_local,
                &mut stmts,
                locals,
                tir_modules,
                lift_ctx,
            );
            assert_eq!(
                lowered.len(),
                flat_return_types.len(),
                "non-Result task return: flat-lowered slot count {} != world flat-return-type count {}",
                lowered.len(),
                flat_return_types.len(),
            );
            let mut task_return_args: Vec<TirExpr> = Vec::with_capacity(lowered.len());
            for (flat_val, &target_vt) in lowered.iter().zip(flat_return_types.iter()) {
                let target_type = cm_val_type_to_type_id(target_vt);
                let source_type = cm_val_type_to_type_id(flat_val.cm_type);
                let mut arg = local_ref(flat_val.index, "__flat", source_type);
                if flat_val.cm_type != target_vt {
                    arg = cast(arg, target_type);
                }
                task_return_args.push(arg);
            }
            stmts.push(expr_stmt(cm_raw_call(
                "task-return",
                task_return_args,
                TypeTable::UNIT,
            )));
        }
    }

    stmts
}
