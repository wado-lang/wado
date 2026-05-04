//! Post-synthesis type fixups and call-site rewrites.
//!
//! After import-adapter synthesis the binding bodies use WASI-derived
//! `TypeId`s (e.g. `Array<Tuple<String, Array<u8>>>`) while the call sites
//! see the user's newtype-aliased types (e.g. `Array<Tuple<FieldName,
//! FieldValue>>`). [`rewrite_calls_in_block`] reaches every effect-style
//! call and resource method call, swaps in the binding `FunctionRef`,
//! flattens args to the binding's flat CM parameter shape, and runs
//! [`fixup_wasi_derived_types_in_adapter`] to re-type the binding body.
//! [`collect_effect_calls_in_block`] is the discovery pass that drives
//! the same set of call sites for adapter generation.

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::Type;
use crate::component_model::{WasiFunctionInfo, WasiRegistry};
use crate::hashmap::{IndexMap, IndexSet};
use crate::name::ModuleSource;
use crate::tir::{
    CallArg, FunctionRef, TirBlock, TirExpr, TirExprKind, TirFunction, TirLocal, TirStmt,
    TirStmtKind, TirTemplatePart, TypeId, TypeTable,
};

use crate::synthesis::common::{cast, i32_const, option_none, synth_span};

use super::types::{
    flatten_param_type, is_gc_passthrough_param, is_wasm_flat_type, wasi_type_to_type_id,
};

/// Recursively replace WASI-derived types with user types in the binding.
/// Given a WASI AST `Type` and the user's `TypeId`, compute the WASI-derived `TypeId`
/// and replace it, then recurse into sub-types (Array elements, Tuple fields, etc.).
fn replace_wasi_derived_type_recursive(
    adapter: &mut TirFunction,
    wasi_type: &Type,
    user_type: TypeId,
    wasi_registry: &WasiRegistry,
    wasi_package: &str,
    type_table: &RefCell<TypeTable>,
) {
    let old_type = {
        let mut tt = type_table.borrow_mut();
        wasi_type_to_type_id(wasi_type, &mut tt, wasi_registry, wasi_package)
    };
    if old_type != user_type && old_type != TypeTable::I32 && old_type != TypeTable::UNIT {
        // Skip replacement if the user type resolves to the same base type
        // after deep newtype resolution. Introducing newtypes into adapter
        // bodies creates monomorphization and WIR type lookup issues.
        let tt = type_table.borrow();
        let old_name = tt.mangle_type_name(old_type);
        let user_resolved_name = tt.mangle_type_name_resolving_newtypes(user_type);
        drop(tt);
        if old_name == user_resolved_name {
            // Same underlying type after resolving newtypes — no replacement needed
        } else {
            let tt = type_table.borrow();
            let new_name = tt.mangle_type_name(user_type);
            drop(tt);
            if old_name == new_name {
                replace_type_in_adapter(adapter, old_type, user_type);
            } else {
                replace_type_in_adapter_with_names(
                    adapter, old_type, user_type, &old_name, &new_name,
                );
            }
        }
    }
    match wasi_type {
        Type::Generic(g) if g.name == "Array" && g.args.len() == 1 => {
            let tt = type_table.borrow();
            if let Some(new_elem_args) = tt.generic_type_args(user_type)
                && new_elem_args.len() == 1
            {
                let new_elem = new_elem_args[0];
                drop(tt);
                replace_wasi_derived_type_recursive(
                    adapter,
                    &g.args[0],
                    new_elem,
                    wasi_registry,
                    wasi_package,
                    type_table,
                );
            }
        }
        Type::Tuple(elems) => {
            // Get the user's tuple field types
            if let Some(user_elems) = type_table.borrow().as_tuple(user_type) {
                for (wasi_elem, &user_elem) in elems.iter().zip(user_elems.iter()) {
                    replace_wasi_derived_type_recursive(
                        adapter,
                        wasi_elem,
                        user_elem,
                        wasi_registry,
                        wasi_package,
                        type_table,
                    );
                }
            }
        }
        Type::Generic(g) if g.name == "Option" && g.args.len() == 1 => {
            let tt = type_table.borrow();
            if let Some(new_args) = tt.generic_type_args(user_type)
                && new_args.len() == 1
            {
                let new_inner = new_args[0];
                drop(tt);
                replace_wasi_derived_type_recursive(
                    adapter,
                    &g.args[0],
                    new_inner,
                    wasi_registry,
                    wasi_package,
                    type_table,
                );
            }
        }
        Type::Generic(g) if g.name == "Result" && g.args.len() == 2 => {
            let tt = type_table.borrow();
            if let Some(new_args) = tt.generic_type_args(user_type)
                && new_args.len() == 2
            {
                let new_ok = new_args[0];
                let new_err = new_args[1];
                drop(tt);
                replace_wasi_derived_type_recursive(
                    adapter,
                    &g.args[0],
                    new_ok,
                    wasi_registry,
                    wasi_package,
                    type_table,
                );
                replace_wasi_derived_type_recursive(
                    adapter,
                    &g.args[1],
                    new_err,
                    wasi_registry,
                    wasi_package,
                    type_table,
                );
            }
        }
        _ => {}
    }
}

/// Fix up WASI-derived types in the binding body to match the user's types.
///
/// The binding body uses `TypeIds` from `wasi_type_to_type_id` (e.g., `Array<Tuple<String, Array<u8>>>`).
/// The call site uses user types with newtype aliases (e.g., `Array<Tuple<FieldName, FieldValue>>`).
/// This function computes the WASI-derived `TypeId` for each param and replaces it in the body.
fn fixup_wasi_derived_types_in_adapter(
    adapter: &mut TirFunction,
    func_info: &WasiFunctionInfo,
    call_args: &[TirExpr],
    user_return_type: TypeId,
    type_table: &RefCell<TypeTable>,
    wasi_registry: &WasiRegistry,
    skip_self: bool,
) {
    let wasi_package = func_info.package.as_str();
    let params_iter: Box<dyn Iterator<Item = &(String, String, Type)>> = if skip_self {
        Box::new(func_info.params.iter().skip(1))
    } else {
        Box::new(func_info.params.iter())
    };
    for (i, (_param_name, _, param_type)) in params_iter.enumerate() {
        if i >= call_args.len() {
            break;
        }
        let new_type = call_args[i].type_id;
        // Resolve newtypes (e.g., FieldName → String) before computing WASI-derived TypeId
        let resolved = wasi_registry.resolve_type(param_type);
        replace_wasi_derived_type_recursive(
            adapter,
            &resolved,
            new_type,
            wasi_registry,
            wasi_package,
            type_table,
        );
    }
    // Also fix up return type's WASI-derived sub-types — but skip for CM bindings
    // that return non-flat types (tuples, variants). Their return TypeId was set
    // precisely by synthesis and should not be modified by the recursive type
    // replacement, which can produce different TypeIds for the same logical type
    // through different wasi_type_to_type_id resolution paths.
    if !adapter.is_cm_binding
        && let Some(return_type) = &func_info.return_type
    {
        let resolved = wasi_registry.resolve_type(return_type);
        replace_wasi_derived_type_recursive(
            adapter,
            &resolved,
            user_return_type,
            wasi_registry,
            wasi_package,
            type_table,
        );
    }
}

/// Fix up the return expression's type in the binding body to match the caller's
/// expected return type. The binding was created with placeholder `TypeId`s
/// (e.g., `TypeTable::I32`) that need to be corrected to actual Wado types.
fn fixup_return_type_in_body(adapter: &mut TirFunction, old_type: TypeId, new_type: TypeId) {
    if let Some(body) = &mut adapter.body {
        fixup_types_in_block(body, old_type, new_type, &mut adapter.locals);
    }
}

/// Replace ALL occurrences of `old_type` with `new_type` throughout the binding's
/// body, locals, and params. Used when a param or return type is fixed up from
/// WASI-derived types to the user code's newtype aliases.
fn replace_type_in_adapter(adapter: &mut TirFunction, old_type: TypeId, new_type: TypeId) {
    if old_type == new_type {
        return;
    }
    // Don't replace the return type of CM binding adapters.
    // The return type was set by synthesis with precise TypeIds from the entry
    // module's TypeTable. Replacing it with a TypeId computed by wasi_type_to_type_id
    // (which may produce different TypeIds for Stream/Future/Result composition)
    // corrupts the type and causes WIR build failures.
    if !adapter.is_cm_binding && adapter.return_type == old_type {
        adapter.return_type = new_type;
    }
    // Fix params
    for param in &mut adapter.params {
        if param.type_id == old_type {
            param.type_id = new_type;
        }
    }
    // Fix locals
    for lt in &mut adapter.locals {
        if lt.type_id == old_type {
            lt.type_id = new_type;
        }
    }
    // Fix body
    if let Some(body) = &mut adapter.body {
        replace_type_in_block(body, old_type, new_type);
    }
}

/// Like `replace_type_in_adapter` but also renames function references that
/// contain the old type name to use the new type name. This is needed when
/// the binding body calls monomorphized functions like `Array<T>::with_capacity`
/// where T is a WASI-derived type that differs from the user's newtype alias.
fn replace_type_in_adapter_with_names(
    adapter: &mut TirFunction,
    old_type: TypeId,
    new_type: TypeId,
    old_name: &str,
    new_name: &str,
) {
    if old_type == new_type {
        return;
    }
    // Don't replace return type of CM binding adapters (same as replace_type_in_adapter)
    if !adapter.is_cm_binding && adapter.return_type == old_type {
        adapter.return_type = new_type;
    }
    // Fix params
    for param in &mut adapter.params {
        if param.type_id == old_type {
            param.type_id = new_type;
        }
    }
    for lt in &mut adapter.locals {
        if lt.type_id == old_type {
            lt.type_id = new_type;
        }
    }
    if let Some(body) = &mut adapter.body {
        replace_type_and_names_in_block(body, old_type, new_type, old_name, new_name);
    }
}

fn replace_type_and_names_in_block(
    block: &mut TirBlock,
    old_type: TypeId,
    new_type: TypeId,
    old_name: &str,
    new_name: &str,
) {
    for stmt in &mut block.stmts {
        replace_type_and_names_in_stmt(stmt, old_type, new_type, old_name, new_name);
    }
}

fn replace_type_and_names_in_stmt(
    stmt: &mut TirStmt,
    old_type: TypeId,
    new_type: TypeId,
    old_name: &str,
    new_name: &str,
) {
    match &mut stmt.kind {
        TirStmtKind::Expr(e) => {
            replace_type_and_names_in_expr(e, old_type, new_type, old_name, new_name);
        }
        TirStmtKind::Return { value: Some(e), .. } => {
            replace_type_and_names_in_expr(e, old_type, new_type, old_name, new_name);
        }
        TirStmtKind::Let { value, type_id, .. } => {
            if *type_id == old_type {
                *type_id = new_type;
            }
            replace_type_and_names_in_expr(value, old_type, new_type, old_name, new_name);
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
            ..
        } => {
            replace_type_and_names_in_expr(condition, old_type, new_type, old_name, new_name);
            replace_type_and_names_in_block(then_block, old_type, new_type, old_name, new_name);
            if let Some(eb) = else_block {
                replace_type_and_names_in_block(eb, old_type, new_type, old_name, new_name);
            }
        }
        TirStmtKind::Loop { body, .. } => {
            replace_type_and_names_in_block(body, old_type, new_type, old_name, new_name);
        }
        _ => {}
    }
}

fn replace_type_and_names_in_expr(
    expr: &mut TirExpr,
    old_type: TypeId,
    new_type: TypeId,
    old_name: &str,
    new_name: &str,
) {
    if expr.type_id == old_type {
        expr.type_id = new_type;
    }
    match &mut expr.kind {
        TirExprKind::Call { func, args, .. } => {
            if func.name.contains(old_name) {
                func.name = func.name.replace(old_name, new_name);
            }
            if let Some(ref mut mono) = func.monomorph_info {
                for ta in &mut mono.impl_type_args {
                    if *ta == old_type {
                        *ta = new_type;
                    }
                }
                for ta in &mut mono.method_type_args {
                    if *ta == old_type {
                        *ta = new_type;
                    }
                }
            }
            for arg in args {
                replace_type_and_names_in_expr(
                    &mut arg.expr,
                    old_type,
                    new_type,
                    old_name,
                    new_name,
                );
            }
        }
        TirExprKind::MethodCall {
            func,
            receiver,
            args,
            ..
        } => {
            if func.name.contains(old_name) {
                func.name = func.name.replace(old_name, new_name);
            }
            if let Some(ref mut mono) = func.monomorph_info {
                for ta in &mut mono.impl_type_args {
                    if *ta == old_type {
                        *ta = new_type;
                    }
                }
                for ta in &mut mono.method_type_args {
                    if *ta == old_type {
                        *ta = new_type;
                    }
                }
            }
            replace_type_and_names_in_expr(receiver, old_type, new_type, old_name, new_name);
            for arg in args {
                replace_type_and_names_in_expr(
                    &mut arg.expr,
                    old_type,
                    new_type,
                    old_name,
                    new_name,
                );
            }
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            replace_type_and_names_in_expr(inner, old_type, new_type, old_name, new_name);
        }
        TirExprKind::Assign { target, value } => {
            replace_type_and_names_in_expr(target, old_type, new_type, old_name, new_name);
            replace_type_and_names_in_expr(value, old_type, new_type, old_name, new_name);
        }
        TirExprKind::Binary { left, right, .. } => {
            replace_type_and_names_in_expr(left, old_type, new_type, old_name, new_name);
            replace_type_and_names_in_expr(right, old_type, new_type, old_name, new_name);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            replace_type_and_names_in_expr(inner, old_type, new_type, old_name, new_name);
        }
        TirExprKind::VariantConstruct {
            variant_type,
            payload,
            ..
        } => {
            if *variant_type == old_type {
                *variant_type = new_type;
            }
            if let Some(p) = payload {
                replace_type_and_names_in_expr(p, old_type, new_type, old_name, new_name);
            }
        }
        TirExprKind::Block(blk) => {
            replace_type_and_names_in_block(blk, old_type, new_type, old_name, new_name);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for f in fields {
                replace_type_and_names_in_expr(
                    &mut f.value,
                    old_type,
                    new_type,
                    old_name,
                    new_name,
                );
            }
        }
        _ => {}
    }
}

fn replace_type_in_block(block: &mut TirBlock, old_type: TypeId, new_type: TypeId) {
    for stmt in &mut block.stmts {
        replace_type_in_stmt(stmt, old_type, new_type);
    }
}

fn replace_type_in_stmt(stmt: &mut TirStmt, old_type: TypeId, new_type: TypeId) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, type_id, .. } => {
            if *type_id == old_type {
                *type_id = new_type;
            }
            replace_type_in_expr(value, old_type, new_type);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                replace_type_in_expr(v, old_type, new_type);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            replace_type_in_expr(condition, old_type, new_type);
            replace_type_in_block(then_block, old_type, new_type);
            if let Some(blk) = else_block {
                replace_type_in_block(blk, old_type, new_type);
            }
        }
        TirStmtKind::Loop { body } => {
            replace_type_in_block(body, old_type, new_type);
        }
        TirStmtKind::Expr(expr) => {
            replace_type_in_expr(expr, old_type, new_type);
        }
        _ => {}
    }
}

fn replace_type_in_expr(expr: &mut TirExpr, old_type: TypeId, new_type: TypeId) {
    if expr.type_id == old_type {
        expr.type_id = new_type;
    }
    match &mut expr.kind {
        TirExprKind::Call { func, args, .. } => {
            if let Some(ref mut mono) = func.monomorph_info {
                for ta in &mut mono.impl_type_args {
                    if *ta == old_type {
                        *ta = new_type;
                    }
                }
                for ta in &mut mono.method_type_args {
                    if *ta == old_type {
                        *ta = new_type;
                    }
                }
            }
            for arg in args {
                replace_type_in_expr(&mut arg.expr, old_type, new_type);
            }
        }
        TirExprKind::MethodCall {
            func,
            receiver,
            args,
            ..
        } => {
            if let Some(ref mut mono) = func.monomorph_info {
                for ta in &mut mono.impl_type_args {
                    if *ta == old_type {
                        *ta = new_type;
                    }
                }
                for ta in &mut mono.method_type_args {
                    if *ta == old_type {
                        *ta = new_type;
                    }
                }
            }
            replace_type_in_expr(receiver, old_type, new_type);
            for arg in args {
                replace_type_in_expr(&mut arg.expr, old_type, new_type);
            }
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            replace_type_in_expr(inner, old_type, new_type);
        }
        TirExprKind::Assign { target, value } => {
            replace_type_in_expr(target, old_type, new_type);
            replace_type_in_expr(value, old_type, new_type);
        }
        TirExprKind::Binary { left, right, .. } => {
            replace_type_in_expr(left, old_type, new_type);
            replace_type_in_expr(right, old_type, new_type);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            replace_type_in_expr(inner, old_type, new_type);
        }
        TirExprKind::VariantConstruct {
            variant_type,
            payload,
            ..
        } => {
            if *variant_type == old_type {
                *variant_type = new_type;
            }
            if let Some(p) = payload {
                replace_type_in_expr(p, old_type, new_type);
            }
        }
        TirExprKind::Block(blk) => {
            replace_type_in_block(blk, old_type, new_type);
        }
        _ => {}
    }
}

/// Recursively fix types in a block — replaces `old_type` with `new_type`
/// in return statements, let bindings, and expressions.
/// Also replaces `TypeTable::I32` placeholders with `new_type` (for cases
/// where the binding used I32 as a placeholder).
fn fixup_types_in_block(
    block: &mut TirBlock,
    old_type: TypeId,
    new_type: TypeId,
    locals: &mut Vec<TirLocal>,
) {
    for stmt in &mut block.stmts {
        match &mut stmt.kind {
            TirStmtKind::Return {
                value: Some(ret_expr),
            } => {
                fixup_expr_type(ret_expr, old_type, new_type);
            }
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                fixup_types_in_block(then_block, old_type, new_type, locals);
                if let Some(blk) = else_block {
                    fixup_types_in_block(blk, old_type, new_type, locals);
                }
            }
            TirStmtKind::Loop { body } => {
                fixup_types_in_block(body, old_type, new_type, locals);
            }
            TirStmtKind::Let {
                value,
                local_index,
                type_id,
                ..
            } => {
                let idx = *local_index;
                fixup_adapter_let(value, idx, old_type, new_type, type_id, locals);
            }
            TirStmtKind::Expr(expr) => {
                fixup_adapter_expr(expr, old_type, new_type);
            }
            _ => {}
        }
    }
}

/// Fix up an expression in a Let statement that might hold adapter intermediate values.
fn fixup_adapter_let(
    expr: &mut TirExpr,
    local_index: u32,
    old_type: TypeId,
    new_type: TypeId,
    let_type_id: &mut TypeId,
    locals: &mut [TirLocal],
) {
    let should_fix = expr.type_id == TypeTable::I32 || expr.type_id == old_type;
    match &mut expr.kind {
        TirExprKind::Call { func, .. } if func.method_info.is_some() => {
            if should_fix {
                expr.type_id = new_type;
                *let_type_id = new_type;
                if (local_index as usize) < locals.len() {
                    locals[local_index as usize].type_id = new_type;
                }
            }
        }
        TirExprKind::Null => {
            if should_fix {
                expr.type_id = new_type;
                *let_type_id = new_type;
                if (local_index as usize) < locals.len() {
                    locals[local_index as usize].type_id = new_type;
                }
            }
        }
        _ => {}
    }
}

/// Fix up an expression statement (e.g., Assign with `VariantConstruct`).
fn fixup_adapter_expr(expr: &mut TirExpr, old_type: TypeId, new_type: TypeId) {
    if let TirExprKind::Assign { target, value } = &mut expr.kind {
        fixup_variant_construct(value, old_type, new_type);
        if target.type_id == TypeTable::I32 || target.type_id == old_type {
            target.type_id = new_type;
        }
    }
}

/// Fix up `VariantConstruct` expressions to use the real type.
fn fixup_variant_construct(expr: &mut TirExpr, old_type: TypeId, new_type: TypeId) {
    if let TirExprKind::VariantConstruct { variant_type, .. } = &mut expr.kind {
        if *variant_type == TypeTable::I32 || *variant_type == old_type {
            *variant_type = new_type;
        }
        if expr.type_id == TypeTable::I32 || expr.type_id == old_type {
            expr.type_id = new_type;
        }
    }
}

/// Recursively fix the `type_id` of an expression and its leaf nodes.
fn fixup_expr_type(expr: &mut TirExpr, old_type: TypeId, new_type: TypeId) {
    if expr.type_id == old_type || expr.type_id == TypeTable::I32 {
        expr.type_id = new_type;
    }
    match &mut expr.kind {
        TirExprKind::TupleLiteral { .. } | TirExprKind::Call { .. } | TirExprKind::Local { .. } => {
        }
        TirExprKind::VariantConstruct { variant_type, .. } => {
            if *variant_type == TypeTable::I32 || *variant_type == old_type {
                *variant_type = new_type;
            }
        }
        _ => {}
    }
}

/// Flatten a Wado-level arg into flat CM ABI args at the call site.
///
/// For multi-flat types like `Option<T>`, the Wado-level arg (e.g., `null`)
/// is expanded into multiple i32 args (discriminant + payload).
fn flatten_arg_for_call_site(arg: &TirExpr, flat_tys: &[TypeId], flat_args: &mut Vec<TirExpr>) {
    // Unwrap Cast nodes transparently
    let inner = match &arg.kind {
        TirExprKind::Cast { expr, .. } => expr.as_ref(),
        _ => arg,
    };
    match &inner.kind {
        // null literal → discriminant=0, payload=0 for each flat type
        TirExprKind::Null => {
            for _ in flat_tys {
                flat_args.push(i32_const(0));
            }
        }
        // VariantConstruct None → discriminant=0, payload=0 for each flat type
        TirExprKind::VariantConstruct {
            case_name,
            payload: None,
            ..
        } if case_name == "None" => {
            for _ in flat_tys {
                flat_args.push(i32_const(0));
            }
        }
        // VariantConstruct Some(value) → discriminant=1, then flatten inner value
        TirExprKind::VariantConstruct {
            case_name,
            payload: Some(value),
            ..
        } if case_name == "Some" => {
            flat_args.push(i32_const(1));
            let remaining = &flat_tys[1..];
            if remaining.len() == 1 {
                // Single-value payload: pass through (e.g., enum discriminant)
                flatten_arg_for_call_site(value, remaining, flat_args);
            } else {
                // Multi-value payload (e.g., String → ptr+len): pass through as-is
                // The binding will lower it internally
                flat_args.push((**value).clone());
                for _ in 2..flat_tys.len() {
                    flat_args.push(i32_const(0));
                }
            }
        }
        // For any other expression, this is an arbitrary Option<T> value.
        // Currently not supported — would need runtime null-check logic.
        _ => {
            panic!(
                "StaticCall adapter: cannot flatten arg of kind {:?} into {} flat types at call site; \
                 only null and VariantConstruct literals are supported",
                inner.kind,
                flat_tys.len()
            );
        }
    }
}

/// Collect local type updates from Let stmts that were modified by the rewrite.
/// This is needed because the lower phase pre-populates `locals`, and the streaming
/// adapter rewrite changes Let binding types from Result<..> to i32.
pub(super) fn collect_local_type_updates(
    block: &TirBlock,
    locals: &[TirLocal],
    updates: &mut Vec<(usize, TypeId)>,
) {
    for stmt in &block.stmts {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index,
                type_id,
                ..
            } => {
                let idx = *local_index as usize;
                if idx < locals.len() && locals[idx].type_id != *type_id {
                    updates.push((idx, *type_id));
                }
            }
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
                collect_local_type_updates(then_block, locals, updates);
                if let Some(blk) = else_block {
                    collect_local_type_updates(blk, locals, updates);
                }
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                collect_local_type_updates(body, locals, updates);
            }
            _ => {}
        }
    }
}

pub(super) fn rewrite_calls_in_block(
    block: &mut TirBlock,
    adapters: &IndexMap<String, Rc<RefCell<TirFunction>>>,
    entry_source: &ModuleSource,
    wasi_registry: &WasiRegistry,
    type_table: &Rc<RefCell<TypeTable>>,
) {
    for stmt in &mut block.stmts {
        rewrite_calls_in_stmt(stmt, adapters, entry_source, wasi_registry, type_table);
    }
}

fn rewrite_calls_in_stmt(
    stmt: &mut TirStmt,
    adapters: &IndexMap<String, Rc<RefCell<TirFunction>>>,
    entry_source: &ModuleSource,
    wasi_registry: &WasiRegistry,
    type_table: &Rc<RefCell<TypeTable>>,
) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, type_id, .. } => {
            let old_type = value.type_id;
            rewrite_calls_in_expr(value, adapters, entry_source, wasi_registry, type_table);
            // If the expression type changed (e.g., streaming adapter returns i32
            // instead of Result<(), ErrorCode>), update the let binding's type.
            if value.type_id != old_type {
                *type_id = value.type_id;
            }
        }
        TirStmtKind::Expr(value) => {
            rewrite_calls_in_expr(value, adapters, entry_source, wasi_registry, type_table);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                rewrite_calls_in_expr(v, adapters, entry_source, wasi_registry, type_table);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            rewrite_calls_in_expr(condition, adapters, entry_source, wasi_registry, type_table);
            rewrite_calls_in_block(
                then_block,
                adapters,
                entry_source,
                wasi_registry,
                type_table,
            );
            if let Some(blk) = else_block {
                rewrite_calls_in_block(blk, adapters, entry_source, wasi_registry, type_table);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            rewrite_calls_in_block(body, adapters, entry_source, wasi_registry, type_table);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            rewrite_calls_in_expr(scrutinee, adapters, entry_source, wasi_registry, type_table);
            rewrite_calls_in_block(
                then_block,
                adapters,
                entry_source,
                wasi_registry,
                type_table,
            );
            if let Some(blk) = else_block {
                rewrite_calls_in_block(blk, adapters, entry_source, wasi_registry, type_table);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                rewrite_calls_in_expr(v, adapters, entry_source, wasi_registry, type_table);
            }
        }
        TirStmtKind::LetDestructure { value, .. } => {
            rewrite_calls_in_expr(value, adapters, entry_source, wasi_registry, type_table);
        }
        TirStmtKind::Continue => {}
        TirStmtKind::TaskReturn { value } => {
            rewrite_calls_in_expr(value, adapters, entry_source, wasi_registry, type_table);
        }
        TirStmtKind::VariadicForOf { .. } => {}
    }
}

fn rewrite_calls_in_expr(
    expr: &mut TirExpr,
    adapters: &IndexMap<String, Rc<RefCell<TirFunction>>>,
    entry_source: &ModuleSource,
    wasi_registry: &WasiRegistry,
    type_table: &Rc<RefCell<TypeTable>>,
) {
    // Check if this is an effect-like Call that should be rewritten
    let is_effect_call = matches!(&expr.kind, TirExprKind::Call { func, .. }
        if func.module_source.clone().is_effect_like() && func.module_source.clone().interface_name().is_some());
    if is_effect_call
        && let TirExprKind::Call {
            func,
            args,
            type_args,
            ..
        } = &mut expr.kind
    {
        let interface_name = func
            .module_source
            .clone()
            .interface_name()
            .unwrap_or_default();
        let method_name = func.name.clone();
        let qualified = format!("{interface_name}::{method_name}");

        if let Some(adapter_rc) = adapters.get(&qualified) {
            // Check if this is a streaming async function
            let is_streaming = wasi_registry
                .get_function(&qualified)
                .is_some_and(|f| !f.is_async && f.has_streaming_param());

            // Fix up binding function types from the call site
            {
                let mut adapter = adapter_rc.borrow_mut();
                if is_streaming {
                    // Streaming adapters return i32 (Future handle) at WIR level.
                    // Keep the caller's original Future type for Wado-level type
                    // checking and CM method dispatch (.drop(), .read()).
                    // WIR flattens Future<T> (GenericResource) to i32 automatically.
                } else if adapter.return_type != expr.type_id {
                    let old_return_type = adapter.return_type;
                    adapter.return_type = expr.type_id;
                    fixup_return_type_in_body(&mut adapter, old_return_type, expr.type_id);
                }
                let wasi_func = wasi_registry.get_function(&qualified);
                for (i, arg) in args.iter().enumerate() {
                    if i < adapter.params.len() && adapter.params[i].type_id != arg.expr.type_id {
                        let is_gc_passthrough = wasi_func.is_some_and(|f| {
                            i < f.params.len()
                                && is_gc_passthrough_param(&f.params[i].2, wasi_registry)
                        });
                        if is_streaming && adapter.params[i].type_id == TypeTable::I32 {
                            // Streaming: keep adapter param as i32, cast the arg instead
                        } else if !is_gc_passthrough && is_wasm_flat_type(adapter.params[i].type_id)
                        {
                            // Non-GC flat param: adapter type is authoritative, cast arg.
                        } else {
                            let local_idx = adapter.params[i].local_index as usize;
                            adapter.params[i].type_id = arg.expr.type_id;
                            if local_idx < adapter.locals.len() {
                                adapter.locals[local_idx].type_id = arg.expr.type_id;
                            }
                        }
                    }
                }
            }

            // Cast call-site args to match adapter param types when they differ
            // (e.g., i32 literal → i64 for Duration newtype)
            {
                let adapter = adapter_rc.borrow();
                for (i, arg) in args.iter_mut().enumerate() {
                    if i < adapter.params.len()
                        && adapter.params[i].type_id != arg.expr.type_id
                        && is_wasm_flat_type(adapter.params[i].type_id)
                    {
                        let target = adapter.params[i].type_id;
                        let original = std::mem::replace(
                            &mut arg.expr,
                            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span()),
                        );
                        arg.expr = cast(original, target);
                    }
                }
            }

            // For streaming adapters, cast GC ref args to i32
            if is_streaming {
                for arg in args.iter_mut() {
                    if arg.expr.type_id != TypeTable::I32 {
                        let original = std::mem::replace(
                            &mut arg.expr,
                            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span()),
                        );
                        arg.expr = cast(original, TypeTable::I32);
                    }
                }
            }

            // Rewrite to call the binding function
            *func = FunctionRef::from_resolved(&adapter_rc.borrow(), entry_source.clone());
            *type_args = vec![];

            // Recurse into args
            for arg in args {
                rewrite_calls_in_expr(
                    &mut arg.expr,
                    adapters,
                    entry_source,
                    wasi_registry,
                    type_table,
                );
            }
            return;
        }
    }

    // Check if this is a resource MethodCall that should be rewritten to target a binding
    if let TirExprKind::MethodCall { func, .. } = &expr.kind
        && let Some(method_info) = func.method_info.clone()
    {
        let mut qualified = format!(
            "{}::{}",
            method_info.base_struct_name, method_info.method_name
        );
        // Resolve through type aliases (e.g., Headers -> Fields). Scoped to
        // `wasi:` — the method resolution path is WASI-only.
        if !adapters.contains_key(&qualified)
            && let Some(source) =
                wasi_registry.find_wasi_newtype_source(&method_info.base_struct_name)
            && let Some(Type::Named(resolved)) =
                wasi_registry.get_newtype_by_source(source, &method_info.base_struct_name)
        {
            let aliased = format!("{}::{}", resolved.name, method_info.method_name);
            if adapters.contains_key(&aliased) {
                qualified = aliased;
            }
        }
        if let Some(adapter_rc) = adapters.get(&qualified) {
            // Check if this is a streaming async function
            let is_streaming = wasi_registry
                .get_function(&qualified)
                .is_some_and(|f| !f.is_async && f.has_streaming_param());

            // Extract receiver and args before replacing
            let (taken_receiver, mut taken_args) =
                if let TirExprKind::MethodCall { receiver, args, .. } = &mut expr.kind {
                    (
                        std::mem::replace(
                            receiver.as_mut(),
                            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span()),
                        ),
                        std::mem::take(args),
                    )
                } else {
                    unreachable!()
                };

            // Fix up binding function types from the call site
            // The binding params include self as the first param
            {
                let mut adapter = adapter_rc.borrow_mut();
                if is_streaming {
                    // Streaming adapters return i32 (Future handle) at WIR level.
                    // Keep the caller's original Future type for Wado-level type
                    // checking and CM method dispatch (.drop(), .read()).
                    // WIR flattens Future<T> (GenericResource) to i32 automatically.
                } else if adapter.return_type != expr.type_id {
                    let old_return_type = adapter.return_type;
                    adapter.return_type = expr.type_id;
                    fixup_return_type_in_body(&mut adapter, old_return_type, expr.type_id);
                }
                // Fix up self param (index 0) from the receiver
                if !adapter.params.is_empty() && adapter.params[0].type_id != taken_receiver.type_id
                {
                    let local_idx = adapter.params[0].local_index as usize;
                    adapter.params[0].type_id = taken_receiver.type_id;
                    if local_idx < adapter.locals.len() {
                        adapter.locals[local_idx].type_id = taken_receiver.type_id;
                    }
                }
                // Fix up remaining params from the args
                let method_func = wasi_registry.get_function(&qualified);
                for (i, arg) in taken_args.iter().enumerate() {
                    let param_idx = i + 1; // +1 to skip self
                    if param_idx < adapter.params.len()
                        && adapter.params[param_idx].type_id != arg.expr.type_id
                    {
                        // For method calls, WASI params include self at index 0, so
                        // the i-th arg corresponds to WASI param index i+1
                        let wasi_param_idx = i + 1;
                        let is_gc_passthrough = method_func.is_some_and(|f| {
                            wasi_param_idx < f.params.len()
                                && is_gc_passthrough_param(
                                    &f.params[wasi_param_idx].2,
                                    wasi_registry,
                                )
                        });
                        if is_streaming && adapter.params[param_idx].type_id == TypeTable::I32 {
                            // Streaming: keep adapter param as i32, cast the arg instead
                        } else if !is_gc_passthrough
                            && is_wasm_flat_type(adapter.params[param_idx].type_id)
                        {
                            // Non-GC flat param: adapter type is authoritative, cast arg.
                        } else {
                            let local_idx = adapter.params[param_idx].local_index as usize;
                            adapter.params[param_idx].type_id = arg.expr.type_id;
                            if local_idx < adapter.locals.len() {
                                adapter.locals[local_idx].type_id = arg.expr.type_id;
                            }
                        }
                    }
                }
                // Replace WASI-derived types in the body (including function names).
                // Skip for CM bindings — their types were set precisely by synthesis
                // and the recursive replacement can produce TypeId mismatches for
                // complex return types like [Stream<T>, Future<Result<_, E>>].
                if !adapter.is_cm_binding
                    && let Some(func_info) = wasi_registry.get_function(&qualified)
                {
                    let call_args: Vec<TirExpr> =
                        taken_args.iter().map(|a| a.expr.clone()).collect();
                    fixup_wasi_derived_types_in_adapter(
                        &mut adapter,
                        func_info,
                        &call_args,
                        expr.type_id,
                        type_table,
                        wasi_registry,
                        true, // skip_self: call_args excludes self
                    );
                }
            }

            // Cast call-site args to match adapter param types when they differ
            {
                let adapter = adapter_rc.borrow();
                for (i, arg) in taken_args.iter_mut().enumerate() {
                    let param_idx = i + 1; // +1 to skip self
                    if param_idx < adapter.params.len()
                        && adapter.params[param_idx].type_id != arg.expr.type_id
                        && is_wasm_flat_type(adapter.params[param_idx].type_id)
                    {
                        let target = adapter.params[param_idx].type_id;
                        let original = std::mem::replace(
                            &mut arg.expr,
                            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span()),
                        );
                        arg.expr = cast(original, target);
                    }
                }
            }

            // Flatten call site args to match the binding's flat CM params.
            // For method calls, self is the first param; remaining args may need flattening.
            let method_func = wasi_registry.get_function(&qualified);
            let flat_taken_args = if let Some(func_info) = method_func {
                let mut flat = Vec::new();
                for (i, arg) in taken_args.iter().enumerate() {
                    // WASI param index: i+1 to skip self
                    let wasi_param_idx = i + 1;
                    if wasi_param_idx >= func_info.params.len() {
                        flat.push(arg.expr.clone());
                        continue;
                    }
                    let param_type = &func_info.params[wasi_param_idx].2;
                    let flat_tys = flatten_param_type(param_type, wasi_registry);
                    if flat_tys.is_empty() {
                        continue;
                    }
                    if is_gc_passthrough_param(param_type, wasi_registry) {
                        if matches!(arg.expr.kind, TirExprKind::Null) {
                            let option_type_id = {
                                let mut tt = type_table.borrow_mut();
                                wasi_type_to_type_id(
                                    param_type,
                                    &mut tt,
                                    wasi_registry,
                                    &func_info.package,
                                )
                            };
                            flat.push(option_none(option_type_id));
                        } else {
                            flat.push(arg.expr.clone());
                        }
                    } else if flat_tys.len() == 1 {
                        flat.push(arg.expr.clone());
                    } else {
                        flatten_arg_for_call_site(&arg.expr, &flat_tys, &mut flat);
                    }
                }
                flat
            } else {
                taken_args.into_iter().map(|a| a.expr).collect()
            };

            // Replace MethodCall with Call targeting the binding
            // Prepend receiver to args
            let mut all_args = vec![taken_receiver];
            all_args.extend(flat_taken_args);

            expr.kind = TirExprKind::Call {
                func: FunctionRef::from_resolved(&adapter_rc.borrow(), entry_source.clone()),
                args: all_args
                    .into_iter()
                    .map(|e| CallArg::new(e, false))
                    .collect(),
                type_args: vec![],
            };

            // Recurse into args of the new Call
            if let TirExprKind::Call { args, .. } = &mut expr.kind {
                for arg in args {
                    rewrite_calls_in_expr(
                        &mut arg.expr,
                        adapters,
                        entry_source,
                        wasi_registry,
                        type_table,
                    );
                }
            }
            return;
        }
    }

    // Check if this is a resource static Call (with method_info) that should be rewritten to target a binding
    if let TirExprKind::Call { func, .. } = &expr.kind
        && func.method_info.is_some()
    {
        let func_name = func.name.clone();
        if let Some(adapter_rc) = adapters.get(&func_name) {
            // Look up WASI function info to flatten args at the call site
            let wasi_func_info = wasi_registry.get_function(&func_name).cloned();

            // Extract args before replacing
            let taken_args = if let TirExprKind::Call { args, .. } = &mut expr.kind {
                std::mem::take(args)
                    .into_iter()
                    .map(|a| a.expr)
                    .collect::<Vec<_>>()
            } else {
                unreachable!()
            };

            // Fix up adapter return type and param types from the call site
            {
                let mut adapter = adapter_rc.borrow_mut();
                if adapter.return_type != expr.type_id {
                    let old_return_type = adapter.return_type;
                    adapter.return_type = expr.type_id;
                    fixup_return_type_in_body(&mut adapter, old_return_type, expr.type_id);
                }
                // Fix up adapter param types for GC pass-through params (String,
                // Array<T>) where the binding receives the GC value directly.
                // Do NOT fix up params that get flattened (Option, resource handles)
                // because taken_args indices don't match flat adapter param indices.
                if let Some(func_info) = &wasi_func_info {
                    let mut flat_idx = 0;
                    for (i, (_name, _, param_type)) in func_info.params.iter().enumerate() {
                        let is_gc_passthrough = matches!(
                            param_type,
                            Type::Named(n) if n.name == "String"
                        ) || matches!(
                            param_type,
                            Type::Generic(g) if g.name == "Array" && g.args.len() == 1
                        );
                        if is_gc_passthrough {
                            if flat_idx < adapter.params.len()
                                && i < taken_args.len()
                                && adapter.params[flat_idx].type_id != taken_args[i].type_id
                            {
                                let local_idx = adapter.params[flat_idx].local_index as usize;
                                adapter.params[flat_idx].type_id = taken_args[i].type_id;
                                if local_idx < adapter.locals.len() {
                                    adapter.locals[local_idx].type_id = taken_args[i].type_id;
                                }
                            }
                            flat_idx += 1;
                        } else {
                            let flat_tys = flatten_param_type(param_type, wasi_registry);
                            flat_idx += flat_tys.len().max(1);
                        }
                    }
                }
                // Replace WASI-derived types in the body with the user's types.
                // Skip for CM bindings (same reason as above).
                if !adapter.is_cm_binding
                    && let Some(func_info) = &wasi_func_info
                {
                    fixup_wasi_derived_types_in_adapter(
                        &mut adapter,
                        func_info,
                        &taken_args,
                        expr.type_id,
                        type_table,
                        wasi_registry,
                        false, // skip_self: static calls have no self
                    );
                }
            }

            // Flatten call site args to match the binding's flat CM params.
            // GC passthrough types (String, Array<u8>, Option<T>) are passed
            // through as GC refs — the binding body handles lowering.
            // Other multi-flat types are flattened here into individual i32 args.
            let flat_call_args = if let Some(func_info) = &wasi_func_info {
                let mut flat = Vec::new();
                for (i, (_param_name, _, param_type)) in func_info.params.iter().enumerate() {
                    let flat_tys = flatten_param_type(param_type, wasi_registry);
                    if flat_tys.is_empty() || i >= taken_args.len() {
                        continue;
                    }
                    if is_gc_passthrough_param(param_type, wasi_registry) {
                        let arg = &taken_args[i];
                        // Convert bare Null to VariantConstruct None.
                        // Use the binding's param type (from the WASI registry)
                        // to get a properly-resolved type_id, since the
                        // source null's type_id may have unknown inner type.
                        if matches!(arg.kind, TirExprKind::Null) {
                            let option_type_id = {
                                let mut tt = type_table.borrow_mut();
                                wasi_type_to_type_id(
                                    param_type,
                                    &mut tt,
                                    wasi_registry,
                                    &func_info.package,
                                )
                            };
                            flat.push(option_none(option_type_id));
                        } else {
                            flat.push(arg.clone());
                        }
                    } else if flat_tys.len() == 1 {
                        flat.push(taken_args[i].clone());
                    } else {
                        flatten_arg_for_call_site(&taken_args[i], &flat_tys, &mut flat);
                    }
                }
                flat
            } else {
                // No WASI function info: pass args as-is (fallback)
                taken_args
            };

            // Replace static Call with Call targeting the binding
            expr.kind = TirExprKind::Call {
                func: FunctionRef::from_resolved(&adapter_rc.borrow(), entry_source.clone()),
                args: flat_call_args
                    .into_iter()
                    .map(|e| CallArg::new(e, false))
                    .collect(),
                type_args: vec![],
            };

            // Recurse into args of the new Call
            if let TirExprKind::Call { args, .. } = &mut expr.kind {
                for arg in args {
                    rewrite_calls_in_expr(
                        &mut arg.expr,
                        adapters,
                        entry_source,
                        wasi_registry,
                        type_table,
                    );
                }
            }
            return;
        }
    }

    // Recurse into sub-expressions
    match &mut expr.kind {
        TirExprKind::Call { args, .. } => {
            for arg in args {
                rewrite_calls_in_expr(
                    &mut arg.expr,
                    adapters,
                    entry_source,
                    wasi_registry,
                    type_table,
                );
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                rewrite_calls_in_expr(arg, adapters, entry_source, wasi_registry, type_table);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            rewrite_calls_in_expr(receiver, adapters, entry_source, wasi_registry, type_table);
            for arg in args {
                rewrite_calls_in_expr(
                    &mut arg.expr,
                    adapters,
                    entry_source,
                    wasi_registry,
                    type_table,
                );
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            rewrite_calls_in_expr(callee, adapters, entry_source, wasi_registry, type_table);
            for arg in args {
                rewrite_calls_in_expr(arg, adapters, entry_source, wasi_registry, type_table);
            }
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            rewrite_calls_in_block(block, adapters, entry_source, wasi_registry, type_table);
        }
        TirExprKind::Binary { left, right, .. } => {
            rewrite_calls_in_expr(left, adapters, entry_source, wasi_registry, type_table);
            rewrite_calls_in_expr(right, adapters, entry_source, wasi_registry, type_table);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. } => {
            rewrite_calls_in_expr(inner, adapters, entry_source, wasi_registry, type_table);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            rewrite_calls_in_expr(condition, adapters, entry_source, wasi_registry, type_table);
            rewrite_calls_in_block(
                then_branch,
                adapters,
                entry_source,
                wasi_registry,
                type_table,
            );
            if let Some(blk) = else_branch {
                rewrite_calls_in_block(blk, adapters, entry_source, wasi_registry, type_table);
            }
        }
        TirExprKind::Index { expr: e, index }
        | TirExprKind::Assign {
            target: e,
            value: index,
        } => {
            rewrite_calls_in_expr(e, adapters, entry_source, wasi_registry, type_table);
            rewrite_calls_in_expr(index, adapters, entry_source, wasi_registry, type_table);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            rewrite_calls_in_expr(scrutinee, adapters, entry_source, wasi_registry, type_table);
            for arm in arms {
                rewrite_calls_in_block(arm, adapters, entry_source, wasi_registry, type_table);
            }
            rewrite_calls_in_block(default, adapters, entry_source, wasi_registry, type_table);
        }
        TirExprKind::Match {
            expr: scrutinee,
            arms,
        } => {
            rewrite_calls_in_expr(scrutinee, adapters, entry_source, wasi_registry, type_table);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    rewrite_calls_in_expr(guard, adapters, entry_source, wasi_registry, type_table);
                }
                rewrite_calls_in_expr(
                    &mut arm.body,
                    adapters,
                    entry_source,
                    wasi_registry,
                    type_table,
                );
            }
        }
        TirExprKind::Closure { body, .. } => {
            rewrite_calls_in_expr(body, adapters, entry_source, wasi_registry, type_table);
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            rewrite_calls_in_expr(functor, adapters, entry_source, wasi_registry, type_table);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in &mut fields.iter_mut() {
                rewrite_calls_in_expr(
                    &mut field.value,
                    adapters,
                    entry_source,
                    wasi_registry,
                    type_table,
                );
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            rewrite_calls_in_expr(value, adapters, entry_source, wasi_registry, type_table);
        }
        TirExprKind::WithHandler { bindings, body, .. } => {
            // Walk the handler value and the do-block body so calls
            // inside `with E => h do { ... }` get the same adapter-binding
            // rewrite the rest of the function gets. The effect-dispatch
            // synthesis pass that runs next consumes
            // `__cm_binding__<E>_<op>` Calls and routes them through
            // dispatch wrappers.
            for binding in bindings {
                rewrite_calls_in_expr(
                    &mut binding.handler,
                    adapters,
                    entry_source,
                    wasi_registry,
                    type_table,
                );
            }
            rewrite_calls_in_block(body, adapters, entry_source, wasi_registry, type_table);
        }
        TirExprKind::Resume { value } => {
            rewrite_calls_in_expr(value, adapters, entry_source, wasi_registry, type_table);
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                rewrite_calls_in_expr(elem, adapters, entry_source, wasi_registry, type_table);
            }
        }
        TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
            rewrite_calls_in_expr(inner, adapters, entry_source, wasi_registry, type_table);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                rewrite_calls_in_expr(
                    payload_expr,
                    adapters,
                    entry_source,
                    wasi_registry,
                    type_table,
                );
            }
        }
        TirExprKind::TemplateString { parts } => {
            for part in parts {
                if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                    rewrite_calls_in_expr(inner, adapters, entry_source, wasi_registry, type_table);
                }
            }
        }
        // Leaf nodes — no sub-expressions. Enumerated explicitly so a new
        // `TirExprKind` variant added in tir.rs forces this match to be
        // updated rather than silently no-op'd by a catch-all.
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Local { .. }
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Capture { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::EnumConstruct { .. } => {}
    }
}

pub(super) fn collect_effect_calls_in_block(
    block: &TirBlock,
    effects: &mut IndexSet<String>,
    wasi_registry: &WasiRegistry,
) {
    for stmt in &block.stmts {
        collect_effect_calls_in_stmt(stmt, effects, wasi_registry);
    }
}

fn collect_effect_calls_in_stmt(
    stmt: &TirStmt,
    effects: &mut IndexSet<String>,
    wasi_registry: &WasiRegistry,
) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } | TirStmtKind::Expr(value) => {
            collect_effect_calls_in_expr(value, effects, wasi_registry);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_effect_calls_in_expr(v, effects, wasi_registry);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_effect_calls_in_expr(condition, effects, wasi_registry);
            collect_effect_calls_in_block(then_block, effects, wasi_registry);
            if let Some(blk) = else_block {
                collect_effect_calls_in_block(blk, effects, wasi_registry);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            collect_effect_calls_in_block(body, effects, wasi_registry);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_effect_calls_in_expr(scrutinee, effects, wasi_registry);
            collect_effect_calls_in_block(then_block, effects, wasi_registry);
            if let Some(blk) = else_block {
                collect_effect_calls_in_block(blk, effects, wasi_registry);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_effect_calls_in_expr(v, effects, wasi_registry);
            }
        }
        TirStmtKind::LetDestructure { value, .. } => {
            collect_effect_calls_in_expr(value, effects, wasi_registry);
        }
        TirStmtKind::Continue => {}
        TirStmtKind::TaskReturn { value } => {
            collect_effect_calls_in_expr(value, effects, wasi_registry);
        }
        TirStmtKind::VariadicForOf { .. } => {}
    }
}

fn collect_effect_calls_in_expr(
    expr: &TirExpr,
    effects: &mut IndexSet<String>,
    wasi_registry: &WasiRegistry,
) {
    match &expr.kind {
        TirExprKind::Call { func, args, .. } => {
            // Collect effect-like Call nodes (sync WASI calls like Environment::get_arguments)
            if func.module_source.clone().is_effect_like()
                && let Some(interface_name) = func.module_source.clone().interface_name()
            {
                let method_name = func.name.clone();
                let qualified = format!("{interface_name}::{method_name}");
                if wasi_registry.get_function(&qualified).is_some() {
                    effects.insert(qualified);
                }
            }
            // Also check if this is a WASI resource static method call (e.g., Response::new)
            if func.method_info.is_some() {
                let func_name = func.name.clone();
                if wasi_registry.get_function(&func_name).is_some() {
                    effects.insert(func_name);
                }
            }
            for arg in args {
                collect_effect_calls_in_expr(&arg.expr, effects, wasi_registry);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_effect_calls_in_expr(arg, effects, wasi_registry);
            }
        }
        TirExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } => {
            // Check if this is a WASI resource method call
            if let Some(method_info) = func.method_info.clone() {
                let qualified = format!(
                    "{}::{}",
                    method_info.base_struct_name, method_info.method_name
                );
                if wasi_registry.get_function(&qualified).is_some() {
                    effects.insert(qualified);
                } else if let Some(source) =
                    wasi_registry.find_wasi_newtype_source(&method_info.base_struct_name)
                    && let Some(Type::Named(resolved)) =
                        wasi_registry.get_newtype_by_source(source, &method_info.base_struct_name)
                {
                    // Resolve through type aliases (e.g., Headers -> Fields)
                    let aliased = format!("{}::{}", resolved.name, method_info.method_name);
                    if wasi_registry.get_function(&aliased).is_some() {
                        effects.insert(aliased);
                    }
                }
            }
            collect_effect_calls_in_expr(receiver, effects, wasi_registry);
            for arg in args {
                collect_effect_calls_in_expr(&arg.expr, effects, wasi_registry);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            collect_effect_calls_in_expr(callee, effects, wasi_registry);
            for arg in args {
                collect_effect_calls_in_expr(arg, effects, wasi_registry);
            }
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            collect_effect_calls_in_block(block, effects, wasi_registry);
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_effect_calls_in_expr(left, effects, wasi_registry);
            collect_effect_calls_in_expr(right, effects, wasi_registry);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. } => {
            collect_effect_calls_in_expr(inner, effects, wasi_registry);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_effect_calls_in_expr(condition, effects, wasi_registry);
            collect_effect_calls_in_block(then_branch, effects, wasi_registry);
            if let Some(blk) = else_branch {
                collect_effect_calls_in_block(blk, effects, wasi_registry);
            }
        }
        TirExprKind::Index { expr: e, index }
        | TirExprKind::Assign {
            target: e,
            value: index,
        } => {
            collect_effect_calls_in_expr(e, effects, wasi_registry);
            collect_effect_calls_in_expr(index, effects, wasi_registry);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_effect_calls_in_expr(scrutinee, effects, wasi_registry);
            for arm in arms {
                collect_effect_calls_in_block(arm, effects, wasi_registry);
            }
            collect_effect_calls_in_block(default, effects, wasi_registry);
        }
        TirExprKind::Match {
            expr: scrutinee,
            arms,
        } => {
            collect_effect_calls_in_expr(scrutinee, effects, wasi_registry);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_effect_calls_in_expr(guard, effects, wasi_registry);
                }
                collect_effect_calls_in_expr(&arm.body, effects, wasi_registry);
            }
        }
        TirExprKind::Closure { body, .. } => {
            collect_effect_calls_in_expr(body, effects, wasi_registry);
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            collect_effect_calls_in_expr(functor, effects, wasi_registry);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_effect_calls_in_expr(&field.value, effects, wasi_registry);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            collect_effect_calls_in_expr(value, effects, wasi_registry);
        }
        TirExprKind::WithHandler { bindings, body, .. } => {
            // Walk the handler value and the do-block body so effect calls
            // reached only from inside `with E => h do { ... }` (which the
            // effect-dispatch synthesis later routes through wrapper
            // functions whose else-branch falls back to the CM-binding
            // adapter) still trigger adapter generation here.
            for binding in bindings {
                collect_effect_calls_in_expr(&binding.handler, effects, wasi_registry);
            }
            collect_effect_calls_in_block(body, effects, wasi_registry);
        }
        TirExprKind::Resume { value } => {
            collect_effect_calls_in_expr(value, effects, wasi_registry);
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                collect_effect_calls_in_expr(elem, effects, wasi_registry);
            }
        }
        TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
            collect_effect_calls_in_expr(inner, effects, wasi_registry);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                collect_effect_calls_in_expr(payload_expr, effects, wasi_registry);
            }
        }
        TirExprKind::TemplateString { parts } => {
            for part in parts {
                if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                    collect_effect_calls_in_expr(inner, effects, wasi_registry);
                }
            }
        }
        // Leaf nodes — no sub-expressions. Enumerated explicitly so a new
        // `TirExprKind` variant added in tir.rs forces this match to be
        // updated rather than silently no-op'd by a catch-all.
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Local { .. }
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Capture { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::EnumConstruct { .. } => {}
    }
}
