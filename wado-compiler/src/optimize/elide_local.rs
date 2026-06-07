//! Write-only local elimination for Wado NIR.
//!
//! Eliminates `let x = expr;` bindings where the local `x` is never read,
//! never has its address taken, and never escapes via closure capture or a
//! `stores`-aliased call. When `expr` is pure the entire statement is removed;
//! otherwise the binding is replaced by `Expr(expr)` so the side effect still
//! runs.
//!
//! NIR analog of `wir_optimize/elide_local.rs`. Running at NIR exposes the
//! freshly dead expressions to the rest of the fixed-point loop
//! (`copy_prop` / `const_fold` / `dce`), which the WIR-level pass cannot.

use crate::hashmap::IndexSet;
use crate::nir_arena::{BlockId, ExprId, ExprKind, StmtId, StmtKind};
use crate::nir_engine::{Engine, Rule};

use super::arena_query;

/// What to do with a statement that binds / assigns a write-only local.
enum Action {
    /// Keep the statement unchanged.
    Keep,
    /// Drop the statement entirely (its value is pure).
    Drop,
    /// Replace the statement with `Expr(value)` so the side effect still runs.
    Demote(ExprId),
}

pub(super) struct ElideRule<'a> {
    stores_aliased: &'a IndexSet<u32>,
}

impl<'a> ElideRule<'a> {
    /// Build the rule for one function. `stores_aliased` lists params whose
    /// reference escaped via a callee's `stores` declaration: the callee may
    /// retain that reference past its return, so writes through the local stay
    /// observable via the alias and the local must not be elided. It is read
    /// off the function before its body is borrowed for the engine. The other
    /// "kept" source — every live read of a local, including `&local` /
    /// `&mut local` and closure-capture reads — is exactly what the engine use
    /// index records, so the rule reads it directly via `Engine::is_local_read`
    /// rather than a separate walk. (`address_taken_locals` is intentionally
    /// *not* a source: it is a stale static record after `inline` / `ref_elim`,
    /// and source-1 reads already cover every live `&local`.)
    pub(super) fn new(stores_aliased: &'a IndexSet<u32>) -> Self {
        Self { stores_aliased }
    }
}

impl Rule for ElideRule<'_> {
    fn apply_block(&self, engine: &mut Engine, id: BlockId) -> bool {
        let stmts = engine.body.blocks[id].stmts.clone();
        let mut new_stmts = Vec::with_capacity(stmts.len());
        let mut changed = false;
        for stmt in stmts {
            match classify(engine, stmt, self.stores_aliased) {
                Action::Keep => new_stmts.push(stmt),
                Action::Drop => changed = true,
                Action::Demote(value) => {
                    let span = engine.body.stmts[stmt].span;
                    new_stmts.push(engine.alloc_stmt(StmtKind::Expr(value), span));
                    changed = true;
                }
            }
        }
        if changed {
            engine.set_block_stmts(id, new_stmts);
        }
        changed
    }
}

/// Classify a statement for write-only-local elimination. Mirrors the former
/// tree `Elider`: an unread `let x = value` or a bare `x = value` (assign at
/// statement position) where `x` is unread is dropped when `value` is pure,
/// otherwise demoted to `Expr(value)`.
fn classify(engine: &Engine, stmt: StmtId, stores_aliased: &IndexSet<u32>) -> Action {
    match &engine.body.stmts[stmt].kind {
        StmtKind::Let {
            local_index, value, ..
        } => {
            let (idx, value) = (*local_index, *value);
            if is_kept(engine, idx, stores_aliased) {
                Action::Keep
            } else if arena_query::is_pure_expr(engine.body, value) {
                Action::Drop
            } else {
                Action::Demote(value)
            }
        }
        // `x = value;` (Assign at stmt position) where `x` is unread. This
        // catches the SROA / variant-lowering shadow-temp pattern where a pass
        // introduces a local, writes to it via Assign, then a downstream pass
        // folds away the only read site. The matching `let x;` declaration
        // falls out once every write to `x` is gone.
        StmtKind::Expr(e) => {
            let assign = match &engine.body.exprs[*e].kind {
                ExprKind::Assign { target, value } => Some((*target, *value)),
                _ => None,
            };
            if let Some((target, value)) = assign
                && let ExprKind::Local { index, .. } = &engine.body.exprs[target].kind
            {
                let index = *index;
                if !is_kept(engine, index, stores_aliased) {
                    return if arena_query::is_pure_expr(engine.body, value) {
                        Action::Drop
                    } else {
                        Action::Demote(value)
                    };
                }
            }
            Action::Keep
        }
        _ => Action::Keep,
    }
}

/// A local is kept (not elidable) when its reference escaped via a `stores`
/// alias, or it is read anywhere in the body.
fn is_kept(engine: &Engine, local: u32, stores_aliased: &IndexSet<u32>) -> bool {
    stores_aliased.contains(&local) || engine.is_local_read(local)
}
