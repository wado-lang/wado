//! A check bound the loop reloads: the length of a list or string local `L`
//! that the body reassigns whole (`L = h.data`, after a call that may have
//! replaced it). The residual puts `L.used` above every index at entry, and the
//! fast loop keeps it there: after each statement that may lower `L.used`, it
//! re-tests the residual and, failing, ends at that iteration's close, where the
//! original loop takes over with its checks. Every deleted check comes before
//! the first such statement, so no check runs on a bound that went untested.
//!
//! A store `X.used = Y + c` with `let Y = X.used` earlier in the iteration, `X`
//! one local and no reload between, needs no re-test: it raises `X.used` from a
//! value the bound held at, and the runtime keeps a length in `0..2^31`, so the
//! sum does not wrap.

use crate::compiler_item::SeqField;
use crate::hashmap::IndexMap;
use crate::nir::{NirBinaryOp, NirUnaryOp};
use crate::nir_arena::{BlockId, ExprId, ExprKind, NodeRef, Operand, StmtId, StmtKind};
use crate::nir_engine::Engine;
use crate::optimize::arena_query::{binary_parts, operand_local};
use crate::optimize::condition_implication::{
    Binds, BoundKey, FieldPath, parse_bound, parse_const_i64,
};
use crate::optimize::heap_effect::{Effect, HeapEffects, HeapFrame, LazyHeapFrame};
use crate::tir::TypeId;

use super::subtree_redefines;
use crate::type_table::{TypeKey, TypeTable};

/// What a subtree does to the reloaded bound.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Reach {
    Clean,
    /// May lower `L.used`: a re-test follows the statement.
    Lowers,
    /// Writes `L` itself somewhere no re-test can follow.
    Refused,
}

struct Walk<'w> {
    binds: &'w Binds,
    effects: &'w HeapEffects<'w>,
    frame: &'w HeapFrame,
    types: &'w TypeTable,
    root: u32,
    key: TypeKey,
    loop_body: BlockId,
    /// The holders of the checks the fast loop deletes.
    checks: &'w [NodeRef],
    /// Each local a `let` bound to `X.used`, with `X` and the reload count then.
    reads: IndexMap<u32, (u32, u32)>,
    reloads: u32,
    retested: bool,
    points: Vec<(BlockId, StmtId)>,
}

/// The statements of `loop_body`, past the guard at `guard_idx`, after which
/// the fast loop re-tests the bound `root.used`, each with its block. `None`
/// when a check follows one, `root` is written other than by a reload, or the
/// loop has an exit besides its guard: the original loop resumes after the
/// fast one, which another exit would skip.
pub(super) fn points(
    engine: &Engine,
    binds: &Binds,
    heap: &LazyHeapFrame,
    types: &TypeTable,
    root: u32,
    loop_body: BlockId,
    guard_idx: usize,
    checks: &[NodeRef],
) -> Option<Vec<(BlockId, StmtId)>> {
    let root_ty = engine.locals().get(root as usize)?.type_id;
    if !types.is_seq_container(types.peel_refs(root_ty))
        || subtree_redefines(engine, loop_body, &[root])
    {
        return None;
    }
    let mut exits = 0;
    engine
        .body
        .for_each_node_under(NodeRef::Block(loop_body), |n| {
            if let NodeRef::Stmt(s) = n
                && matches!(
                    engine.body.stmts[s].kind,
                    StmtKind::Break { label: None, .. }
                )
            {
                exits += 1;
            }
        });
    if exits != 1 {
        return None;
    }
    let mut walk = Walk {
        binds,
        effects: heap.effects,
        frame: heap.get(engine.body),
        types,
        root,
        key: types.type_key(types.peel_refs(root_ty)),
        loop_body,
        checks,
        reads: IndexMap::default(),
        reloads: 0,
        retested: false,
        points: Vec::new(),
    };
    let stmts = engine.body.blocks[loop_body].stmts.clone();
    walk.stmts(engine, loop_body, &stmts[guard_idx + 1..])?;
    Some(walk.points)
}

impl Walk<'_> {
    fn stmts(&mut self, engine: &Engine, block: BlockId, stmts: &[StmtId]) -> Option<()> {
        for &s in stmts {
            self.stmt(engine, block, s)?;
        }
        Some(())
    }

    fn stmt(&mut self, engine: &Engine, block: BlockId, s: StmtId) -> Option<()> {
        let body = &*engine.body;
        if self.retested && self.holds_check(engine, s) {
            return None;
        }
        let reach = match &body.stmts[s].kind {
            StmtKind::Let {
                local_index, value, ..
            } => {
                let reach = self.scan(engine, NodeRef::Stmt(s))?;
                if let Some(x) = self.len_read(engine, *value) {
                    self.reads.insert(*local_index, (x, self.reloads));
                }
                reach
            }
            StmtKind::Expr(Operand::Expr(e)) if self.reloaded_by(engine, *e) => {
                let ExprKind::Assign { value, .. } = &body.exprs[*e].kind else {
                    unreachable!("a reload is an `Assign`");
                };
                if let Some(v) = value.as_expr() {
                    self.scan(engine, NodeRef::Expr(v))?;
                }
                self.reloads += 1;
                Reach::Lowers
            }
            StmtKind::Expr(Operand::Expr(e)) if self.raises(engine, *e) => {
                let ExprKind::Assign { target, value } = &body.exprs[*e].kind else {
                    unreachable!("a raise is an `Assign`");
                };
                let target = self.scan(engine, NodeRef::Expr(*target))?;
                let value = value
                    .as_expr()
                    .map_or(Some(Reach::Clean), |v| self.scan(engine, NodeRef::Expr(v)))?;
                // The target's own store is the raise; the scans see only what
                // computes the place and the value.
                target.max(value)
            }
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                let condition = condition
                    .as_expr()
                    .map_or(Some(Reach::Clean), |c| self.scan(engine, NodeRef::Expr(c)))?;
                if condition == Reach::Clean {
                    for arm in std::iter::once(*then_block).chain(*else_block) {
                        let arm_stmts = body.blocks[arm].stmts.clone();
                        self.stmts(engine, arm, &arm_stmts)?;
                    }
                    return Some(());
                }
                condition
            }
            StmtKind::LabeledBlock { block: inner, .. } => {
                let inner_stmts = body.blocks[*inner].stmts.clone();
                return self.stmts(engine, *inner, &inner_stmts);
            }
            StmtKind::Expr(_)
            | StmtKind::Return { .. }
            | StmtKind::Loop { .. }
            | StmtKind::Break { .. }
            | StmtKind::Continue
            | StmtKind::LetDestructure { .. } => self.scan(engine, NodeRef::Stmt(s))?,
        };
        if reach == Reach::Lowers {
            // A check inside the statement may run after what lowers the bound.
            if self.holds_check(engine, s) {
                return None;
            }
            self.points.push((block, s));
            self.retested = true;
        }
        Some(())
    }

    fn holds_check(&self, engine: &Engine, s: StmtId) -> bool {
        engine
            .body
            .find_in_nodes_under(NodeRef::Stmt(s), |n| self.checks.contains(&n).then_some(()))
            .is_some()
    }

    /// `root = …`, the whole statement.
    fn reloaded_by(&self, engine: &Engine, e: ExprId) -> bool {
        matches!(&engine.body.exprs[e].kind, ExprKind::Assign { target, .. }
            if self.local_of(engine, *target) == Some(self.root))
    }

    /// `X.used = Y + c`, `c >= 0`, with `Y` read as `X.used` since the last
    /// reload and `X` a local the loop holds still.
    fn raises(&self, engine: &Engine, e: ExprId) -> bool {
        let body = &*engine.body;
        let ExprKind::Assign { target, value } = &body.exprs[e].kind else {
            return false;
        };
        let Some(x) = self.len_place(engine, *target) else {
            return false;
        };
        let (y, c) = match binary_parts(body, *value) {
            Some((left, NirBinaryOp::Add, right)) => {
                let (Some(y), Some(c)) = (
                    operand_local(body, left),
                    parse_const_i64(engine, self.binds, right),
                ) else {
                    return false;
                };
                (y, c)
            }
            _ => match operand_local(body, *value) {
                Some(y) => (y, 0),
                None => return false,
            },
        };
        c >= 0
            && self.reads.get(&y) == Some(&(x, self.reloads))
            && self.slot_writes(engine, x) == 0
            && self.slot_writes(engine, y) == 1
    }

    /// How many times the loop binds `local`'s slot: by a `let`, a whole
    /// assign, or a `&mut` of the bare local.
    fn slot_writes(&self, engine: &Engine, local: u32) -> usize {
        let body = &*engine.body;
        let mut writes = 0;
        body.for_each_node_under(NodeRef::Block(self.loop_body), |n| match n {
            NodeRef::Stmt(s) => {
                if matches!(&body.stmts[s].kind, StmtKind::Let { local_index, .. } if *local_index == local)
                    || matches!(&body.stmts[s].kind, StmtKind::LetDestructure { .. })
                {
                    writes += 1;
                }
            }
            NodeRef::Expr(e) => {
                let place = match &body.exprs[e].kind {
                    ExprKind::Assign { target, .. } => Some(Operand::Expr(*target)),
                    ExprKind::Unary {
                        op: NirUnaryOp::MutRef,
                        expr,
                    } => Some(*expr),
                    _ => None,
                };
                if place.is_some_and(|p| operand_local(body, p) == Some(local)) {
                    writes += 1;
                }
            }
            NodeRef::Block(_) | NodeRef::Pat(_) => {}
        });
        writes
    }

    /// The local `X` of a read `X.used` of the bound's type. The read itself,
    /// not a copy temp holding one: a temp may hold a read from before a reload.
    fn len_read(&self, engine: &Engine, value: Operand) -> Option<u32> {
        let BoundKey::Field(x, path) = parse_bound(engine, &Binds::default(), value)? else {
            return None;
        };
        let ty = engine.locals().get(x as usize)?.type_id;
        (path == FieldPath::of(SeqField::Len.index()) && self.of_key(ty)).then_some(x)
    }

    /// The local `X` of a place `X.used` of the bound's type.
    fn len_place(&self, engine: &Engine, place: ExprId) -> Option<u32> {
        let ExprKind::FieldAccess {
            expr, field_index, ..
        } = &engine.body.exprs[place].kind
        else {
            return None;
        };
        (*field_index == SeqField::Len.index() && self.of_key(engine.body.operand_type(*expr)))
            .then(|| operand_local(engine.body, *expr))?
    }

    fn local_of(&self, engine: &Engine, e: ExprId) -> Option<u32> {
        operand_local(engine.body, Operand::Expr(e))
    }

    fn of_key(&self, ty: TypeId) -> bool {
        self.types.type_key(self.types.peel_refs(ty)) == self.key
    }

    /// The worst `node`'s subtree does to the bound, `None` for
    /// [`Reach::Refused`].
    fn scan(&self, engine: &Engine, node: NodeRef) -> Option<Reach> {
        let body = &*engine.body;
        let mut reach = Reach::Clean;
        body.for_each_node_under(node, |n| {
            let NodeRef::Expr(e) = n else { return };
            let here = match &body.exprs[e].kind {
                ExprKind::Assign { target, .. } => {
                    if self.local_of(engine, *target) == Some(self.root) {
                        Reach::Refused
                    } else if self.len_place(engine, *target).is_some()
                        || matches!(
                            &body.exprs[*target].kind,
                            ExprKind::Unary {
                                op: NirUnaryOp::Deref,
                                ..
                            }
                        ) && self.of_key(body.exprs[*target].type_id)
                    {
                        Reach::Lowers
                    } else {
                        Reach::Clean
                    }
                }
                ExprKind::Unary {
                    op: NirUnaryOp::MutRef,
                    expr,
                } if operand_local(body, *expr) == Some(self.root) => Reach::Refused,
                ExprKind::Call { .. } | ExprKind::IndirectCall { .. } => {
                    if self.frame.call_may(
                        self.effects,
                        body,
                        e,
                        Effect::Write,
                        self.key,
                        self.root,
                    ) {
                        Reach::Lowers
                    } else {
                        Reach::Clean
                    }
                }
                ExprKind::CmRawCall { .. } => Reach::Lowers,
                _ => Reach::Clean,
            };
            reach = reach.max(here);
        });
        (reach != Reach::Refused).then_some(reach)
    }
}
