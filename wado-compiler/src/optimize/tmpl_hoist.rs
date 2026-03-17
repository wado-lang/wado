//! Template String Buffer Hoisting for Loops
//!
//! When a template string (`__tmpl` labeled block) appears inside a loop,
//! this pass hoists the entire `String` allocation before the loop and reuses
//! it across iterations, resetting `used = 0` instead of creating a new struct.
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
//! let mut __tmpl_buf_0 = String { repr: array_new(N), used: 0 };
//! loop {
//!     let s /* skip_value_copy */ = __tmpl: {
//!         __tmpl_buf_0.used = 0;        // reset (no struct.new)
//!         __tmpl_buf_0.append(...);      // reuse same String
//!         break __tmpl: __tmpl_buf_0;
//!     };
//!     s.len();   // s aliases __tmpl_buf_0
//! }
//! ```
//!
//! Safety: The optimization reuses the same String GC struct across iterations.
//! It is only applied when the template result does not escape the iteration:
//! the result must be bound to a Let variable that is only used as a method
//! receiver (`self`), never passed as a regular function argument.

use crate::hashmap::IndexSet;
use crate::project::Project;
use crate::tir::{
    TirBlock, TirExpr, TirExprKind, TirFunction, TirStmt, TirStmtKind, TypeId, TypeTable,
};
use crate::token::Span;

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
            TirStmtKind::IfLet {
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
    /// The initial value expression (e.g., `String { repr: array_new(N), used: 0 }`)
    init_value: TirExpr,
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
    let mut escaping = IndexSet::default();
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
        TirStmtKind::IfLet {
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
        TirExprKind::Call { args, .. } => {
            for arg in args {
                collect_local_refs(&arg.expr, escaping);
                collect_escaping_in_expr(&arg.expr, escaping);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            // Receiver (self) doesn't escape — only non-self args escape
            collect_escaping_in_expr(receiver, escaping);
            for arg in args {
                collect_local_refs(&arg.expr, escaping);
                collect_escaping_in_expr(&arg.expr, escaping);
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
            local_index,
            value,
            skip_value_copy,
            ..
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
                // The hoisted String is reused; skip deep copy so `s` aliases `__tmpl_buf`.
                *skip_value_copy = true;
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
        TirStmtKind::IfLet {
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
        TirExprKind::Call { args, .. } => {
            for arg in args {
                transform_expr(
                    &mut arg.expr,
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
                    &mut arg.expr,
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
    let (buf_local_index, string_type, init_value, span) = match &first_stmt.kind {
        TirStmtKind::Let {
            name,
            local_index,
            value,
            type_id,
            ..
        } if name == "__r" => {
            // Try pre-lowered form: String::with_capacity(N)
            if let TirExprKind::Call { func, .. } = &value.kind
                && func.method_info.is_some()
                && func.name.clone() == "String::with_capacity"
            {
                return Some(TmplCandidate {
                    buf_local_index: *local_index,
                    init_value: value.clone(),
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
                    // Verify the repr field contains an array_new call
                    let repr_field = fields.iter().find(|f| f.name == "repr")?;
                    extract_array_new_capacity(&repr_field.value)?;
                    // Verify used field is 0
                    let used_field = fields.iter().find(|f| f.name == "used")?;
                    if !matches!(
                        &used_field.value.kind,
                        TirExprKind::IntLiteral { value: 0, .. }
                    ) {
                        return None;
                    }
                    (*local_index, *type_id, value.clone(), value.span)
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
        init_value,
        string_type,
        span,
    })
}

/// Extract the capacity argument from an `array_new<u8>(N)` call.
fn extract_array_new_capacity(expr: &TirExpr) -> Option<TirExpr> {
    match &expr.kind {
        TirExprKind::Call { func, args, .. } => {
            let name = func.name.clone();
            if name.contains("array_new") {
                args.first().map(|a| a.expr.clone())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Transform a `__tmpl` block to reuse a hoisted String.
///
/// The entire String (not just the backing array) is hoisted before the loop.
/// Inside the block, `let mut __r = String { ... }` is replaced with
/// `__tmpl_buf.used = 0` (field reset), and all references to `__r` are
/// renamed to `__tmpl_buf`. The outer Let binding gets `skip_value_copy = true`
/// so the bound variable aliases the hoisted String directly.
fn transform_tmpl_block(
    block: &mut TirBlock,
    candidate: &TmplCandidate,
    hoist_stmts: &mut Vec<TirStmt>,
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    _type_table: &std::cell::RefCell<TypeTable>,
) {
    let span = candidate.span;
    let string_type = candidate.string_type;

    // Allocate a new local for the hoisted String
    let buf_local_index = *local_count;
    *local_count += 1;
    local_types.push(string_type);

    let buf_local_name = format!("__tmpl_buf_{buf_local_index}");

    // Hoist statement: let mut __tmpl_buf_N = String { repr: array_new(N), used: 0 };
    hoist_stmts.push(TirStmt::new(
        TirStmtKind::Let {
            name: buf_local_name.clone(),
            local_index: buf_local_index,
            is_mut: true,
            is_reactive: false,
            type_id: string_type,
            value: candidate.init_value.clone(),
            skip_value_copy: false,
        },
        span,
    ));

    // Replace the first statement (let mut __r = String { ... }) with a field reset:
    // __tmpl_buf_N.used = 0;
    let reset_stmt = TirStmt::new(
        TirStmtKind::Expr(TirExpr::new(
            TirExprKind::Assign {
                target: Box::new(TirExpr::new(
                    TirExprKind::FieldAccess {
                        expr: Box::new(TirExpr::new(
                            TirExprKind::Local {
                                index: buf_local_index,
                                name: buf_local_name.clone(),
                            },
                            string_type,
                            span,
                        )),
                        field_index: 1,
                        field_name: "used".to_string(),
                    },
                    TypeTable::I32,
                    span,
                )),
                value: Box::new(TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: 0,
                        repr: "0".to_string(),
                    },
                    TypeTable::I32,
                    span,
                )),
            },
            TypeTable::UNIT,
            span,
        )),
        span,
    );
    block.stmts[0] = reset_stmt;

    // Rename all references from __r (old local index) to __tmpl_buf_N (new local index)
    let old_index = candidate.buf_local_index;
    for stmt in &mut block.stmts {
        rename_local_in_stmt(stmt, old_index, buf_local_index, &buf_local_name);
    }
}

fn rename_local_in_stmt(stmt: &mut TirStmt, old_index: u32, new_index: u32, new_name: &str) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => {
            rename_local_in_expr(value, old_index, new_index, new_name);
        }
        TirStmtKind::Expr(expr) => {
            rename_local_in_expr(expr, old_index, new_index, new_name);
        }
        TirStmtKind::Return { value: Some(expr) } => {
            rename_local_in_expr(expr, old_index, new_index, new_name);
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            rename_local_in_expr(condition, old_index, new_index, new_name);
            rename_local_in_block(then_block, old_index, new_index, new_name);
            if let Some(eb) = else_block {
                rename_local_in_block(eb, old_index, new_index, new_name);
            }
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            rename_local_in_block(block, old_index, new_index, new_name);
        }
        TirStmtKind::Loop { body } => {
            rename_local_in_block(body, old_index, new_index, new_name);
        }
        TirStmtKind::Break {
            value: Some(expr), ..
        } => {
            rename_local_in_expr(expr, old_index, new_index, new_name);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            rename_local_in_expr(scrutinee, old_index, new_index, new_name);
            rename_local_in_block(then_block, old_index, new_index, new_name);
            if let Some(eb) = else_block {
                rename_local_in_block(eb, old_index, new_index, new_name);
            }
        }
        _ => {}
    }
}

fn rename_local_in_block(block: &mut TirBlock, old_index: u32, new_index: u32, new_name: &str) {
    for stmt in &mut block.stmts {
        rename_local_in_stmt(stmt, old_index, new_index, new_name);
    }
}

fn rename_local_in_expr(expr: &mut TirExpr, old_index: u32, new_index: u32, new_name: &str) {
    match &mut expr.kind {
        TirExprKind::Local { index, name } if *index == old_index => {
            *index = new_index;
            *name = new_name.to_string();
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                rename_local_in_expr(&mut arg.expr, old_index, new_index, new_name);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            rename_local_in_expr(receiver, old_index, new_index, new_name);
            for arg in args {
                rename_local_in_expr(&mut arg.expr, old_index, new_index, new_name);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            rename_local_in_expr(callee, old_index, new_index, new_name);
            for arg in args {
                rename_local_in_expr(arg, old_index, new_index, new_name);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            rename_local_in_expr(left, old_index, new_index, new_name);
            rename_local_in_expr(right, old_index, new_index, new_name);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. } => {
            rename_local_in_expr(inner, old_index, new_index, new_name);
        }
        TirExprKind::Assign { target, value } => {
            rename_local_in_expr(target, old_index, new_index, new_name);
            rename_local_in_expr(value, old_index, new_index, new_name);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for f in fields {
                rename_local_in_expr(&mut f.value, old_index, new_index, new_name);
            }
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            rename_local_in_expr(inner, old_index, new_index, new_name);
            rename_local_in_expr(index, old_index, new_index, new_name);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            rename_local_in_expr(condition, old_index, new_index, new_name);
            rename_local_in_block(then_branch, old_index, new_index, new_name);
            if let Some(eb) = else_branch {
                rename_local_in_block(eb, old_index, new_index, new_name);
            }
        }
        TirExprKind::LabeledBlock { block, .. } => {
            rename_local_in_block(block, old_index, new_index, new_name);
        }
        TirExprKind::Block(block) => {
            rename_local_in_block(block, old_index, new_index, new_name);
        }
        _ => {}
    }
}
