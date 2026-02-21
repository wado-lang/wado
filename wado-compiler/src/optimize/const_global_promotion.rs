//! Constant Global Promotion (CGP) optimization
//!
//! After lowering, globals with non-constant initializers (e.g., `global B: i32 = A + 10`)
//! are forced to be Wasm-mutable and initialized at runtime in `__initialize_module()`.
//! After constant propagation and folding optimize the init function body, some of these
//! runtime initializations may have been reduced to scalar constants (e.g., `B = 15`).
//!
//! This pass promotes such globals back to immutable compile-time constants:
//! 1. Scans all functions for `GlobalVarSet` statements with constant values targeting
//!    promotable globals (user-declared immutable, currently Wasm-mutable)
//! 2. Updates the global's initializer and marks it immutable
//! 3. Removes all `GlobalVarSet` to promoted globals from all functions
//!    (handles both original `__initialize_module` and inlined copies)
//!
//! This enables further optimization in subsequent iterations:
//! - Promoted constants become available for constant propagation
//! - Dependent globals may then also fold to constants and get promoted

use crate::name::ModuleSource;
use crate::project::Project;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind};
use indexmap::IndexMap;

type GlobalKey = (ModuleSource, String);

/// Try to promote constant globals in all modules.
/// Returns `true` if any globals were promoted.
pub fn promote_constant_globals(project: &mut Project) -> bool {
    let mut changed = false;

    let module_sources: Vec<_> = project.tir_modules.keys().cloned().collect();
    for module_source in module_sources {
        changed |= promote_in_module(project, &module_source);
    }

    changed
}

fn promote_in_module(project: &mut Project, module_source: &ModuleSource) -> bool {
    let module = &project.tir_modules[module_source];

    // Build a lookup of promotable globals: currently Wasm-mutable but user-declared immutable
    let mut promotable_globals: IndexMap<GlobalKey, usize> = IndexMap::new();
    for (idx, global) in module.globals.iter().enumerate() {
        if global.mutable && !global.wado_mutable {
            promotable_globals.insert((global.module_source.clone(), global.name.clone()), idx);
        }
    }

    if promotable_globals.is_empty() {
        return false;
    }

    // Scan all functions for GlobalVarSet to promotable globals with constant values
    let mut promotions: IndexMap<GlobalKey, TirExprKind> = IndexMap::new();
    for func_rc in &module.functions {
        let func = func_rc.borrow();
        let Some(body) = &func.body else { continue };
        collect_promotions_from_block(body, &promotable_globals, &mut promotions);
    }

    if promotions.is_empty() {
        return false;
    }

    // Apply promotions: update global initializers and mark immutable
    let module = project.tir_modules.get_mut(module_source).unwrap();
    for (key, init_kind) in &promotions {
        if let Some(&idx) = promotable_globals.get(key) {
            let global = &mut module.globals[idx];
            global.initializer.kind = init_kind.clone();
            global.mutable = false;
        }
    }

    // Remove all GlobalVarSet to promoted globals from all functions
    for func_rc in &module.functions {
        let mut func = func_rc.borrow_mut();
        let Some(body) = &mut func.body else { continue };
        remove_promoted_sets_from_block(body, &promotions);
    }

    true
}

/// Recursively scan a block for `GlobalVarSet` statements that set a promotable global
/// to a scalar constant value.
fn collect_promotions_from_block(
    block: &TirBlock,
    promotable: &IndexMap<GlobalKey, usize>,
    promotions: &mut IndexMap<GlobalKey, TirExprKind>,
) {
    for stmt in &block.stmts {
        collect_promotions_from_stmt(stmt, promotable, promotions);
    }
}

fn collect_promotions_from_stmt(
    stmt: &TirStmt,
    promotable: &IndexMap<GlobalKey, usize>,
    promotions: &mut IndexMap<GlobalKey, TirExprKind>,
) {
    match &stmt.kind {
        TirStmtKind::Expr(expr) => {
            collect_promotions_from_expr(expr, promotable, promotions);
        }
        TirStmtKind::Let { value, .. } => {
            collect_promotions_from_expr(value, promotable, promotions);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_promotions_from_expr(v, promotable, promotions);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_promotions_from_expr(condition, promotable, promotions);
            collect_promotions_from_block(then_block, promotable, promotions);
            if let Some(eb) = else_block {
                collect_promotions_from_block(eb, promotable, promotions);
            }
        }
        TirStmtKind::Loop { body } => {
            collect_promotions_from_block(body, promotable, promotions);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            collect_promotions_from_block(block, promotable, promotions);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_promotions_from_expr(scrutinee, promotable, promotions);
            collect_promotions_from_block(then_block, promotable, promotions);
            if let Some(eb) = else_block {
                collect_promotions_from_block(eb, promotable, promotions);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_promotions_from_expr(v, promotable, promotions);
            }
        }
        TirStmtKind::LetPattern { value, .. } => {
            collect_promotions_from_expr(value, promotable, promotions);
        }
        TirStmtKind::Continue | TirStmtKind::TaskReturn { .. } => {}
    }
}

fn collect_promotions_from_expr(
    expr: &TirExpr,
    promotable: &IndexMap<GlobalKey, usize>,
    promotions: &mut IndexMap<GlobalKey, TirExprKind>,
) {
    if let TirExprKind::GlobalVarSet {
        module_source,
        name,
        value,
    } = &expr.kind
    {
        let key = (module_source.clone(), name.clone());
        if promotable.contains_key(&key) && is_scalar_constant(&value.kind) {
            promotions.entry(key).or_insert_with(|| value.kind.clone());
        }
    }

    // Recurse into sub-expressions that may contain labeled blocks with GlobalVarSet
    visit_expr_children(expr, |child| {
        collect_promotions_from_expr(child, promotable, promotions);
    });
}

/// Recursively remove `GlobalVarSet` statements to promoted globals from a block.
fn remove_promoted_sets_from_block(
    block: &mut TirBlock,
    promotions: &IndexMap<GlobalKey, TirExprKind>,
) {
    // Process nested blocks first
    for stmt in &mut block.stmts {
        remove_promoted_sets_from_stmt(stmt, promotions);
    }

    // Remove GlobalVarSet statements at this level
    block.stmts.retain(|stmt| {
        let TirStmtKind::Expr(expr) = &stmt.kind else {
            return true;
        };
        let TirExprKind::GlobalVarSet {
            module_source,
            name,
            ..
        } = &expr.kind
        else {
            return true;
        };
        let key = (module_source.clone(), name.clone());
        !promotions.contains_key(&key)
    });
}

fn remove_promoted_sets_from_stmt(
    stmt: &mut TirStmt,
    promotions: &IndexMap<GlobalKey, TirExprKind>,
) {
    match &mut stmt.kind {
        TirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            remove_promoted_sets_from_block(then_block, promotions);
            if let Some(eb) = else_block {
                remove_promoted_sets_from_block(eb, promotions);
            }
        }
        TirStmtKind::Loop { body } => {
            remove_promoted_sets_from_block(body, promotions);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            remove_promoted_sets_from_block(block, promotions);
        }
        TirStmtKind::IfPattern {
            then_block,
            else_block,
            ..
        } => {
            remove_promoted_sets_from_block(then_block, promotions);
            if let Some(eb) = else_block {
                remove_promoted_sets_from_block(eb, promotions);
            }
        }
        TirStmtKind::Expr(expr) => {
            remove_promoted_sets_from_expr(expr, promotions);
        }
        TirStmtKind::Let { value, .. } | TirStmtKind::LetPattern { value, .. } => {
            remove_promoted_sets_from_expr(value, promotions);
        }
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                remove_promoted_sets_from_expr(v, promotions);
            }
        }
        TirStmtKind::Continue | TirStmtKind::TaskReturn { .. } => {}
    }
}

fn remove_promoted_sets_from_expr(
    expr: &mut TirExpr,
    promotions: &IndexMap<GlobalKey, TirExprKind>,
) {
    match &mut expr.kind {
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            remove_promoted_sets_from_block(block, promotions);
        }
        TirExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            remove_promoted_sets_from_block(then_branch, promotions);
            if let Some(eb) = else_branch {
                remove_promoted_sets_from_block(eb, promotions);
            }
        }
        _ => {}
    }
}

/// Visit immediate child expressions of an expression (for promotion collection).
fn visit_expr_children(expr: &TirExpr, mut visitor: impl FnMut(&TirExpr)) {
    match &expr.kind {
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            for stmt in &block.stmts {
                visit_stmt_exprs(stmt, &mut visitor);
            }
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            visitor(condition);
            for stmt in &then_branch.stmts {
                visit_stmt_exprs(stmt, &mut visitor);
            }
            if let Some(eb) = else_branch {
                for stmt in &eb.stmts {
                    visit_stmt_exprs(stmt, &mut visitor);
                }
            }
        }
        _ => {}
    }
}

fn visit_stmt_exprs(stmt: &TirStmt, visitor: &mut impl FnMut(&TirExpr)) {
    match &stmt.kind {
        TirStmtKind::Expr(expr) => visitor(expr),
        TirStmtKind::Let { value, .. } | TirStmtKind::LetPattern { value, .. } => visitor(value),
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                visitor(v);
            }
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            for s in &block.stmts {
                visit_stmt_exprs(s, visitor);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            visitor(condition);
            for s in &then_block.stmts {
                visit_stmt_exprs(s, visitor);
            }
            if let Some(eb) = else_block {
                for s in &eb.stmts {
                    visit_stmt_exprs(s, visitor);
                }
            }
        }
        TirStmtKind::Loop { body } => {
            for s in &body.stmts {
                visit_stmt_exprs(s, visitor);
            }
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            visitor(scrutinee);
            for s in &then_block.stmts {
                visit_stmt_exprs(s, visitor);
            }
            if let Some(eb) = else_block {
                for s in &eb.stmts {
                    visit_stmt_exprs(s, visitor);
                }
            }
        }
        TirStmtKind::Continue | TirStmtKind::TaskReturn { .. } => {}
    }
}

fn is_scalar_constant(kind: &TirExprKind) -> bool {
    matches!(
        kind,
        TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
    )
}
