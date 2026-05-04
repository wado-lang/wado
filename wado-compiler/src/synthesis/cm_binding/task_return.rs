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
use crate::name::ModuleSource;
use crate::tir::{
    ResolvedType, TirBlock, TirExpr, TirFunction, TirLocal, TirModule, TirStmt, TirStmtKind,
    TypeTable,
};

use crate::synthesis::common::{
    alloc_local, assign, block, cast, cm_raw_call, expr_stmt, i32_const, if_stmt, let_mut_stmt,
    let_stmt, local_ref,
};

use super::export_adapter::{synthesize_lower_to_flat, synthesize_variant_lower_to_flat};
use super::types::{
    LiftContext, cm_val_type_to_type_id, cm_zero, find_variant_decl, flat_types_from_type_id,
    variant_payload, variant_test,
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
    interner: &RefCell<crate::name::ModuleSourceInterner>,
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
    interner: &RefCell<crate::name::ModuleSourceInterner>,
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
    interner: &RefCell<crate::name::ModuleSourceInterner>,
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
/// For other types, generates task-return(0, ...`flat_values`).
fn generate_inline_task_return(
    value: TirExpr,
    flat_return_types: &[cm_abi::CmValType],
    next_local: &mut u32,
    locals: &mut Vec<TirLocal>,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table: &Rc<RefCell<TypeTable>>,
    wasi_registry: &WasiRegistry,
    cm_package: &str,
    interner: &RefCell<crate::name::ModuleSourceInterner>,
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

        // === Ok case ===
        let mut ok_stmts: Vec<TirStmt> = Vec::new();
        ok_stmts.push(expr_stmt(assign(
            local_ref(
                flat_locals[0].0,
                &flat_locals[0].1,
                cm_val_type_to_type_id(flat_return_types[0]),
            ),
            i32_const(0),
        )));
        let ok_value = variant_payload(
            local_ref(result_local, "__task_ret", value_type_id),
            0,
            ok_type_id,
        );
        let tt = type_table.borrow();
        let ok_flat_types = flat_types_from_type_id(ok_type_id, tir_modules, &tt);
        drop(tt);
        if !ok_flat_types.is_empty() {
            let ok_local = alloc_local(next_local, locals, ok_type_id);
            ok_stmts.push(let_stmt("__ok_val", ok_local, ok_type_id, ok_value));
            let ok_lowered = synthesize_lower_to_flat(
                local_ref(ok_local, "__ok_val", ok_type_id),
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

        // === Err case ===
        let mut err_stmts: Vec<TirStmt> = Vec::new();
        err_stmts.push(expr_stmt(assign(
            local_ref(
                flat_locals[0].0,
                &flat_locals[0].1,
                cm_val_type_to_type_id(flat_return_types[0]),
            ),
            i32_const(1),
        )));
        let err_value = variant_payload(
            local_ref(result_local, "__task_ret", value_type_id),
            1,
            err_type_id,
        );
        let err_local = alloc_local(next_local, locals, err_type_id);
        err_stmts.push(let_stmt("__err_val", err_local, err_type_id, err_value));
        let err_resolved = type_table.borrow().get(err_type_id).clone();
        if let ResolvedType::Variant { name, .. } = &err_resolved {
            if let Some(variant_decl) = find_variant_decl(name, tir_modules) {
                synthesize_variant_lower_to_flat(
                    err_local,
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
                    local_ref(err_local, "__err_val", err_type_id),
                )));
            }
        } else {
            let err_lowered = synthesize_lower_to_flat(
                local_ref(err_local, "__err_val", err_type_id),
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

        // Combine Ok/Err branches
        stmts.push(if_stmt(
            variant_test(
                local_ref(result_local, "__task_ret", value_type_id),
                0,
                "Ok",
            ),
            block(ok_stmts),
            Some(block(err_stmts)),
        ));
    } else {
        drop(tt);
        // Non-Result (or empty flat types): just emit task-return(0)
        stmts.push(expr_stmt(cm_raw_call(
            "task-return",
            vec![i32_const(0)],
            TypeTable::UNIT,
        )));
    }

    stmts
}
