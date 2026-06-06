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
use crate::nir::{NirBlock, NirExpr, NirExprKind, NirStmtKind};
use crate::nir_arena::{BlockId, ExprId, ExprKind, StmtId, StmtKind};
use crate::nir_engine::{Engine, Rule};
use crate::nir_package::NirPackage;
use crate::nir_visitor::NirRefVisitor;

use super::arena_query;

pub fn elide_write_only_locals(project: &mut NirPackage) -> bool {
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        // `stores_aliased_locals` — params whose reference escaped via a
        // callee's `stores` declaration. The callee may retain that reference
        // past its return, so writes through the local stay observable via the
        // alias and the local must not be elided. Read it off the function
        // before borrowing the body. The other "kept" source — every live read
        // of a local, including `&local` / `&mut local` and closure-capture
        // reads — is exactly what the engine use index records, so the rule
        // reads it directly via `Engine::is_local_read` rather than a separate
        // walk. (`address_taken_locals` is intentionally *not* a source: it is
        // a stale static record after `inline` / `ref_elim`, and source-1 reads
        // already cover every live `&local`.)
        let stores_aliased = func.stores_aliased_locals.clone();
        if let Some(body) = func.body.as_mut() {
            let mut engine = Engine::new(body);
            let rule = ElideRule {
                stores_aliased: &stores_aliased,
            };
            changed |= engine.run(&[&rule]);
        }
    }
    changed
}

/// What to do with a statement that binds / assigns a write-only local.
enum Action {
    /// Keep the statement unchanged.
    Keep,
    /// Drop the statement entirely (its value is pure).
    Drop,
    /// Replace the statement with `Expr(value)` so the side effect still runs.
    Demote(ExprId),
}

struct ElideRule<'a> {
    stores_aliased: &'a IndexSet<u32>,
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

/// Tree-walking read collector: inserts every local that is read, treating a
/// bare-`Local` `Assign` target as a write (not a read) but recursing into
/// nested write places (`a.field = …`, `a[i] = …`) and the assigned value.
/// `&local` / `&mut local` count as reads via the default walk. Kept for the
/// tree consumers (`dae`); the engine rule above uses `Engine::is_local_read`.
struct ReadCollector<'a> {
    kept: &'a mut IndexSet<u32>,
}

impl NirRefVisitor for ReadCollector<'_> {
    fn visit_expr(&mut self, expr: &NirExpr) {
        match &expr.kind {
            NirExprKind::Local { index, .. } => {
                self.kept.insert(*index);
                return;
            }
            NirExprKind::Assign { target, value } => {
                if !matches!(target.kind, NirExprKind::Local { .. }) {
                    self.visit_expr(target);
                }
                self.visit_expr(value);
                return;
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}

/// Public helper used by `dae` to collect locals that the function body reads
/// (or whose addresses escape via captures). Insertion is done by `ReadCollector`.
pub(super) fn collect_reads_in_block(block: &NirBlock, out: &mut IndexSet<u32>) {
    let mut collector = ReadCollector { kept: out };
    collector.visit_block(block);
}

/// True when `expr` and every sub-expression has no observable effect.
///
/// Conservative — calls, global writes, assignments, closure construction,
/// and control-flow constructs whose branches are themselves impure are
/// treated as impure. Pure reads (`Local`, `GlobalVarGet`, `FieldAccess`,
/// `Index`), arithmetic, and reference-taking (`&x` / `&mut x`) are pure
/// since the *act* of taking a reference does not mutate; only writing
/// through the resulting reference would, and that shows up as a separate
/// `Assign` / call. Mirrors the WIR-level `is_side_effect_free` contract.
pub(super) fn is_pure_expr(expr: &NirExpr) -> bool {
    match &expr.kind {
        NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::Local { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::EnumConstruct { .. } => true,
        NirExprKind::Binary { left, right, .. } => is_pure_expr(left) && is_pure_expr(right),
        NirExprKind::Unary { expr: inner, op } => {
            // `&mut x` is a pure root by itself, but only meaningful when the
            // resulting reference is used; an unused MutRef has no observable
            // effect on the local because nothing reads/writes through it.
            let _ = op;
            is_pure_expr(inner)
        }
        NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. } => is_pure_expr(inner),
        NirExprKind::Index { expr: e, index: i } => is_pure_expr(e) && is_pure_expr(i),
        NirExprKind::StructLiteral { fields, .. } => fields.iter().all(|f| is_pure_expr(&f.value)),
        NirExprKind::TupleLiteral { elements } | NirExprKind::ArrayLiteral { elements } => {
            elements.iter().all(is_pure_expr)
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            payload.as_ref().is_none_or(|p| is_pure_expr(p))
        }
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => is_pure_block(block),
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            is_pure_expr(condition)
                && is_pure_block(then_branch)
                && else_branch.as_ref().is_none_or(is_pure_block)
        }
        // Calls, mutations, closures, control-flow exits, and anything that
        // could suspend are conservatively impure.
        _ => false,
    }
}

fn is_pure_block(block: &NirBlock) -> bool {
    block.stmts.iter().all(|s| match &s.kind {
        NirStmtKind::Expr(e) | NirStmtKind::Let { value: e, .. } => is_pure_expr(e),
        _ => false,
    })
}
