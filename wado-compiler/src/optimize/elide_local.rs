//! Write-only local elimination for Wado NIR: a `let x = expr;` whose `x` is
//! never read, address-taken, or escaped loses its binding — the whole statement
//! when `expr` is pure, else a bare `Expr(expr)`. The NIR analog of
//! `wir_optimize/elide_local.rs`; running here exposes the freshly dead
//! expressions to the rest of the fixed-point loop, which the WIR pass cannot.

use crate::hashmap::IndexSet;
use crate::nir_arena::{BlockId, ExprId, ExprKind, Operand, StmtId, StmtKind};
use crate::nir_engine::{Engine, Rule};

use super::arena_query;
use crate::optimize::dce::deletable_value;
use crate::optimize::mod_ref::CallFacts;

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
    /// What this body's calls may do, so a dead binding whose value is a call
    /// can still go: the structural purity predicate refuses every call on
    /// sight, while a summary can prove one has no effect and cannot trap.
    calls: CallFacts<'a>,
}

impl<'a> ElideRule<'a> {
    /// Build the rule for one function. `stores_aliased` lists params whose
    /// reference a callee may retain past its return, keeping writes through the
    /// local observable, so those are never elided; it is read off the function
    /// before the engine borrows its body. Every other live read comes from
    /// `Engine::is_local_read`, so `address_taken_locals` — stale after `inline`
    /// / `ref_elim` — is deliberately not consulted.
    pub(super) fn new(stores_aliased: &'a IndexSet<u32>, calls: CallFacts<'a>) -> Self {
        Self {
            stores_aliased,
            calls,
        }
    }
}

impl Rule for ElideRule<'_> {
    fn apply_block(&self, engine: &mut Engine, id: BlockId) -> bool {
        let stmts = engine.body.blocks[id].stmts.clone();
        let mut new_stmts = Vec::with_capacity(stmts.len());
        let mut changed = false;
        let mut elided: Vec<u32> = Vec::new();
        let len = stmts.len();
        for (i, stmt) in stmts.into_iter().enumerate() {
            let is_tail = i + 1 == len;
            match classify(engine, stmt, is_tail, self) {
                Action::Keep => new_stmts.push(stmt),
                Action::Drop => {
                    elided.extend(bound_local(engine, stmt));
                    changed = true;
                }
                Action::Demote(value) => {
                    elided.extend(bound_local(engine, stmt));
                    let span = engine.body.stmts[stmt].span;
                    new_stmts.push(engine.alloc_stmt(StmtKind::Expr(value.into()), span));
                    changed = true;
                }
            }
        }
        if changed {
            engine.set_block_stmts(id, new_stmts);
        }
        // The session end audits these; checking here would walk the whole body
        // per block. See `Engine::note_elided_local`.
        for local in elided {
            engine.note_elided_local(local);
        }
        changed
    }
}

/// The local a statement binds or assigns, which elision leaves without a
/// definition.
fn bound_local(engine: &Engine, stmt: StmtId) -> Option<u32> {
    match &engine.body.stmts[stmt].kind {
        StmtKind::Let { local_index, .. } => Some(*local_index),
        StmtKind::Expr(Operand::Expr(e)) => match &engine.body.exprs[*e].kind {
            ExprKind::Assign { target, .. } => match &engine.body.exprs[*target].kind {
                ExprKind::Local { index, .. } => Some(*index),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

/// Classify a statement for write-only-local elimination. Mirrors the former
/// tree `Elider`: an unread `let x = value` or a bare `x = value` (assign at
/// statement position) where `x` is unread is dropped when `value` is pure,
/// otherwise demoted to `Expr(value)`.
fn classify(engine: &Engine, stmt: StmtId, is_tail: bool, rule: &ElideRule) -> Action {
    match &engine.body.stmts[stmt].kind {
        StmtKind::Let {
            local_index, value, ..
        } => {
            let (idx, value) = (*local_index, *value);
            if is_kept(engine, idx, rule.stores_aliased) {
                Action::Keep
            } else if deletable(engine, value, rule) {
                Action::Drop
            } else {
                // Effectful or trap-capable. A skeleton expr demotes to a bare
                // `Expr(value)`, which still runs it; a promoted value has no
                // `ExprId` to demote to, so its binding is what runs it.
                value.as_expr().map_or(Action::Keep, Action::Demote)
            }
        }
        // `x = value;` (Assign at stmt position) where `x` is unread. This
        // catches the SROA / variant-lowering shadow-temp pattern where a pass
        // introduces a local, writes to it via Assign, then a downstream pass
        // folds away the only read site. The matching `let x;` declaration
        // falls out once every write to `x` is gone.
        StmtKind::Expr(operand @ Operand::Value(_)) => {
            if !is_tail && deletable(engine, *operand, rule) {
                return Action::Drop;
            }
            Action::Keep
        }
        StmtKind::Expr(Operand::Expr(e)) => {
            let assign = match &engine.body.exprs[*e].kind {
                ExprKind::Assign { target, value } => Some((*target, *value)),
                _ => None,
            };
            // Not in tail position — a block's tail `Expr` is its value.
            if assign.is_none() && !is_tail && deletable(engine, Operand::Expr(*e), rule) {
                return Action::Drop;
            }
            if let Some((target, value)) = assign
                && let ExprKind::Local { index, .. } = &engine.body.exprs[target].kind
            {
                let index = *index;
                if !is_kept(engine, index, rule.stores_aliased) {
                    return if deletable(engine, value, rule) {
                        Action::Drop
                    } else {
                        // Effectful or trap-capable; see the `Let` arm above for
                        // why a promoted value is kept rather than demoted.
                        value.as_expr().map_or(Action::Keep, Action::Demote)
                    };
                }
            }
            Action::Keep
        }
        _ => Action::Keep,
    }
}

/// Whether the value of a dead binding may go with it: no observable effect and
/// no trap, since a trap is observable too. With a type table this is
/// [`super::dce::deletable_value`], which answers a call by its summary: that is
/// what lets a dead call to a pure helper leave rather than linger as a
/// `drop(f(x))`. Without one only the structural predicate can answer.
fn deletable(engine: &Engine, value: Operand, rule: &ElideRule) -> bool {
    match engine.value_graph_type_table() {
        Some(types) => deletable_value(engine.body, value, types, rule.calls),
        None => arena_query::is_pure_nontrapping_operand_typed(engine.body, value, None),
    }
}

/// A local is kept (not elidable) when a retaining callee aliased its
/// reference, or it is read anywhere in the body.
fn is_kept(engine: &Engine, local: u32, stores_aliased: &IndexSet<u32>) -> bool {
    stores_aliased.contains(&local)
        || engine.is_local_read(local)
        || engine.reads_promoted_local(local)
}

#[cfg(test)]
mod tests {
    use super::*;

    use cranelift_entity::EntityRef;

    use crate::call_args::CallArgs;
    use crate::nir::{FuncId, NirLocal};
    use crate::nir_arena::{BlockNode, Body, ExprNode, StmtNode};
    use crate::nir_engine::EngineBuffers;
    use crate::optimize::bounds::Proofs;
    use crate::optimize::mod_ref::FnEffect;
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
        let proofs = Proofs::default();
        // No callees in these bodies, so the summaries are unused.
        let calls = CallFacts {
            effects: &[],
            proofs: &proofs,
        };
        let rule = ElideRule::new(&stores_aliased, calls);
        let mut buffers = EngineBuffers::default();
        let mut engine = Engine::new(body, &mut buffers, locals);
        engine.run(&[&rule]);
    }

    /// `let x = f();` where `f`, function 0, may trap by its summary. The call
    /// carries a proof made against `proven_callee`, if any. Answers the
    /// root's statements.
    fn elide_binding_of_trapping_call(proven_callee: Option<usize>) -> Vec<StmtKind> {
        let mut body = Body::empty();
        let call = body.exprs.push(ExprNode {
            kind: ExprKind::Call {
                func_id: FuncId::new(0),
                type_args: vec![],
                args: CallArgs::free(vec![]),
            },
            type_id: TypeTable::I32,
            span: Span::default(),
        });
        let binding = body.stmts.push(StmtNode {
            kind: StmtKind::Let {
                name: "x".to_string(),
                local_index: 0,
                is_mut: false,
                type_id: TypeTable::I32,
                value: Operand::Expr(call),
                skip_value_copy: false,
            },
            span: Span::default(),
        });
        body.root = body.blocks.push(BlockNode {
            stmts: vec![binding],
            span: Span::default(),
        });
        let mut locals = vec![NirLocal {
            name: "x".to_string(),
            type_id: TypeTable::I32,
            is_mut: false,
        }];
        let effects = [FnEffect {
            may_trap: true,
            ..FnEffect::default()
        }];
        let stores_aliased = IndexSet::default();
        let proofs = Proofs::from_iter(proven_callee.map(|f| (call, FuncId::new(f))));
        let types = TypeTable::new();
        let calls = CallFacts {
            effects: &effects,
            proofs: &proofs,
        };
        let rule = ElideRule::new(&stores_aliased, calls);
        let mut buffers = EngineBuffers::default();
        let mut engine = Engine::new(&mut body, &mut buffers, &mut locals);
        engine.set_value_graph_type_table(&types);
        engine.run(&[&rule]);
        body.blocks[body.root]
            .stmts
            .iter()
            .map(|s| body.stmts[*s].kind.clone())
            .collect()
    }

    #[test]
    fn drops_a_dead_call_whose_trap_checks_are_proven() {
        assert!(elide_binding_of_trapping_call(Some(0)).is_empty());
    }

    #[test]
    fn demotes_a_dead_call_that_may_trap() {
        for proven_callee in [None, Some(1)] {
            let stmts = elide_binding_of_trapping_call(proven_callee);
            assert!(
                matches!(stmts.as_slice(), [StmtKind::Expr(Operand::Expr(_))]),
                "a call proven against {proven_callee:?} still runs, for its trap: {stmts:?}"
            );
        }
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

    /// A statement whose operand is a promoted value carries no binding, so the
    /// only thing keeping it alive is the statement itself.
    #[test]
    fn drops_a_promoted_value_standing_alone_as_a_statement() {
        let mut body = Body::empty();
        let dead = body.values.int_typed(1, TypeTable::I32);
        let tail = body.values.int_typed(2, TypeTable::I32);
        let stmts = [dead, tail]
            .map(|v| {
                body.stmts.push(StmtNode {
                    kind: StmtKind::Expr(Operand::Value(v)),
                    span: Span::default(),
                })
            })
            .to_vec();
        body.root = body.blocks.push(BlockNode {
            stmts,
            span: Span::default(),
        });
        let mut locals = Vec::new();
        run_elide(&mut body, &mut locals);
        assert_eq!(
            body.blocks[body.root].stmts.len(),
            1,
            "only the tail statement, which is the block's value, should remain"
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
