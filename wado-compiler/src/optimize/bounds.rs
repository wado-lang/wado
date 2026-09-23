//! Bounds a body establishes on its own, without a value graph: the range of an
//! integer operand, the length of a GC array, and which loops count up to a
//! constant. [`FnEffect`](super::mod_ref::FnEffect) reads them to clear the trap
//! of a builtin whose `#[trap(...)]` checks all hold, and the divergence of a
//! loop that runs out.

use crate::const_eval;
use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{FuncId, NirBinaryOp, NirUnaryOp};
use crate::nir_arena::{
    BlockId, Body, ExprId, ExprKind, NodeRef, Operand, PatKind, StmtId, StmtKind,
};
use crate::nir_value_graph::ValueKind;
use crate::optimize::arena_query::{binary_parts, local_written_by, operand_local, storage_root};
use crate::primitive::PrimitiveType;
use crate::tir::{ResolvedType, TrapCheck, TrapSpec, TypeTable};

/// What a bodyless builtin declared, as a call to it is read here.
#[derive(Debug, Clone, Copy)]
pub(super) struct Builtin<'a> {
    /// `#[trap(...)]`; `None` is "may trap".
    pub trap: Option<&'a TrapSpec<usize>>,
    /// The positions it takes by `&mut`.
    pub mut_params: &'a IndexSet<usize>,
    /// `#[result(owned)]`.
    pub owned: bool,
}

impl Builtin<'_> {
    /// Positions of arrays whose bounds the call checks, and so leaves in place.
    fn checked_arrays(&self) -> impl Iterator<Item = usize> + '_ {
        self.trap.into_iter().flat_map(|spec| {
            spec.checks.iter().filter_map(|check| match check {
                TrapCheck::Outside { array, .. } => Some(*array),
                TrapCheck::Negative(_) | TrapCheck::Unset(_) => None,
            })
        })
    }
}

/// What [`analyze`] proves about one body.
#[derive(Debug, Default)]
pub(super) struct Bounds {
    /// Builtin calls whose every `#[trap(...)]` check holds.
    pub in_bounds: IndexSet<ExprId>,
    /// `Loop` statements that count a local up to a constant, so they end.
    pub counted: IndexSet<StmtId>,
    /// Some store reaches heap memory this body did not allocate itself.
    pub writes_shared_heap: bool,
}

/// Inclusive integer range.
type Range = (i64, i64);

/// Bind-chain and arithmetic depth cap: exceeding it forgoes a proof, never
/// makes a wrong one.
const MAX_DEPTH: u32 = 16;

pub(super) fn analyze<'b>(
    body: &Body,
    types: &TypeTable,
    builtin: impl Fn(FuncId) -> Option<Builtin<'b>>,
) -> Bounds {
    let mut scan = Scan {
        body,
        types,
        builtin,
        lets: IndexMap::default(),
        writes: IndexMap::default(),
        counters: Vec::new(),
        out: Bounds::default(),
    };
    scan.count_writes();
    scan.walk_block(body.root);
    scan.out
}

struct Scan<'a, F> {
    body: &'a Body,
    types: &'a TypeTable,
    builtin: F,
    /// Every `let` of each local.
    lets: IndexMap<u32, Vec<(StmtId, Operand)>>,
    /// Every other write of each local: an assignment, a `&mut` escape, a
    /// pattern binding, or a receiver call.
    writes: IndexMap<u32, u32>,
    /// Counters in scope, each with its range between guard and step.
    counters: Vec<(u32, Range)>,
    out: Bounds,
}

impl<'b, F: Fn(FuncId) -> Option<Builtin<'b>>> Scan<'_, F> {
    fn count_writes(&mut self) {
        let body = self.body;
        // A builtin checking an array's bounds does not rebind that array, so
        // its `&mut` writes elements and leaves every length read here intact.
        let mut element_writes: IndexSet<ExprId> = IndexSet::default();
        body.for_each_reachable_node(|n| {
            if let NodeRef::Expr(e) = n
                && let ExprKind::Call { func_id, args, .. } = &body.exprs[e].kind
                && let Some(builtin) = (self.builtin)(*func_id)
            {
                for pos in builtin.checked_arrays() {
                    if let Some(Operand::Expr(arr)) = args.get(pos).map(|a| a.expr) {
                        element_writes.insert(arr);
                    }
                }
            }
        });
        let mut written = Vec::new();
        body.for_each_reachable_node(|n| match n {
            NodeRef::Stmt(s) => {
                if let StmtKind::Let {
                    local_index, value, ..
                } = &body.stmts[s].kind
                {
                    self.lets.entry(*local_index).or_default().push((s, *value));
                }
            }
            NodeRef::Pat(p) => {
                if let PatKind::Binding { local_index, .. } = &body.pats[p].kind {
                    written.push(*local_index);
                }
            }
            NodeRef::Expr(e) => {
                if element_writes.contains(&e) {
                    return;
                }
                if let Some(root) = local_written_by(body, n) {
                    written.push(root);
                }
                // A receiver call may rebind what it is handed with no `&mut`
                // node to say so.
                if let ExprKind::Call {
                    args,
                    has_receiver: true,
                    ..
                } = &body.exprs[e].kind
                    && let Some(Operand::Expr(recv)) = args.first().map(|a| a.expr)
                    && let Some(root) = storage_root(body, recv)
                {
                    written.push(root);
                }
            }
            NodeRef::Block(_) => {}
        });
        for local in written {
            *self.writes.entry(local).or_default() += 1;
        }
    }

    fn writes_of(&self, local: u32) -> u32 {
        self.writes.get(&local).copied().unwrap_or(0)
    }

    /// The one `let` of a local nothing else writes.
    fn sole_let(&self, local: u32) -> Option<Operand> {
        match self.lets.get(&local).map(Vec::as_slice) {
            Some(&[(_, value)]) if self.writes_of(local) == 0 => Some(value),
            _ => None,
        }
    }

    fn walk_block(&mut self, block: BlockId) {
        let stmts = self.body.blocks[block].stmts.clone();
        self.walk_stmts(&stmts, 0..stmts.len());
    }

    /// Walk `stmts[at]`, with the whole block at hand for a loop to find its
    /// counter's `let` in.
    fn walk_stmts(&mut self, stmts: &[StmtId], at: std::ops::Range<usize>) {
        for i in at {
            let s = stmts[i];
            match &self.body.stmts[s].kind {
                StmtKind::Loop { body } => self.walk_loop(&stmts[..i], s, *body),
                _ => self.walk(NodeRef::Stmt(s)),
            }
        }
    }

    fn walk(&mut self, node: NodeRef) {
        match node {
            NodeRef::Block(b) => return self.walk_block(b),
            NodeRef::Expr(e) => {
                self.check_call(e);
                self.out.writes_shared_heap |= self.stores_to_shared(e);
            }
            NodeRef::Stmt(_) | NodeRef::Pat(_) => {}
        }
        let mut children = Vec::new();
        self.body.for_each_child(node, |c| children.push(c));
        for c in children {
            self.walk(c);
        }
    }

    fn walk_loop(&mut self, before: &[StmtId], loop_stmt: StmtId, loop_body: BlockId) {
        let Some((counter, range, ends)) = self.counted_loop(before, loop_body) else {
            return self.walk_block(loop_body);
        };
        if ends {
            self.out.counted.insert(loop_stmt);
        }
        let stmts = self.body.blocks[loop_body].stmts.clone();
        let last = stmts.len() - 1;
        self.walk_stmts(&stmts, 0..1);
        self.counters.push((counter, range));
        self.walk_stmts(&stmts, 1..last);
        self.counters.pop();
        self.walk_stmts(&stmts, last..stmts.len());
    }

    /// `let x = e; loop { if !(x < N) { break; } ..; x = x + 1; }` with `x`
    /// written nowhere else: the counter, its range in the interior, and
    /// whether the loop is sure to end (no `continue` skips the step).
    fn counted_loop(&self, before: &[StmtId], loop_body: BlockId) -> Option<(u32, Range, bool)> {
        let stmts = &self.body.blocks[loop_body].stmts;
        let (&guard, rest) = stmts.split_first()?;
        let &step = rest.last()?;
        let (counter, last) = self.exit_guard(guard)?;
        if !self.increments(step, counter) || self.writes_of(counter) != 1 {
            return None;
        }
        let &[(let_stmt, entry)] = self.lets.get(&counter)?.as_slice() else {
            return None;
        };
        if !before.contains(&let_stmt) {
            return None;
        }
        let (lo, _) = self.range(entry, 0)?;
        let ends = !rest[..rest.len() - 1]
            .iter()
            .any(|&s| continues_this_loop(self.body, NodeRef::Stmt(s)));
        Some((counter, (lo, last.max(lo)), ends))
    }

    /// `if !(x < N) { break; }` (or `<=`, or `x >= N` / `x > N`): the counter
    /// and the last value the interior sees. Refused where the step could wrap.
    fn exit_guard(&self, guard: StmtId) -> Option<(u32, i64)> {
        let (cond, then_block, has_else) = match &self.body.stmts[guard].kind {
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => (*condition, *then_block, else_block.is_some()),
            StmtKind::Expr(Operand::Expr(e)) => match &self.body.exprs[*e].kind {
                ExprKind::If {
                    condition,
                    then_branch,
                    else_branch,
                } => (*condition, *then_branch, else_branch.is_some()),
                _ => return None,
            },
            _ => return None,
        };
        if has_else || !self.is_bare_break(then_block) {
            return None;
        }
        let (exit, negated) = match self.not_of(cond) {
            Some(inner) => (inner, true),
            None => (cond, false),
        };
        let (l, op, r) = binary_parts(self.body, exit)?;
        let counter = operand_local(self.body, l)?;
        let (n, n_hi) = self.range(r, 0)?;
        if n != n_hi {
            return None;
        }
        // The last value the interior sees; the step must reach one past it.
        let last = match (negated, op) {
            (true, NirBinaryOp::Lt) | (false, NirBinaryOp::GtEq) => n.checked_sub(1)?,
            (true, NirBinaryOp::LtEq) | (false, NirBinaryOp::Gt) => n,
            _ => return None,
        };
        let (_, max) = prim_range(self.prim(l)?)?;
        (last < max).then_some((counter, last))
    }

    fn is_bare_break(&self, block: BlockId) -> bool {
        matches!(
            self.body.blocks[block].stmts.as_slice(),
            &[s] if matches!(
                self.body.stmts[s].kind,
                StmtKind::Break { label: None, value: None }
            )
        )
    }

    fn not_of(&self, op: Operand) -> Option<Operand> {
        match op {
            Operand::Expr(e) => match &self.body.exprs[e].kind {
                ExprKind::Unary {
                    op: NirUnaryOp::Not,
                    expr,
                } => Some(*expr),
                _ => None,
            },
            Operand::Value(v) => match self.body.values.kind(v) {
                ValueKind::Unary {
                    op: NirUnaryOp::Not,
                    operand,
                    ..
                } => Some(Operand::Value(*operand)),
                _ => None,
            },
        }
    }

    /// Whether `step` is exactly `counter = counter + 1`.
    fn increments(&self, step: StmtId, counter: u32) -> bool {
        let StmtKind::Expr(Operand::Expr(e)) = &self.body.stmts[step].kind else {
            return false;
        };
        let ExprKind::Assign { target, value } = &self.body.exprs[*e].kind else {
            return false;
        };
        if !matches!(&self.body.exprs[*target].kind, ExprKind::Local { index, .. } if *index == counter)
        {
            return false;
        }
        let Some((l, NirBinaryOp::Add, r)) = binary_parts(self.body, *value) else {
            return false;
        };
        let one = |op| self.range(op, 0) == Some((1, 1));
        let is_counter = |op| operand_local(self.body, op) == Some(counter);
        (is_counter(l) && one(r)) || (one(l) && is_counter(r))
    }

    fn check_call(&mut self, e: ExprId) {
        let ExprKind::Call { func_id, args, .. } = &self.body.exprs[e].kind else {
            return;
        };
        let Some(spec) = (self.builtin)(*func_id).and_then(|b| b.trap) else {
            return;
        };
        let arg = |pos: usize| args[pos].expr;
        let nonneg = |pos: Option<usize>, absent: i64| match pos {
            Some(pos) => self.range(arg(pos), 0).filter(|r| r.0 >= 0).map(|r| r.1),
            None => Some(absent),
        };
        let holds = |check: &TrapCheck<usize>| match *check {
            TrapCheck::Negative(pos) => nonneg(Some(pos), 0).is_some(),
            TrapCheck::Outside { array, at, len } => matches!(
                (self.array_len(arg(array), 0), nonneg(at, 0), nonneg(len, 1)),
                (Some(l), Some(at), Some(n)) if at.checked_add(n).is_some_and(|end| end <= l)
            ),
            TrapCheck::Unset(array) => self.elements_never_unset(arg(array)),
        };
        if spec.checks.iter().all(holds) {
            self.out.in_bounds.insert(e);
        }
    }

    /// Whether every slot of the array `op` holds a value: a primitive element
    /// is a Wasm value type, while any other is stored nullable.
    fn elements_never_unset(&self, op: Operand) -> bool {
        let array = self.types.peel_refs(self.body.operand_type(op));
        let Some(ResolvedType::BuiltinArray(element)) = self.types.get_pruned(array) else {
            panic!("`#[trap(unset = ...)]` names a parameter that is not an `Array<T>`");
        };
        self.types.is_primitive_like(*element)
    }

    /// Whether `e` stores into an object this body did not allocate: through a
    /// projection an assignment names, or through a builtin's `&mut` argument.
    fn stores_to_shared(&self, e: ExprId) -> bool {
        match &self.body.exprs[e].kind {
            ExprKind::Assign { target, .. } => match &self.body.exprs[*target].kind {
                ExprKind::Local { .. } | ExprKind::GlobalVarGet { .. } => false,
                ExprKind::FieldAccess { expr, .. }
                | ExprKind::Index { expr, .. }
                | ExprKind::VariantPayload { expr, .. } => !self.fresh(*expr, 0),
                _ => true,
            },
            ExprKind::Call { func_id, args, .. } => (self.builtin)(*func_id).is_some_and(|b| {
                b.mut_params
                    .iter()
                    .any(|&pos| !self.fresh(args[pos].expr, 0))
            }),
            _ => false,
        }
    }

    /// Whether `op` names an object this body allocated, reached only through
    /// bindings nothing rebinds, so no caller can hold it.
    fn fresh(&self, op: Operand, depth: u32) -> bool {
        if depth > MAX_DEPTH {
            return false;
        }
        if let Some(local) = operand_local(self.body, op) {
            return self
                .sole_let(local)
                .is_some_and(|v| self.fresh(v, depth + 1));
        }
        let Operand::Expr(e) = op else {
            return false;
        };
        match &self.body.exprs[e].kind {
            ExprKind::Unary {
                op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
                expr,
            } => self.fresh(*expr, depth + 1),
            ExprKind::Call { func_id, .. } => (self.builtin)(*func_id).is_some_and(|b| b.owned),
            ExprKind::StructLiteral { .. }
            | ExprKind::TupleLiteral { .. }
            | ExprKind::ArrayLiteral { .. }
            | ExprKind::PackedArray(_)
            | ExprKind::VariantConstruct { .. } => true,
            ExprKind::FieldAccess { .. } => self
                .literal_field(e)
                .is_some_and(|v| self.fresh(v, depth + 1)),
            _ => false,
        }
    }

    /// The value a struct literal bound to a local nothing rebinds gave the
    /// field `field_access` reads.
    fn literal_field(&self, field_access: ExprId) -> Option<Operand> {
        let ExprKind::FieldAccess {
            expr: receiver,
            field_index,
            ..
        } = &self.body.exprs[field_access].kind
        else {
            return None;
        };
        let Operand::Expr(literal) = self.sole_let(operand_local(self.body, *receiver)?)? else {
            return None;
        };
        let ExprKind::StructLiteral { fields, .. } = &self.body.exprs[literal].kind else {
            return None;
        };
        fields
            .iter()
            .find(|f| f.field_index == *field_index)
            .map(|f| f.value)
    }

    fn prim(&self, op: Operand) -> Option<PrimitiveType> {
        const_eval::prim_of(self.body.operand_type(op), self.types)
    }

    /// The range of an integer operand wherever it is read, or `None`.
    fn range(&self, op: Operand, depth: u32) -> Option<Range> {
        if depth > MAX_DEPTH {
            return None;
        }
        if let Operand::Value(v) = op
            && let Some(const_eval::Value::Int { value, prim }) =
                self.body.values.const_of(v, self.types)
        {
            let c = int_value(value, prim)?;
            return Some((c, c));
        }
        if let Some(local) = operand_local(self.body, op) {
            if let Some(&(_, r)) = self.counters.iter().rev().find(|(c, _)| *c == local) {
                return Some(r);
            }
            return self.range(self.sole_let(local)?, depth + 1);
        }
        if let Some((l, bin, r)) = binary_parts(self.body, op) {
            let (a, b) = (self.range(l, depth + 1)?, self.range(r, depth + 1)?);
            let result = binary_range(bin, a, b)?;
            return fits(result, self.prim(op)?);
        }
        let Operand::Expr(e) = op else {
            return None;
        };
        let ExprKind::Cast { expr, .. } = &self.body.exprs[e].kind else {
            return None;
        };
        let source = self.prim(*expr)?;
        let r = self
            .range(*expr, depth + 1)
            .or_else(|| prim_range(source).filter(|_| narrow(source)))?;
        fits(r, self.prim(op)?)
    }

    /// A lower bound on the length of the GC array `op` names.
    fn array_len(&self, op: Operand, depth: u32) -> Option<i64> {
        if depth > MAX_DEPTH {
            return None;
        }
        if let Some(local) = operand_local(self.body, op) {
            return self.array_len(self.sole_let(local)?, depth + 1);
        }
        let Operand::Expr(e) = op else {
            return None;
        };
        match &self.body.exprs[e].kind {
            ExprKind::Unary {
                op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
                expr,
            } => self.array_len(*expr, depth + 1),
            ExprKind::Call { func_id, args, .. } => {
                let pos = (self.builtin)(*func_id)?.trap?.result_len?;
                let (lo, _) = self.range(args[pos].expr, depth + 1)?;
                (lo >= 0).then_some(lo)
            }
            ExprKind::ArrayLiteral { elements } => i64::try_from(elements.len()).ok(),
            ExprKind::PackedArray(bytes) => i64::try_from(bytes.len()).ok(),
            ExprKind::FieldAccess { .. } => self.array_len(self.literal_field(e)?, depth + 1),
            _ => None,
        }
    }
}

/// Whether `node` holds a `continue` that restarts the loop whose body it sits
/// in, rather than one of a loop nested inside it.
fn continues_this_loop(body: &Body, node: NodeRef) -> bool {
    match node {
        NodeRef::Stmt(s) => match &body.stmts[s].kind {
            StmtKind::Continue => return true,
            StmtKind::Loop { .. } => return false,
            _ => {}
        },
        NodeRef::Expr(_) | NodeRef::Block(_) | NodeRef::Pat(_) => {}
    }
    let mut found = false;
    body.for_each_child(node, |c| found |= continues_this_loop(body, c));
    found
}

fn binary_range(op: NirBinaryOp, a: Range, b: Range) -> Option<Range> {
    match op {
        NirBinaryOp::Add => Some((a.0.checked_add(b.0)?, a.1.checked_add(b.1)?)),
        NirBinaryOp::Sub => Some((a.0.checked_sub(b.1)?, a.1.checked_sub(b.0)?)),
        NirBinaryOp::Mul => {
            let products = [
                a.0.checked_mul(b.0)?,
                a.0.checked_mul(b.1)?,
                a.1.checked_mul(b.0)?,
                a.1.checked_mul(b.1)?,
            ];
            Some((*products.iter().min()?, *products.iter().max()?))
        }
        // A non-negative side masks the other into `0..=` its own maximum.
        NirBinaryOp::BitAnd => match (a.0 >= 0, b.0 >= 0) {
            (true, true) => Some((0, a.1.min(b.1))),
            (true, false) => Some((0, a.1)),
            (false, true) => Some((0, b.1)),
            (false, false) => None,
        },
        NirBinaryOp::Shr if a.0 >= 0 && b.0 == b.1 && (0..32).contains(&b.0) => {
            Some((a.0 >> b.0, a.1 >> b.0))
        }
        _ => None,
    }
}

/// `r` when every value in it is representable in `prim`, so nothing wrapped.
fn fits(r: Range, prim: PrimitiveType) -> Option<Range> {
    let (min, max) = prim_range(prim)?;
    (min <= r.0 && r.1 <= max).then_some(r)
}

/// Whether a cast from `prim` bounds the result by the source type alone.
fn narrow(prim: PrimitiveType) -> bool {
    matches!(
        prim,
        PrimitiveType::U8 | PrimitiveType::U16 | PrimitiveType::I8 | PrimitiveType::I16
    )
}

/// [`PrimitiveType::int_range`] where it fits a [`Range`], which `u64` does not.
fn prim_range(prim: PrimitiveType) -> Option<Range> {
    let (min, max) = prim.int_range()?;
    Some((i64::try_from(min).ok()?, i64::try_from(max).ok()?))
}

/// A constant's value, from the sign- or zero-extended pattern the pool keeps.
fn int_value(bits: u64, prim: PrimitiveType) -> Option<i64> {
    prim_range(prim)?;
    Some(bits.cast_signed())
}
