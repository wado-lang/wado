//! Template String Buffer Hoisting for Loops
//!
//! When a template string (`__tmpl` labeled block) appears inside a loop,
//! this pass hoists the `String::with_capacity` allocation before the loop
//! and reuses the backing array across iterations.
//!
//! **Before:**
//! ```text
//! loop {
//!     let s = __tmpl: {
//!         let mut __r = String { repr: array_new(N), used: 0 };
//!         __r.append(...);
//!         break __tmpl: __r;
//!     };
//!     s.len();   // s only used as method receiver
//! }
//! ```
//!
//! **After:**
//! ```text
//! let __tmpl_repr_0 = builtin::array_new::<u8>(N);
//! loop {
//!     let s = __tmpl: {
//!         let mut __r = String { repr: __tmpl_repr_0, used: 0 };
//!         __r.append(...);
//!         __tmpl_repr_0 = __r.repr;   // sync back in case of growth
//!         break __tmpl: __r;
//!     };
//!     s.len();
//! }
//! ```
//!
//! Safety: The optimization shares the backing array between iterations.
//! It is only applied when the template result does not escape the iteration:
//! the result must be bound to a Let variable that is only used as a method
//! receiver (`self`), never passed as a regular function argument.

use crate::name::ModuleSource;
use crate::project::Project;
use crate::tir::{
    TirBlock, TirExpr, TirExprKind, TirFunction, TirStmt, TirStmtKind, TirStructField, TypeId,
    TypeTable,
};
use crate::token::Span;
use indexmap::IndexSet;

/// Apply template string buffer hoisting to all functions in the project.
pub fn hoist_template_buffers(project: &mut Project) -> bool {
    let mut changed = false;
    for module in project.tir_modules.values_mut() {
        let type_table = module.type_table.clone();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            changed |= hoist_in_function(&mut func, &type_table);
        }
    }
    changed
}

fn hoist_in_function(func: &mut TirFunction, type_table: &std::cell::RefCell<TypeTable>) -> bool {
    let Some(ref mut body) = func.body else {
        return false;
    };
    let mut local_count = func.local_count;
    let mut local_types = func.local_types.clone();
    let changed = hoist_in_block(body, &mut local_count, &mut local_types, type_table);
    func.local_count = local_count;
    func.local_types = local_types;
    changed
}

fn hoist_in_block(
    block: &mut TirBlock,
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    type_table: &std::cell::RefCell<TypeTable>,
) -> bool {
    let mut changed = false;
    let mut new_stmts = Vec::new();

    for mut stmt in std::mem::take(&mut block.stmts) {
        match &mut stmt.kind {
            TirStmtKind::Loop { body } => {
                // Recurse into loop body first (for nested loops)
                changed |= hoist_in_block(body, local_count, local_types, type_table);

                // Try to hoist template buffers out of this loop
                let hoist_stmts = hoist_tmpl_from_loop(body, local_count, local_types, type_table);
                if !hoist_stmts.is_empty() {
                    changed = true;
                    new_stmts.extend(hoist_stmts);
                }
                new_stmts.push(stmt);
            }
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                changed |= hoist_in_block(then_block, local_count, local_types, type_table);
                if let Some(eb) = else_block {
                    changed |= hoist_in_block(eb, local_count, local_types, type_table);
                }
                new_stmts.push(stmt);
            }
            TirStmtKind::LabeledBlock { block: inner, .. } => {
                changed |= hoist_in_block(inner, local_count, local_types, type_table);
                new_stmts.push(stmt);
            }
            TirStmtKind::IfPattern {
                then_block,
                else_block,
                ..
            } => {
                changed |= hoist_in_block(then_block, local_count, local_types, type_table);
                if let Some(eb) = else_block {
                    changed |= hoist_in_block(eb, local_count, local_types, type_table);
                }
                new_stmts.push(stmt);
            }
            _ => {
                new_stmts.push(stmt);
            }
        }
    }

    block.stmts = new_stmts;
    changed
}

/// Information about a `__tmpl` block that can be hoisted.
struct TmplCandidate {
    /// Index of the `__r` local in the `__tmpl` block
    buf_local_index: u32,
    /// The capacity argument from `String::with_capacity(N)` / `array_new(N)`
    capacity_expr: TirExpr,
    /// The String type ID
    string_type: TypeId,
    /// The span of the original expression
    span: Span,
}

/// Scan a loop body for `__tmpl` labeled blocks and hoist their buffer allocations.
/// Returns hoisting statements to prepend before the loop.
fn hoist_tmpl_from_loop(
    loop_body: &mut TirBlock,
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    type_table: &std::cell::RefCell<TypeTable>,
) -> Vec<TirStmt> {
    // Phase 1: Collect all Let bindings whose value is a __tmpl LabeledBlock,
    // and check if the bound variable escapes (used as a non-self argument).
    let escaping_locals = collect_escaping_locals(loop_body);

    // Phase 2: Transform safe __tmpl blocks
    let mut hoist_stmts = Vec::new();
    transform_stmts_in_block(
        loop_body,
        &escaping_locals,
        &mut hoist_stmts,
        local_count,
        local_types,
        type_table,
    );
    hoist_stmts
}

/// Collect local indices that "escape" — used as a non-receiver function argument
/// or stored in a struct/array/collection.
fn collect_escaping_locals(block: &TirBlock) -> IndexSet<u32> {
    let mut escaping = IndexSet::new();
    for stmt in &block.stmts {
        collect_escaping_in_stmt(stmt, &mut escaping);
    }
    escaping
}

fn collect_escaping_in_stmt(stmt: &TirStmt, escaping: &mut IndexSet<u32>) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => {
            collect_escaping_in_expr(value, escaping);
        }
        TirStmtKind::Expr(expr) => {
            collect_escaping_in_expr(expr, escaping);
        }
        TirStmtKind::Return { value: Some(expr) } => {
            // Returning a value means it escapes the loop
            collect_local_refs(expr, escaping);
            collect_escaping_in_expr(expr, escaping);
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_escaping_in_expr(condition, escaping);
            for s in &then_block.stmts {
                collect_escaping_in_stmt(s, escaping);
            }
            if let Some(eb) = else_block {
                for s in &eb.stmts {
                    collect_escaping_in_stmt(s, escaping);
                }
            }
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            for s in &block.stmts {
                collect_escaping_in_stmt(s, escaping);
            }
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_escaping_in_expr(scrutinee, escaping);
            for s in &then_block.stmts {
                collect_escaping_in_stmt(s, escaping);
            }
            if let Some(eb) = else_block {
                for s in &eb.stmts {
                    collect_escaping_in_stmt(s, escaping);
                }
            }
        }
        TirStmtKind::Loop { body } => {
            for s in &body.stmts {
                collect_escaping_in_stmt(s, escaping);
            }
        }
        _ => {}
    }
}

/// Mark all locals that appear as non-receiver function arguments as escaping.
fn collect_escaping_in_expr(expr: &TirExpr, escaping: &mut IndexSet<u32>) {
    match &expr.kind {
        // Function call: args (not receiver) escape
        TirExprKind::Call { args, .. } | TirExprKind::StaticCall { args, .. } => {
            for arg in args {
                collect_local_refs(arg, escaping);
                collect_escaping_in_expr(arg, escaping);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            // Receiver (self) doesn't escape — only non-self args escape
            collect_escaping_in_expr(receiver, escaping);
            for arg in args {
                collect_local_refs(arg, escaping);
                collect_escaping_in_expr(arg, escaping);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            collect_escaping_in_expr(callee, escaping);
            for arg in args {
                collect_local_refs(arg, escaping);
                collect_escaping_in_expr(arg, escaping);
            }
        }
        // Assignment to a field means the value escapes
        TirExprKind::Assign { target, value } => {
            if matches!(&target.kind, TirExprKind::FieldAccess { .. }) {
                collect_local_refs(value, escaping);
            }
            collect_escaping_in_expr(target, escaping);
            collect_escaping_in_expr(value, escaping);
        }
        // Struct literal fields: all field values escape
        TirExprKind::StructLiteral { fields, .. } => {
            for f in fields {
                collect_local_refs(&f.value, escaping);
                collect_escaping_in_expr(&f.value, escaping);
            }
        }
        // Index expressions: the index value may escape (used in indexing)
        TirExprKind::Index { expr: inner, index } => {
            collect_escaping_in_expr(inner, escaping);
            collect_escaping_in_expr(index, escaping);
        }
        // Binary/unary: recurse
        TirExprKind::Binary { left, right, .. } => {
            collect_escaping_in_expr(left, escaping);
            collect_escaping_in_expr(right, escaping);
        }
        TirExprKind::Unary { expr: inner, .. } | TirExprKind::Cast { expr: inner, .. } => {
            collect_escaping_in_expr(inner, escaping);
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            collect_escaping_in_expr(inner, escaping);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_escaping_in_expr(condition, escaping);
            for s in &then_branch.stmts {
                collect_escaping_in_stmt(s, escaping);
            }
            if let Some(eb) = else_branch {
                for s in &eb.stmts {
                    collect_escaping_in_stmt(s, escaping);
                }
            }
        }
        TirExprKind::LabeledBlock { block, .. } => {
            for s in &block.stmts {
                collect_escaping_in_stmt(s, escaping);
            }
        }
        TirExprKind::Block(block) => {
            for s in &block.stmts {
                collect_escaping_in_stmt(s, escaping);
            }
        }
        _ => {}
    }
}

/// Collect all local variable indices that directly appear in an expression.
/// Does NOT follow through `FieldAccess`: `s.repr` as a struct field value does
/// not mark `s` as escaping, since field extraction is typically for temporary
/// iterators/formatters consumed within the same scope.
fn collect_local_refs(expr: &TirExpr, locals: &mut IndexSet<u32>) {
    match &expr.kind {
        TirExprKind::Local { index, .. } => {
            locals.insert(*index);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            // The block result is the break value — check those
            for s in &block.stmts {
                if let TirStmtKind::Break {
                    value: Some(val), ..
                } = &s.kind
                {
                    collect_local_refs(val, locals);
                }
            }
        }
        TirExprKind::Unary { expr: inner, .. } | TirExprKind::Cast { expr: inner, .. } => {
            collect_local_refs(inner, locals);
        }
        // FieldAccess (e.g., s.repr) — accessing a subfield doesn't mean the whole
        // local escapes. Skip to avoid false positives with iterator construction.
        _ => {}
    }
}

/// Recursively transform statements, looking for Let bindings with __tmpl blocks.
fn transform_stmts_in_block(
    block: &mut TirBlock,
    escaping_locals: &IndexSet<u32>,
    hoist_stmts: &mut Vec<TirStmt>,
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    type_table: &std::cell::RefCell<TypeTable>,
) {
    for stmt in &mut block.stmts {
        transform_stmt(
            stmt,
            escaping_locals,
            hoist_stmts,
            local_count,
            local_types,
            type_table,
        );
    }
}

fn transform_stmt(
    stmt: &mut TirStmt,
    escaping_locals: &IndexSet<u32>,
    hoist_stmts: &mut Vec<TirStmt>,
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    type_table: &std::cell::RefCell<TypeTable>,
) {
    match &mut stmt.kind {
        TirStmtKind::Let {
            local_index, value, ..
        } => {
            // Check if this Let binds a __tmpl block result
            if let TirExprKind::LabeledBlock { label, block, .. } = &mut value.kind
                && label == "__tmpl"
                && !escaping_locals.contains(local_index)
                && let Some(candidate) = extract_tmpl_candidate(block)
            {
                transform_tmpl_block(
                    block,
                    &candidate,
                    hoist_stmts,
                    local_count,
                    local_types,
                    type_table,
                );
                return;
            }
            // Recurse into the value expression
            transform_expr(
                value,
                escaping_locals,
                hoist_stmts,
                local_count,
                local_types,
                type_table,
            );
        }
        TirStmtKind::Expr(expr) => {
            transform_expr(
                expr,
                escaping_locals,
                hoist_stmts,
                local_count,
                local_types,
                type_table,
            );
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            transform_expr(
                condition,
                escaping_locals,
                hoist_stmts,
                local_count,
                local_types,
                type_table,
            );
            transform_stmts_in_block(
                then_block,
                escaping_locals,
                hoist_stmts,
                local_count,
                local_types,
                type_table,
            );
            if let Some(eb) = else_block {
                transform_stmts_in_block(
                    eb,
                    escaping_locals,
                    hoist_stmts,
                    local_count,
                    local_types,
                    type_table,
                );
            }
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            transform_stmts_in_block(
                block,
                escaping_locals,
                hoist_stmts,
                local_count,
                local_types,
                type_table,
            );
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            transform_expr(
                scrutinee,
                escaping_locals,
                hoist_stmts,
                local_count,
                local_types,
                type_table,
            );
            transform_stmts_in_block(
                then_block,
                escaping_locals,
                hoist_stmts,
                local_count,
                local_types,
                type_table,
            );
            if let Some(eb) = else_block {
                transform_stmts_in_block(
                    eb,
                    escaping_locals,
                    hoist_stmts,
                    local_count,
                    local_types,
                    type_table,
                );
            }
        }
        TirStmtKind::Break {
            value: Some(expr), ..
        } => {
            transform_expr(
                expr,
                escaping_locals,
                hoist_stmts,
                local_count,
                local_types,
                type_table,
            );
        }
        // Don't recurse into nested loops
        TirStmtKind::Loop { .. } => {}
        _ => {}
    }
}

fn transform_expr(
    expr: &mut TirExpr,
    escaping_locals: &IndexSet<u32>,
    hoist_stmts: &mut Vec<TirStmt>,
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    type_table: &std::cell::RefCell<TypeTable>,
) {
    match &mut expr.kind {
        // Recurse into sub-expressions
        // Note: __tmpl in non-Let contexts (e.g. directly as function arguments like
        // `map[\`key{i}\`] = v`) are NOT hoisted because we can't track if the result escapes.
        TirExprKind::Call { args, .. } | TirExprKind::StaticCall { args, .. } => {
            for arg in args {
                transform_expr(
                    arg,
                    escaping_locals,
                    hoist_stmts,
                    local_count,
                    local_types,
                    type_table,
                );
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            transform_expr(
                receiver,
                escaping_locals,
                hoist_stmts,
                local_count,
                local_types,
                type_table,
            );
            for arg in args {
                transform_expr(
                    arg,
                    escaping_locals,
                    hoist_stmts,
                    local_count,
                    local_types,
                    type_table,
                );
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            transform_expr(
                left,
                escaping_locals,
                hoist_stmts,
                local_count,
                local_types,
                type_table,
            );
            transform_expr(
                right,
                escaping_locals,
                hoist_stmts,
                local_count,
                local_types,
                type_table,
            );
        }
        TirExprKind::Unary { expr: inner, .. } | TirExprKind::Cast { expr: inner, .. } => {
            transform_expr(
                inner,
                escaping_locals,
                hoist_stmts,
                local_count,
                local_types,
                type_table,
            );
        }
        TirExprKind::Assign { target, value } => {
            transform_expr(
                target,
                escaping_locals,
                hoist_stmts,
                local_count,
                local_types,
                type_table,
            );
            transform_expr(
                value,
                escaping_locals,
                hoist_stmts,
                local_count,
                local_types,
                type_table,
            );
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            transform_expr(
                condition,
                escaping_locals,
                hoist_stmts,
                local_count,
                local_types,
                type_table,
            );
            transform_stmts_in_block(
                then_branch,
                escaping_locals,
                hoist_stmts,
                local_count,
                local_types,
                type_table,
            );
            if let Some(eb) = else_branch {
                transform_stmts_in_block(
                    eb,
                    escaping_locals,
                    hoist_stmts,
                    local_count,
                    local_types,
                    type_table,
                );
            }
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            transform_expr(
                inner,
                escaping_locals,
                hoist_stmts,
                local_count,
                local_types,
                type_table,
            );
        }
        TirExprKind::LabeledBlock { block, .. } => {
            transform_stmts_in_block(
                block,
                escaping_locals,
                hoist_stmts,
                local_count,
                local_types,
                type_table,
            );
        }
        TirExprKind::Block(block) => {
            transform_stmts_in_block(
                block,
                escaping_locals,
                hoist_stmts,
                local_count,
                local_types,
                type_table,
            );
        }
        _ => {}
    }
}

/// Check if a `__tmpl` block has the expected pattern.
///
/// Before lowering:
///   `let mut __r = String::with_capacity(N);`
///
/// After lowering (inlined):
///   `let mut __r = String { repr: array_new<u8>(N), used: 0 };`
///
/// Both end with:
///   `break __tmpl: __r;`
fn extract_tmpl_candidate(block: &TirBlock) -> Option<TmplCandidate> {
    // First statement must be: let mut __r = ...
    let first_stmt = block.stmts.first()?;
    let (buf_local_index, string_type, capacity_expr, span) = match &first_stmt.kind {
        TirStmtKind::Let {
            name,
            local_index,
            value,
            type_id,
            ..
        } if name == "__r" => {
            // Try pre-lowered form: String::with_capacity(N)
            if let TirExprKind::StaticCall { func, args } = &value.kind
                && func.name() == "String::with_capacity"
            {
                let capacity = args.first()?;
                return Some(TmplCandidate {
                    buf_local_index: *local_index,
                    capacity_expr: capacity.clone(),
                    string_type: *type_id,
                    span: value.span,
                });
            }
            // Try post-lowered form: String { repr: array_new<u8>(N), used: 0 }
            if let TirExprKind::StructLiteral {
                struct_name,
                fields,
                ..
            } = &value.kind
            {
                if struct_name == "String" {
                    // Find the repr field containing an array_new call
                    let repr_field = fields.iter().find(|f| f.name == "repr")?;
                    let capacity = extract_array_new_capacity(&repr_field.value)?;
                    // Verify used field is 0
                    let used_field = fields.iter().find(|f| f.name == "used")?;
                    if !matches!(
                        &used_field.value.kind,
                        TirExprKind::IntLiteral { value: 0, .. }
                    ) {
                        return None;
                    }
                    (*local_index, *type_id, capacity, value.span)
                } else {
                    return None;
                }
            } else {
                return None;
            }
        }
        _ => return None,
    };

    // Last statement must be: break __tmpl: __r
    let last_stmt = block.stmts.last()?;
    match &last_stmt.kind {
        TirStmtKind::Break {
            label: Some(label),
            value: Some(val),
        } if label == "__tmpl" => match &val.kind {
            TirExprKind::Local { index, .. } if *index == buf_local_index => {}
            _ => return None,
        },
        _ => return None,
    }

    Some(TmplCandidate {
        buf_local_index,
        capacity_expr,
        string_type,
        span,
    })
}

/// Extract the capacity argument from an `array_new<u8>(N)` call.
fn extract_array_new_capacity(expr: &TirExpr) -> Option<TirExpr> {
    match &expr.kind {
        TirExprKind::StaticCall { func, args } | TirExprKind::Call { func, args, .. } => {
            let name = func.name();
            if name.contains("array_new") {
                args.first().cloned()
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Transform a `__tmpl` block to reuse a hoisted buffer.
fn transform_tmpl_block(
    block: &mut TirBlock,
    candidate: &TmplCandidate,
    hoist_stmts: &mut Vec<TirStmt>,
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    type_table: &std::cell::RefCell<TypeTable>,
) {
    let span = candidate.span;
    let string_type = candidate.string_type;

    // Create the repr array type: builtin::array<u8>
    let repr_type = type_table.borrow_mut().make_builtin_array(TypeTable::U8);

    // Allocate a new local for the hoisted repr array
    let repr_local_index = *local_count;
    *local_count += 1;
    local_types.push(repr_type);

    let repr_local_name = format!("__tmpl_repr_{repr_local_index}");

    // Hoist statement: let __tmpl_repr_N = builtin::array_new::<u8>(capacity)
    let array_new_call = TirExpr::new(
        TirExprKind::Call {
            func: crate::tir::FunctionRef::External {
                module_source: ModuleSource::builtin(),
                name: "array_new".to_string(),
                monomorph_info: Some(crate::tir::MonomorphInfo {
                    generic_name: "array_new".to_string(),
                    type_args: vec![TypeTable::U8],
                    is_blanket: false,
                }),
                method_info: None,
            },
            type_args: vec![],
            args: vec![candidate.capacity_expr.clone()],
        },
        repr_type,
        span,
    );

    hoist_stmts.push(TirStmt::new(
        TirStmtKind::Let {
            name: repr_local_name.clone(),
            local_index: repr_local_index,
            is_mut: true,
            is_reactive: false,
            type_id: repr_type,
            value: array_new_call,
            skip_value_copy: false,
        },
        span,
    ));

    // Replace the first statement (String::with_capacity / String { .. }) with a StructLiteral:
    // let mut __r = String { repr: __tmpl_repr_N, used: 0 };
    let struct_literal = TirExpr::new(
        TirExprKind::StructLiteral {
            struct_type: string_type,
            struct_name: "String".to_string(),
            fields: vec![
                TirStructField {
                    name: "repr".to_string(),
                    value: TirExpr::new(
                        TirExprKind::Local {
                            index: repr_local_index,
                            name: repr_local_name.clone(),
                        },
                        repr_type,
                        span,
                    ),
                    field_index: 0,
                },
                TirStructField {
                    name: "used".to_string(),
                    value: TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: 0,
                            repr: "0".to_string(),
                        },
                        TypeTable::I32,
                        span,
                    ),
                    field_index: 1,
                },
            ],
        },
        string_type,
        span,
    );

    // Replace the first statement's value
    if let TirStmtKind::Let { value, .. } = &mut block.stmts[0].kind {
        *value = struct_literal;
    }

    // Insert a sync statement before the last break:
    // __tmpl_repr_N = __r.repr;
    let sync_stmt = TirStmt::new(
        TirStmtKind::Expr(TirExpr::new(
            TirExprKind::Assign {
                target: Box::new(TirExpr::new(
                    TirExprKind::Local {
                        index: repr_local_index,
                        name: repr_local_name,
                    },
                    repr_type,
                    span,
                )),
                value: Box::new(TirExpr::new(
                    TirExprKind::FieldAccess {
                        expr: Box::new(TirExpr::new(
                            TirExprKind::Local {
                                index: candidate.buf_local_index,
                                name: "__r".to_string(),
                            },
                            string_type,
                            span,
                        )),
                        field_index: 0,
                        field_name: "repr".to_string(),
                    },
                    repr_type,
                    span,
                )),
            },
            TypeTable::UNIT,
            span,
        )),
        span,
    );

    // Insert the sync before the last statement (the break)
    let break_idx = block.stmts.len() - 1;
    block.stmts.insert(break_idx, sync_stmt);
}
