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

use std::cell::OnceCell;

use crate::hashmap::IndexSet;
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, Operand, StmtId, StmtKind};
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
    /// Whole-function effect summaries, so a dead binding whose value is a call
    /// can still go: the structural purity predicate refuses every call on
    /// sight, while a summary can prove one has no effect and cannot trap.
    effects: &'a [super::mod_ref::FnEffect],
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
    pub(super) fn new(
        stores_aliased: &'a IndexSet<u32>,
        effects: &'a [super::mod_ref::FnEffect],
    ) -> Self {
        Self {
            stores_aliased,
            effects,
        }
    }
}

impl Rule for ElideRule<'_> {
    fn apply_block(&self, engine: &mut Engine, id: BlockId) -> bool {
        let stmts = engine.body.blocks[id].stmts.clone();
        // Locals read only through a promoted `Operand::Value` are live but
        // invisible to the use index, so keep them. Recomputed per block
        // application, since a rewrite elsewhere in the run can promote a fresh
        // read into the skeleton.
        let promoted_reads = PromotedReads::default();
        let mut new_stmts = Vec::with_capacity(stmts.len());
        let mut changed = false;
        let len = stmts.len();
        for (i, stmt) in stmts.into_iter().enumerate() {
            let is_tail = i + 1 == len;
            match classify(
                engine,
                stmt,
                is_tail,
                self.stores_aliased,
                self.effects,
                &promoted_reads,
            ) {
                Action::Keep => new_stmts.push(stmt),
                Action::Drop => changed = true,
                Action::Demote(value) => {
                    let span = engine.body.stmts[stmt].span;
                    new_stmts.push(engine.alloc_stmt(StmtKind::Expr(value.into()), span));
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
fn classify(
    engine: &Engine,
    stmt: StmtId,
    is_tail: bool,
    stores_aliased: &IndexSet<u32>,
    effects: &[super::mod_ref::FnEffect],
    promoted_reads: &PromotedReads,
) -> Action {
    match &engine.body.stmts[stmt].kind {
        StmtKind::Let {
            local_index, value, ..
        } => {
            let (idx, value) = (*local_index, *value);
            if is_kept(engine, idx, stores_aliased, promoted_reads) {
                Action::Keep
            } else if deletable(engine, value, effects) {
                Action::Drop
            } else {
                // Effectful or trap-capable ⟹ a skeleton expr (a promoted value
                // is pure and non-trapping → Drop above). Demote keeps it as a
                // bare `Expr(value)` so any trap it carries still fires.
                value.as_expr().map_or(Action::Drop, Action::Demote)
            }
        }
        // `x = value;` (Assign at stmt position) where `x` is unread. This
        // catches the SROA / variant-lowering shadow-temp pattern where a pass
        // introduces a local, writes to it via Assign, then a downstream pass
        // folds away the only read site. The matching `let x;` declaration
        // falls out once every write to `x` is gone.
        StmtKind::Expr(Operand::Expr(e)) => {
            let assign = match &engine.body.exprs[*e].kind {
                ExprKind::Assign { target, value } => Some((*target, *value)),
                _ => None,
            };
            // Not in tail position — a block's tail `Expr` is its value.
            if assign.is_none()
                && !is_tail
                && arena_query::is_pure_nontrapping_operand_typed(
                    engine.body,
                    Operand::Expr(*e),
                    engine.value_graph_type_table(),
                )
            {
                return Action::Drop;
            }
            if let Some((target, value)) = assign
                && let ExprKind::Local { index, .. } = &engine.body.exprs[target].kind
            {
                let index = *index;
                if !is_kept(engine, index, stores_aliased, promoted_reads) {
                    return if deletable(engine, value, effects) {
                        Action::Drop
                    } else {
                        // Effectful or trap-capable ⟹ a skeleton expr (a promoted
                        // value is pure and non-trapping → Drop above).
                        value.as_expr().map_or(Action::Drop, Action::Demote)
                    };
                }
            }
            Action::Keep
        }
        _ => Action::Keep,
    }
}

/// Whether the value of a dead binding may go with it: no observable effect and
/// no trap, since a trap is observable too. The structural predicate answers
/// first; a call it refuses on sight is answered by its whole-function summary
/// ([`super::dce::deletable_value`]), which is what lets a dead call to a pure
/// helper leave rather than linger as a `drop(f(x))`.
fn deletable(engine: &Engine, value: Operand, effects: &[super::mod_ref::FnEffect]) -> bool {
    if arena_query::is_pure_nontrapping_operand_typed(
        engine.body,
        value,
        engine.value_graph_type_table(),
    ) {
        return true;
    }
    engine
        .value_graph_type_table()
        .is_some_and(|types| super::dce::deletable_value(engine.body, value, types, effects))
}

/// A local is kept (not elidable) when its reference escaped via a `stores`
/// alias, or it is read anywhere in the body.
fn is_kept(
    engine: &Engine,
    local: u32,
    stores_aliased: &IndexSet<u32>,
    promoted_reads: &PromotedReads,
) -> bool {
    stores_aliased.contains(&local)
        || engine.is_local_read(local)
        || promoted_reads.contains(engine.body, local)
}

/// The locals a reachable promoted operand reads
/// ([`arena_query::promoted_local_reads`]), computed on first query.
///
/// [`is_kept`] reaches it only after the escape set and the use index both come
/// up empty — that is, only for a statement about to be elided — so a block
/// with nothing to elide never pays for the walk.
#[derive(Default)]
struct PromotedReads(OnceCell<IndexSet<u32>>);

impl PromotedReads {
    fn contains(&self, body: &Body, local: u32) -> bool {
        self.0
            .get_or_init(|| {
                let mut out = IndexSet::default();
                arena_query::promoted_local_reads(body, &mut out);
                out
            })
            .contains(&local)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::nir::NirLocal;
    use crate::nir_arena::{BlockNode, StmtNode};
    use crate::nir_engine::EngineBuffers;
    use crate::tir::TypeTable;
    use crate::token::Span;

    /// `let x = 1;` at the root, with a promoted `Opaque(Local x)` interned in
    /// the pool. `extra_read` decides whether a statement still carries that
    /// value as an operand — i.e. whether the read is reachable or stale
    /// residue of a fold.
    fn body_with_promoted_read(extra_read: bool) -> (Body, Vec<NirLocal>) {
        let mut body = Body::empty();
        let one = body.values.int_typed(1, TypeTable::I32);
        let read = body.values.canonical_local(0, TypeTable::I32);
        let binding = body.stmts.push(StmtNode {
            kind: StmtKind::Let {
                name: "x".to_string(),
                local_index: 0,
                is_mut: false,
                is_reactive: false,
                type_id: TypeTable::I32,
                value: Operand::Value(one),
                skip_value_copy: false,
            },
            span: Span::default(),
        });
        let mut stmts = vec![binding];
        if extra_read {
            stmts.push(body.stmts.push(StmtNode {
                kind: StmtKind::Expr(Operand::Value(read)),
                span: Span::default(),
            }));
        }
        body.root = body.blocks.push(BlockNode {
            stmts,
            span: Span::default(),
        });
        let locals = vec![NirLocal {
            name: "x".to_string(),
            type_id: TypeTable::I32,
            is_mut: false,
        }];
        (body, locals)
    }

    fn run_elide(body: &mut Body, locals: &mut Vec<NirLocal>) {
        let stores_aliased = IndexSet::default();
        // No callees in these bodies, so the summaries are unused.
        let rule = ElideRule::new(&stores_aliased, &[]);
        let mut buffers = EngineBuffers::default();
        let mut engine = Engine::new(body, &mut buffers, locals);
        engine.run(&[&rule]);
    }

    #[test]
    fn elides_a_binding_whose_promoted_read_no_operand_carries() {
        let (mut body, mut locals) = body_with_promoted_read(false);
        run_elide(&mut body, &mut locals);
        assert!(
            body.blocks[body.root].stmts.is_empty(),
            "the pool still names local 0, but no reachable operand reads it"
        );
    }

    #[test]
    fn keeps_a_binding_read_through_a_reachable_promoted_operand() {
        let (mut body, mut locals) = body_with_promoted_read(true);
        run_elide(&mut body, &mut locals);
        assert_eq!(
            body.blocks[body.root].stmts.len(),
            2,
            "a read living in the value pool is still a read"
        );
    }
}
