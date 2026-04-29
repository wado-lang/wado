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
//!         __r.push_str(...);
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
//!         __tmpl_buf_0.push_str(...);      // reuse same String
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

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexSet;
use crate::tir::{
    TirBlock, TirExpr, TirExprKind, TirFunction, TirLocal, TirStmt, TirStmtKind, TirUnaryOp,
    TypeId, TypeTable,
};
use crate::token::Span;

/// Apply template string buffer hoisting to all functions in the project.
pub fn hoist_template_buffers(project: &mut FlatPackage) -> bool {
    let mut changed = false;
    let type_table = project.type_table.clone();
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        changed |= hoist_in_function(&mut func, &type_table);
    }
    changed
}

fn hoist_in_function(func: &mut TirFunction, type_table: &std::cell::RefCell<TypeTable>) -> bool {
    let Some(ref mut body) = func.body else {
        return false;
    };
    let mut local_count = func.local_count;
    let mut locals = func.locals.clone();
    let changed = hoist_in_block(body, &mut local_count, &mut locals, type_table);
    func.local_count = local_count;
    func.locals = locals;
    changed
}

fn hoist_in_block(
    block: &mut TirBlock,
    local_count: &mut u32,
    locals: &mut Vec<TirLocal>,
    type_table: &std::cell::RefCell<TypeTable>,
) -> bool {
    let mut changed = false;
    let mut new_stmts = Vec::new();

    for mut stmt in std::mem::take(&mut block.stmts) {
        match &mut stmt.kind {
            TirStmtKind::Loop { body } => {
                // Recurse into loop body first (for nested loops)
                changed |= hoist_in_block(body, local_count, locals, type_table);

                // Try to hoist template buffers out of this loop
                let hoist_stmts = hoist_tmpl_from_loop(body, local_count, locals, type_table);
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
                changed |= hoist_in_block(then_block, local_count, locals, type_table);
                if let Some(eb) = else_block {
                    changed |= hoist_in_block(eb, local_count, locals, type_table);
                }
                new_stmts.push(stmt);
            }
            TirStmtKind::LabeledBlock { block: inner, .. } => {
                changed |= hoist_in_block(inner, local_count, locals, type_table);
                new_stmts.push(stmt);
            }
            TirStmtKind::IfLet {
                then_block,
                else_block,
                ..
            } => {
                changed |= hoist_in_block(then_block, local_count, locals, type_table);
                if let Some(eb) = else_block {
                    changed |= hoist_in_block(eb, local_count, locals, type_table);
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

/// Information about a Formatter struct literal that can be hoisted out of a `__tmpl` block.
struct FmtCandidate {
    /// Index of the statement inside the `__tmpl` block that creates the Formatter
    stmt_index: usize,
    /// The local index being assigned to (e.g., `__local_13`)
    fmt_local_index: u32,
    /// The Formatter struct literal expression
    init_value: TirExpr,
    /// The Formatter type ID
    formatter_type: TypeId,
    /// Index of the `indent` field in the Formatter struct
    indent_field_index: u32,
    /// The span
    span: Span,
}

/// Scan a loop body for `__tmpl` labeled blocks and hoist their buffer allocations.
/// Returns hoisting statements to prepend before the loop.
fn hoist_tmpl_from_loop(
    loop_body: &mut TirBlock,
    local_count: &mut u32,
    locals: &mut Vec<TirLocal>,
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
        locals,
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
        TirStmtKind::Return { value: None } | TirStmtKind::Continue => {}
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_local_refs(v, escaping);
                collect_escaping_in_expr(v, escaping);
            }
        }
        TirStmtKind::LetDestructure { value, .. } => {
            collect_escaping_in_expr(value, escaping);
        }
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
        TirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
        }
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
        // Assignment: the value escapes (stored in target location)
        TirExprKind::Assign { target, value } => {
            collect_local_refs(value, escaping);
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
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => {
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
        TirExprKind::Match { expr: inner, arms } => {
            collect_escaping_in_expr(inner, escaping);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_escaping_in_expr(guard, escaping);
                }
                collect_escaping_in_expr(&arm.body, escaping);
            }
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_escaping_in_expr(scrutinee, escaping);
            for arm in arms {
                for s in &arm.stmts {
                    collect_escaping_in_stmt(s, escaping);
                }
            }
            for s in &default.stmts {
                collect_escaping_in_stmt(s, escaping);
            }
        }
        TirExprKind::Closure { body, captures, .. } => {
            // Closure captures: the captured variables escape
            for capture in captures {
                escaping.insert(capture.outer_index);
            }
            collect_escaping_in_expr(body, escaping);
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_local_refs(arg, escaping);
                collect_escaping_in_expr(arg, escaping);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                collect_local_refs(elem, escaping);
                collect_escaping_in_expr(elem, escaping);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                collect_local_refs(p, escaping);
                collect_escaping_in_expr(p, escaping);
            }
        }
        TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::ClosureToCanonical { functor: inner, .. } => {
            collect_escaping_in_expr(inner, escaping);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            collect_local_refs(value, escaping);
            collect_escaping_in_expr(value, escaping);
        }
        // Leaf nodes
        TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
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
    locals: &mut Vec<TirLocal>,
    type_table: &std::cell::RefCell<TypeTable>,
) {
    for stmt in &mut block.stmts {
        transform_stmt(
            stmt,
            escaping_locals,
            hoist_stmts,
            local_count,
            locals,
            type_table,
        );
    }
}

fn transform_stmt(
    stmt: &mut TirStmt,
    escaping_locals: &IndexSet<u32>,
    hoist_stmts: &mut Vec<TirStmt>,
    local_count: &mut u32,
    locals: &mut Vec<TirLocal>,
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
                    locals,
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
                locals,
                type_table,
            );
        }
        TirStmtKind::Expr(expr) => {
            transform_expr(
                expr,
                escaping_locals,
                hoist_stmts,
                local_count,
                locals,
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
                locals,
                type_table,
            );
            transform_stmts_in_block(
                then_block,
                escaping_locals,
                hoist_stmts,
                local_count,
                locals,
                type_table,
            );
            if let Some(eb) = else_block {
                transform_stmts_in_block(
                    eb,
                    escaping_locals,
                    hoist_stmts,
                    local_count,
                    locals,
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
                locals,
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
                locals,
                type_table,
            );
            transform_stmts_in_block(
                then_block,
                escaping_locals,
                hoist_stmts,
                local_count,
                locals,
                type_table,
            );
            if let Some(eb) = else_block {
                transform_stmts_in_block(
                    eb,
                    escaping_locals,
                    hoist_stmts,
                    local_count,
                    locals,
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
                locals,
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
    locals: &mut Vec<TirLocal>,
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
                    locals,
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
                locals,
                type_table,
            );
            for arg in args {
                transform_expr(
                    &mut arg.expr,
                    escaping_locals,
                    hoist_stmts,
                    local_count,
                    locals,
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
                locals,
                type_table,
            );
            transform_expr(
                right,
                escaping_locals,
                hoist_stmts,
                local_count,
                locals,
                type_table,
            );
        }
        TirExprKind::Unary { expr: inner, .. } | TirExprKind::Cast { expr: inner, .. } => {
            transform_expr(
                inner,
                escaping_locals,
                hoist_stmts,
                local_count,
                locals,
                type_table,
            );
        }
        TirExprKind::Assign { target, value } => {
            transform_expr(
                target,
                escaping_locals,
                hoist_stmts,
                local_count,
                locals,
                type_table,
            );
            transform_expr(
                value,
                escaping_locals,
                hoist_stmts,
                local_count,
                locals,
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
                locals,
                type_table,
            );
            transform_stmts_in_block(
                then_branch,
                escaping_locals,
                hoist_stmts,
                local_count,
                locals,
                type_table,
            );
            if let Some(eb) = else_branch {
                transform_stmts_in_block(
                    eb,
                    escaping_locals,
                    hoist_stmts,
                    local_count,
                    locals,
                    type_table,
                );
            }
        }
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => {
            transform_expr(
                inner,
                escaping_locals,
                hoist_stmts,
                local_count,
                locals,
                type_table,
            );
        }
        TirExprKind::LabeledBlock { block, .. } => {
            transform_stmts_in_block(
                block,
                escaping_locals,
                hoist_stmts,
                local_count,
                locals,
                type_table,
            );
        }
        TirExprKind::Block(block) => {
            transform_stmts_in_block(
                block,
                escaping_locals,
                hoist_stmts,
                local_count,
                locals,
                type_table,
            );
        }
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
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

/// Collect all Formatter struct literals in a `__tmpl` block that can be hoisted.
///
/// Detects three patterns:
///   1. Assign: `__local_N = Formatter { ... }`
///   2. Let:    `let mut __f = Formatter { ... }`
///   3. `LabeledBlock` (inlined `Formatter::new)`:
///      `let __f = label: { let buf = &mut __tmpl_buf; break: Formatter { ..., buf } }`
///      or `__local_N = label: { ... break: Formatter { ... } }`
fn extract_fmt_candidates(
    block: &TirBlock,
    hoisted_buf_index: u32,
    type_table: &std::cell::RefCell<TypeTable>,
) -> Vec<FmtCandidate> {
    let type_table = type_table.borrow();
    let mut candidates = Vec::new();

    for (i, stmt) in block.stmts.iter().enumerate() {
        if i == 0 || i == block.stmts.len() - 1 {
            continue;
        }

        // Try to extract (local_index, value_expr) from the statement
        let (fmt_local_index, value_expr): (u32, &TirExpr) = match &stmt.kind {
            TirStmtKind::Expr(expr) => {
                let TirExprKind::Assign { target, value } = &expr.kind else {
                    continue;
                };
                let TirExprKind::Local { index, .. } = &target.kind else {
                    continue;
                };
                (*index, value)
            }
            TirStmtKind::Let {
                local_index, value, ..
            } => (*local_index, value),
            _ => continue,
        };

        // Try to extract Formatter fields from the value expression
        let Some(extracted) = extract_formatter_fields(value_expr, hoisted_buf_index) else {
            continue;
        };

        let (struct_name, fields, struct_type, value_type_id, value_span) = extracted;

        if struct_name != "Formatter" {
            continue;
        }

        // Find the `indent` field index
        let Some(indent_field) = fields.iter().find(|f| f.name == "indent") else {
            continue;
        };
        let indent_field_index = indent_field.field_index;

        // Verify all non-buf fields are constant (literals)
        let all_const = fields.iter().all(|f| {
            if f.name == "buf" {
                return true;
            }
            is_constant_expr(&f.value)
        });
        if !all_const {
            continue;
        }

        // Verify the Formatter type is a struct
        let resolved = type_table.get(value_type_id);
        if !matches!(resolved, crate::tir::ResolvedType::Struct { .. }) {
            continue;
        }

        // Build init_value with buf pointing to hoisted buffer
        let init_fields: Vec<_> = fields
            .iter()
            .map(|f| {
                if f.name == "buf" {
                    // Normalize buf to &mut __tmpl_buf
                    crate::tir::TirStructField {
                        name: f.name.clone(),
                        value: TirExpr::new(
                            TirExprKind::Unary {
                                op: TirUnaryOp::MutRef,
                                expr: Box::new(TirExpr::new(
                                    TirExprKind::Local {
                                        index: hoisted_buf_index,
                                        name: format!("__tmpl_buf_{hoisted_buf_index}"),
                                    },
                                    f.value.type_id,
                                    value_span,
                                )),
                            },
                            f.value.type_id,
                            value_span,
                        ),
                        field_index: f.field_index,
                    }
                } else {
                    f.clone()
                }
            })
            .collect();

        candidates.push(FmtCandidate {
            stmt_index: i,
            fmt_local_index,
            init_value: TirExpr::new(
                TirExprKind::StructLiteral {
                    struct_type,
                    struct_name: struct_name.to_string(),
                    fields: init_fields,
                },
                value_type_id,
                value_span,
            ),
            formatter_type: struct_type,
            indent_field_index,
            span: value_span,
        });
    }
    candidates
}

/// Extract Formatter struct literal fields from a value expression.
///
/// Handles:
///   - Direct `StructLiteral { ... }` where buf references hoisted buffer
///   - `LabeledBlock { let buf = ...; break: StructLiteral { ..., buf } }`
///     where the intermediate `buf` local traces back to the hoisted buffer
fn extract_formatter_fields(
    value: &TirExpr,
    hoisted_buf_index: u32,
) -> Option<(&str, &[crate::tir::TirStructField], TypeId, TypeId, Span)> {
    match &value.kind {
        TirExprKind::StructLiteral {
            struct_name,
            fields,
            struct_type,
        } => {
            let buf_field = fields.iter().find(|f| f.name == "buf")?;
            if !buf_field_references_local(&buf_field.value, hoisted_buf_index) {
                return None;
            }
            Some((struct_name, fields, *struct_type, value.type_id, value.span))
        }
        TirExprKind::LabeledBlock { block, .. } => {
            // Pattern: { let buf = &mut __tmpl_buf; break label: Formatter { ..., buf } }
            // or: { __local = __tmpl_buf; break: Formatter { ..., buf: ref.as_non_null(__local) } }
            let break_stmt = block.stmts.last()?;
            let break_value = match &break_stmt.kind {
                TirStmtKind::Break { value: Some(v), .. } => v,
                _ => return None,
            };
            let TirExprKind::StructLiteral {
                struct_name,
                fields,
                struct_type,
            } = &break_value.kind
            else {
                return None;
            };

            // Check if buf traces to hoisted buffer (directly or via intermediate local)
            let buf_field = fields.iter().find(|f| f.name == "buf")?;
            if buf_field_references_local(&buf_field.value, hoisted_buf_index) {
                return Some((struct_name, fields, *struct_type, value.type_id, value.span));
            }

            // Trace through intermediate local in the block
            let buf_inner_local = extract_local_from_ref(&buf_field.value)?;
            for stmt in &block.stmts {
                match &stmt.kind {
                    TirStmtKind::Let {
                        local_index,
                        value: let_value,
                        ..
                    } if *local_index == buf_inner_local => {
                        if references_local(let_value, hoisted_buf_index) {
                            return Some((
                                struct_name,
                                fields,
                                *struct_type,
                                value.type_id,
                                value.span,
                            ));
                        }
                    }
                    TirStmtKind::Expr(expr) => {
                        if let TirExprKind::Assign { target, value: av } = &expr.kind
                            && let TirExprKind::Local { index, .. } = &target.kind
                            && *index == buf_inner_local
                            && references_local(av, hoisted_buf_index)
                        {
                            return Some((
                                struct_name,
                                fields,
                                *struct_type,
                                value.type_id,
                                value.span,
                            ));
                        }
                    }
                    _ => {}
                }
            }
            None
        }
        _ => None,
    }
}

/// Extract the local index from a reference expression.
/// Handles `&mut Local(N)`, `Local(N)`, and `ref.as_non_null(Local(N))`.
fn extract_local_from_ref(expr: &TirExpr) -> Option<u32> {
    match &expr.kind {
        TirExprKind::Local { index, .. } => Some(*index),
        TirExprKind::Unary {
            op: TirUnaryOp::MutRef | TirUnaryOp::Ref,
            expr: inner,
        } => match &inner.kind {
            TirExprKind::Local { index, .. } => Some(*index),
            _ => None,
        },
        TirExprKind::Call { func, args, .. } if func.name.contains("ref.as_non_null") => {
            args.first().and_then(|a| match &a.expr.kind {
                TirExprKind::Local { index, .. } => Some(*index),
                _ => None,
            })
        }
        _ => None,
    }
}

/// Check if an expression references a specific local (possibly through &mut).
fn references_local(expr: &TirExpr, local_index: u32) -> bool {
    match &expr.kind {
        TirExprKind::Local { index, .. } => *index == local_index,
        TirExprKind::Unary {
            op: TirUnaryOp::MutRef,
            expr: inner,
        } => references_local(inner, local_index),
        _ => false,
    }
}

/// Check if a `buf` field expression references the given local (the hoisted String buffer).
/// Handles both `&mut local` (TIR form) and `ref.as_non_null(local)` patterns.
fn buf_field_references_local(expr: &TirExpr, local_index: u32) -> bool {
    match &expr.kind {
        // &mut __tmpl_buf (TIR level)
        TirExprKind::Unary {
            op: TirUnaryOp::MutRef,
            expr: inner,
        } => matches!(&inner.kind, TirExprKind::Local { index, .. } if *index == local_index),
        // ref.as_non_null(__tmpl_buf) (WIR level / after lowering)
        TirExprKind::Call { func, args, .. } => {
            func.name.contains("ref.as_non_null")
                && args.len() == 1
                && matches!(&args[0].expr.kind, TirExprKind::Local { index, .. } if *index == local_index)
        }
        _ => false,
    }
}

/// Check if an expression is a compile-time constant (literal).
fn is_constant_expr(expr: &TirExpr) -> bool {
    matches!(
        &expr.kind,
        TirExprKind::IntLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::EnumConstruct { .. }
    )
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
    locals: &mut Vec<TirLocal>,
    type_table: &std::cell::RefCell<TypeTable>,
) {
    let span = candidate.span;
    let string_type = candidate.string_type;

    // Allocate a new local for the hoisted String
    let buf_local_index = *local_count;
    *local_count += 1;
    let buf_local_name = format!("__tmpl_buf_{buf_local_index}");
    locals.push(TirLocal {
        name: buf_local_name.clone(),
        type_id: string_type,
        is_mut: true,
    });

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

    // Phase 2: Hoist Formatter struct literals as well.
    // After the String rename above, the block may contain one or more Formatter
    // creations (direct struct literals or inlined Formatter::new LabeledBlocks).
    // Each distinct Formatter is hoisted to its own local before the loop.
    let fmt_candidates = extract_fmt_candidates(block, buf_local_index, type_table);
    if !fmt_candidates.is_empty() {
        transform_fmts_in_tmpl_block(
            block,
            &fmt_candidates,
            hoist_stmts,
            local_count,
            locals,
        );
    }
}

/// Hoist Formatter struct literals out of a `__tmpl` block.
///
/// Each candidate gets its own hoisted local. Replaces the struct literal with an
/// `indent` field reset, and renames all references to the hoisted local.
///
/// Processes candidates in reverse order so that `stmt_index` values remain valid
/// as we replace statements.
fn transform_fmts_in_tmpl_block(
    block: &mut TirBlock,
    candidates: &[FmtCandidate],
    hoist_stmts: &mut Vec<TirStmt>,
    local_count: &mut u32,
    locals: &mut Vec<TirLocal>,
) {
    // Sort by stmt_index ascending to compute rename ranges
    let mut sorted_candidates: Vec<_> = candidates.iter().collect();
    sorted_candidates.sort_by_key(|c| c.stmt_index);

    // Build a mapping from each candidate to its hoisted local and rename range.
    // When multiple candidates share the same fmt_local_index, each one's rename
    // range extends from its stmt_index to the next candidate's stmt_index (exclusive)
    // that shares the same local. For the last candidate with a given local, the
    // range extends to the end of the block.
    struct HoistInfo {
        hoisted_index: u32,
        hoisted_name: String,
        stmt_index: usize,
        rename_start: usize,
        rename_end: usize, // exclusive
        old_fmt_index: u32,
        formatter_type: TypeId,
        indent_field_index: u32,
        init_value: TirExpr,
        span: Span,
    }
    let block_len = block.stmts.len();
    let mut hoist_infos = Vec::new();

    for (pos, candidate) in sorted_candidates.iter().enumerate() {
        let fmt_local_index = *local_count;
        *local_count += 1;
        locals.push(TirLocal {
            name: format!("__tmpl_fmt_{fmt_local_index}"),
            type_id: candidate.formatter_type,
            is_mut: true,
        });

        // Find the next candidate that shares the same fmt_local_index
        let rename_end = sorted_candidates[pos + 1..]
            .iter()
            .find(|c| c.fmt_local_index == candidate.fmt_local_index)
            .map(|c| c.stmt_index)
            .unwrap_or(block_len);

        hoist_infos.push(HoistInfo {
            hoisted_index: fmt_local_index,
            hoisted_name: format!("__fmt_buf_{fmt_local_index}"),
            stmt_index: candidate.stmt_index,
            rename_start: candidate.stmt_index,
            rename_end,
            old_fmt_index: candidate.fmt_local_index,
            formatter_type: candidate.formatter_type,
            indent_field_index: candidate.indent_field_index,
            init_value: candidate.init_value.clone(),
            span: candidate.span,
        });
    }

    for info in &hoist_infos {
        // Hoist statement: let mut __fmt_buf_N = Formatter { fill: ..., buf: ... };
        hoist_stmts.push(TirStmt::new(
            TirStmtKind::Let {
                name: info.hoisted_name.clone(),
                local_index: info.hoisted_index,
                is_mut: true,
                is_reactive: false,
                type_id: info.formatter_type,
                value: info.init_value.clone(),
                skip_value_copy: false,
            },
            info.span,
        ));

        // Replace the Formatter struct literal with an indent field reset:
        //   __fmt_buf_N.indent = 0;
        //
        // IMPORTANT: Do NOT remove this indent = 0 reset! Format functions
        // (especially pretty-print with `:#?`) may modify the `indent` field
        // during formatting. Without this reset, the indent value would
        // accumulate across loop iterations, causing incorrect indentation.
        let indent_reset_stmt = TirStmt::new(
            TirStmtKind::Expr(TirExpr::new(
                TirExprKind::Assign {
                    target: Box::new(TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: info.hoisted_index,
                                    name: info.hoisted_name.clone(),
                                },
                                info.formatter_type,
                                info.span,
                            )),
                            field_index: info.indent_field_index,
                            field_name: "indent".to_string(),
                        },
                        TypeTable::I32,
                        info.span,
                    )),
                    value: Box::new(TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: 0,
                            repr: "0".to_string(),
                        },
                        TypeTable::I32,
                        info.span,
                    )),
                },
                TypeTable::UNIT,
                info.span,
            )),
            info.span,
        );
        block.stmts[info.stmt_index] = indent_reset_stmt;

        // Rename references from the old Formatter local to the hoisted one,
        // only within [rename_start, rename_end) to avoid clobbering other
        // candidates that share the same original local.
        for stmt in &mut block.stmts[info.rename_start..info.rename_end] {
            rename_local_in_stmt(
                stmt,
                info.old_fmt_index,
                info.hoisted_index,
                &info.hoisted_name,
            );
        }
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
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
        _ => {}
    }
}
