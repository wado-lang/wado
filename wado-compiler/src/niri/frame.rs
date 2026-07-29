//! The compile-time frame: running a body rather than reading it.
//!
//! The frame performs each statement exactly once, in order, over values the
//! engine itself built. That is what separates it from the lattice projection,
//! which is re-entrant and may run over the same node any number of times: a
//! write belongs here and nowhere else, and a call that performs one is refused
//! by the projection outright.
//!
//! Everything the frame cannot carry out abandons the evaluation. Stepping past
//! an unperformed write would leave the place it targets holding a value the
//! program never produced.

use crate::const_eval::{MAX_SEQ_ELEMENTS, Value};
use crate::nir_arena::{
    BlockId, Body, ExprId, ExprKind, ExprNode, NodeRef, Operand, StmtId, StmtKind, StmtNode,
};
use crate::nir_visitor::NirRefVisitor;
use crate::tir::{PrimitiveType, ResolvedType};

use super::place::{overlapping_places, place_of};
use super::trackability::{ExecutedWrites, aggregate_safe_locals, clobbered_locals};
use super::{CallRun, CalleeKey, CtfeBuiltin, Interpreter, Lattice, prim_of};

/// How control left a statement sequence during compile-time execution.
///
/// `Break` is not loop-specific: a labeled block is the value-producing shape
/// a `break L: v` completes, and a loop catches only an unlabeled `break`.
enum Flow {
    /// Reached the end, carrying the value of the last statement that
    /// produced one.
    Fallthrough(Lattice),
    Return(Lattice),
    Break {
        label: Option<String>,
        value: Lattice,
    },
    Continue,
    /// The engine could not follow the program: abandon the evaluation so the
    /// original call survives.
    Bail,
}

/// The loop-body nodes one iteration may rewrite, kept so the next iteration
/// starts from the same program. Reduction rewrites an expression toward the
/// value it took *this* time round — `acc + 1` becomes the literal `1` on the
/// first pass — so without restoring, later iterations read the first one's
/// results.
struct LoopSnapshot {
    exprs: Vec<(ExprId, ExprNode)>,
    stmts: Vec<(StmtId, StmtNode)>,
    blocks: Vec<(BlockId, Vec<StmtId>)>,
}

impl LoopSnapshot {
    fn capture(body: &Body, block: BlockId) -> Self {
        #[derive(Default)]
        struct Collect {
            exprs: Vec<ExprId>,
            stmts: Vec<StmtId>,
            blocks: Vec<BlockId>,
        }
        impl NirRefVisitor for Collect {
            fn visit_node(&mut self, body: &Body, node: NodeRef) {
                match node {
                    NodeRef::Expr(e) => self.exprs.push(e),
                    NodeRef::Stmt(s) => self.stmts.push(s),
                    NodeRef::Block(b) => self.blocks.push(b),
                    // Patterns are matched, never rewritten.
                    NodeRef::Pat(_) => {}
                }
                self.walk_node(body, node);
            }
        }
        let mut collect = Collect::default();
        collect.visit_node(body, NodeRef::Block(block));
        Self {
            exprs: collect
                .exprs
                .into_iter()
                .map(|e| (e, body.exprs[e].clone()))
                .collect(),
            stmts: collect
                .stmts
                .into_iter()
                .map(|s| (s, body.stmts[s].clone()))
                .collect(),
            blocks: collect
                .blocks
                .into_iter()
                .map(|b| (b, body.blocks[b].stmts.clone()))
                .collect(),
        }
    }

    /// Put the captured nodes back. Nodes an iteration allocated are left
    /// behind unreferenced.
    fn restore(&self, body: &mut Body) {
        for (e, node) in &self.exprs {
            body.exprs[*e] = node.clone();
        }
        for (s, node) in &self.stmts {
            body.stmts[*s] = node.clone();
        }
        for (b, stmts) in &self.blocks {
            body.blocks[*b].stmts.clone_from(stmts);
        }
    }
}

impl<'a> Interpreter<'a> {
    fn exec_block_a(&mut self, body: &mut Body, block: BlockId) -> Flow {
        let stmts = body.blocks[block].stmts.clone();
        let mut value = Lattice::Unevaluated;
        for s in stmts {
            match self.exec_stmt_a(body, s) {
                Flow::Fallthrough(v) => value = v,
                other => return other,
            }
        }
        Flow::Fallthrough(value)
    }

    /// Execute one statement, charging the step budget.
    ///
    /// A statement counts as executed only when everything it evaluates lands
    /// on a constant. Reducing an expression is not performing it, so anything
    /// left undone — an unfolded call, a global write, a would-be trap —
    /// abandons the evaluation rather than being stepped past.
    fn exec_stmt_a(&mut self, body: &mut Body, s: StmtId) -> Flow {
        if self.step_budget == 0 {
            return Flow::Bail;
        }
        self.step_budget -= 1;
        match &body.stmts[s].kind {
            StmtKind::Let {
                local_index, value, ..
            } => {
                let (index, value) = (*local_index, *value);
                let lattice = match value.as_expr().and_then(|e| self.exec_call_stmt_a(body, e)) {
                    Some(Flow::Fallthrough(lattice)) => lattice,
                    Some(Flow::Bail | Flow::Return(_) | Flow::Break { .. } | Flow::Continue) => {
                        return Flow::Bail;
                    }
                    None => self.eval_operand_a(body, value),
                };
                match lattice {
                    lattice @ Lattice::Const(_) => {
                        self.bind_ctfe_local(index, lattice);
                        Flow::Fallthrough(Lattice::Unevaluated)
                    }
                    Lattice::NonConst | Lattice::Unevaluated => Flow::Bail,
                }
            }
            StmtKind::Expr(op) => {
                let op = *op;
                self.exec_expr_stmt_a(body, op)
            }
            StmtKind::Return { value } => match *value {
                None => Flow::Return(Lattice::Unevaluated),
                // A returned expression the frame could not evaluate is one
                // it stepped over, along with whatever that expression would
                // have written.
                Some(op) => match self.eval_operand_a(body, op) {
                    lattice @ (Lattice::Const(_) | Lattice::NonConst) => Flow::Return(lattice),
                    Lattice::Unevaluated => Flow::Bail,
                },
            },
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                let (condition, then_block, else_block) = (*condition, *then_block, *else_block);
                match self.eval_operand_a(body, condition) {
                    Lattice::Const(Value::Bool(true)) => self.exec_block_a(body, then_block),
                    Lattice::Const(Value::Bool(false)) => match else_block {
                        Some(eb) => self.exec_block_a(body, eb),
                        None => Flow::Fallthrough(Lattice::Unevaluated),
                    },
                    Lattice::Const(
                        Value::Int { .. }
                        | Value::Float { .. }
                        | Value::Char(_)
                        | Value::Aggregate { .. }
                        | Value::Seq { .. },
                    )
                    | Lattice::NonConst
                    | Lattice::Unevaluated => Flow::Bail,
                }
            }
            StmtKind::Break { label, value } => {
                let (label, value) = (label.clone(), *value);
                let value = self.eval_optional_operand_a(body, value);
                Flow::Break { label, value }
            }
            StmtKind::Continue => Flow::Continue,
            StmtKind::Loop { body: block } => {
                let block = *block;
                self.exec_loop_a(body, block)
            }
            StmtKind::LabeledBlock { label, block } => {
                let (label, block) = (label.clone(), *block);
                match self.exec_block_a(body, block) {
                    Flow::Break {
                        label: Some(broke),
                        value,
                    } if broke == label => Flow::Fallthrough(value),
                    other => other,
                }
            }
            StmtKind::LetDestructure { .. } => Flow::Bail,
        }
    }

    /// Run a loop until it breaks, control leaves the function, or the budget
    /// runs out. Termination rests on the budget alone — the per-iteration
    /// charge covers an empty body too — so no constant trip count is needed.
    fn exec_loop_a(&mut self, body: &mut Body, block: BlockId) -> Flow {
        let snapshot = LoopSnapshot::capture(body, block);
        loop {
            if self.step_budget == 0 {
                return Flow::Bail;
            }
            self.step_budget -= 1;
            match self.exec_block_a(body, block) {
                Flow::Fallthrough(_) | Flow::Continue => {}
                // A labeled `break` belongs to an enclosing labeled block.
                Flow::Break { label: None, .. } => {
                    return Flow::Fallthrough(Lattice::Unevaluated);
                }
                other => return other,
            }
            snapshot.restore(body);
            self.scratch_folds.clear();
        }
    }

    /// An assignment updates the environment; anything else contributes its
    /// value as the block's result.
    fn exec_expr_stmt_a(&mut self, body: &mut Body, op: Operand) -> Flow {
        if let Some(e) = op.as_expr()
            && let Some(flow) = self.exec_builtin_stmt_a(body, e)
        {
            return flow;
        }
        if let Some(e) = op.as_expr()
            && let ExprKind::Assign { target, value } = &body.exprs[e].kind
        {
            let (target, value) = (*target, *value);
            return self.exec_store_a(body, target, value);
        }
        if let Some(e) = op.as_expr()
            && let Some(flow) = self.exec_call_stmt_a(body, e)
        {
            return flow;
        }
        match self.eval_operand_a(body, op) {
            lattice @ Lattice::Const(_) => Flow::Fallthrough(lattice),
            Lattice::NonConst | Lattice::Unevaluated => Flow::Bail,
        }
    }

    /// Run a call at statement position for the writes it performs. `None`
    /// when the expression is not a call the frame knows; `Flow::Bail` when it
    /// is one the frame cannot run — stepping past a call whose writes it did
    /// not apply would leave the caller's places stale.
    fn exec_call_stmt_a(&mut self, body: &Body, e: ExprId) -> Option<Flow> {
        let (key, _) = self.call_target_a(body, e)?;
        if !self.callees.is_some_and(|c| c.contains_key(&key)) {
            return None;
        }
        let Some(run) = self.run_call_a(body, e, true) else {
            return Some(Flow::Bail);
        };
        match self.apply_writes_a(run.writes) {
            Some(()) => Some(Flow::Fallthrough(run.result)),
            None => Some(Flow::Bail),
        }
    }

    /// Perform `place = value`, updating the frame's value for the place's
    /// root. A target that names no place, or a projection into a root the
    /// frame holds no constant for, bails: stepping past a store it did not
    /// apply would leave the container stale.
    fn exec_store_a(&mut self, body: &mut Body, target: ExprId, value: Operand) -> Flow {
        let Some((root, path)) = place_of(body, target.into()) else {
            return Flow::Bail;
        };
        let Lattice::Const(value) = self.eval_operand_a(body, value) else {
            return Flow::Bail;
        };
        if path.is_empty() {
            self.bind_ctfe_local(root, Lattice::Const(value));
            return Flow::Fallthrough(Lattice::Unevaluated);
        }
        match self.update_place_a(root, &path, |_| Some(value)) {
            Some(()) => Flow::Fallthrough(Lattice::Unevaluated),
            None => Flow::Bail,
        }
    }

    /// Rebind the frame's value for `root` with the value at `path` replaced by
    /// what `update` makes of it. `None` when the frame holds no constant for
    /// the root — which is what confines a write to values the engine itself
    /// built — or when the path does not reach a value the update applies to.
    fn update_place_a(
        &mut self,
        root: u32,
        path: &[u32],
        update: impl FnOnce(&Value) -> Option<Value>,
    ) -> Option<()> {
        let Lattice::Const(container) = self.env.get(&root)?.clone() else {
            return None;
        };
        let target = path.iter().try_fold(&container, |v, i| v.field(*i))?;
        let updated = container.with_path(path, update(target)?)?;
        self.bind_ctfe_local(root, Lattice::Const(updated));
        Some(())
    }

    /// Perform a builtin at statement position, updating the frame's value for
    /// the place a write lands in. `None` when the statement is not a builtin
    /// the engine knows; `Some(Flow::Bail)` when it is one it cannot follow —
    /// stepping past a write it did not apply would leave the container stale.
    ///
    /// The root has to be a local the frame already holds a constant for, which
    /// confines the write to values the engine itself built.
    fn exec_builtin_stmt_a(&mut self, body: &mut Body, e: ExprId) -> Option<Flow> {
        let ExprKind::Call { func_id, args, .. } = &body.exprs[e].kind else {
            return None;
        };
        let builtin = *self.ctfe_builtins.and_then(|m| m.get(func_id))?;
        let args: Vec<Operand> = args.iter().map(|a| a.expr).collect();
        match builtin {
            CtfeBuiltin::ArraySet => Some(self.exec_element_write_a(body, &args)),
            CtfeBuiltin::ArrayCopy => Some(self.exec_run_write_a(body, &args)),
            CtfeBuiltin::ColdPath => Some(Flow::Fallthrough(Lattice::Unevaluated)),
            CtfeBuiltin::ArrayGet
            | CtfeBuiltin::ArrayLen
            | CtfeBuiltin::ArrayNew
            | CtfeBuiltin::Select => None,
        }
    }

    fn exec_element_write_a(&mut self, body: &mut Body, args: &[Operand]) -> Flow {
        let [seq, index, element] = *args else {
            return Flow::Bail;
        };
        let Some((root, path)) = place_of(body, seq) else {
            return Flow::Bail;
        };
        let (Lattice::Const(index), Lattice::Const(element)) = (
            self.eval_operand_a(body, index),
            self.eval_operand_a(body, element),
        ) else {
            return Flow::Bail;
        };
        let Some((index, _)) = index.as_int() else {
            return Flow::Bail;
        };
        match self.update_place_a(root, &path, |seq| seq.with_element(index, element)) {
            Some(()) => Flow::Fallthrough(Lattice::Unevaluated),
            None => Flow::Bail,
        }
    }

    /// Splices `len` of `src`'s elements into the destination. A run either
    /// side cannot supply is left to trap at run time.
    fn exec_run_write_a(&mut self, body: &mut Body, args: &[Operand]) -> Flow {
        let [destination, at, source, from, len] = *args else {
            return Flow::Bail;
        };
        let Some((root, path)) = place_of(body, destination) else {
            return Flow::Bail;
        };
        let (Lattice::Const(at), Lattice::Const(source), Lattice::Const(from), Lattice::Const(len)) = (
            self.eval_operand_a(body, at),
            self.eval_operand_a(body, source),
            self.eval_operand_a(body, from),
            self.eval_operand_a(body, len),
        ) else {
            return Flow::Bail;
        };
        let (Some((at, _)), Some((from, _)), Some((len, _))) =
            (at.as_int(), from.as_int(), len.as_int())
        else {
            return Flow::Bail;
        };
        match self.update_place_a(root, &path, |destination| {
            destination.with_run(at, &source, from, len)
        }) {
            Some(()) => Flow::Fallthrough(Lattice::Unevaluated),
            None => Flow::Bail,
        }
    }

    fn eval_optional_operand_a(&mut self, body: &mut Body, op: Option<Operand>) -> Lattice {
        match op {
            Some(op) => self.eval_operand_a(body, op),
            None => Lattice::Unevaluated,
        }
    }

    /// Reducing in place first is what lets a nested call fold before the
    /// operand is projected.
    fn eval_operand_a(&mut self, body: &mut Body, op: Operand) -> Lattice {
        match op.as_expr() {
            Some(e) => {
                self.reduce_in_place_a(body, e);
                self.reduce_to_lattice_a(body, e)
            }
            None => self.operand_to_lattice_a(body, op),
        }
    }

    /// Bind a local inside a compile-time frame. A local the frame may reach
    /// through another handle keeps no value.
    fn bind_ctfe_local(&mut self, index: u32, lattice: Lattice) {
        if self.ctfe_clobbered.contains(index) {
            self.env.insert(index, Lattice::NonConst);
        } else {
            self.bind_local(index, lattice);
        }
    }

    /// Evaluate `array_get(seq, i)` / `array_len(seq)` over a constant
    /// sequence, or the sequence `array_new(len)` allocates. A read's argument
    /// is a reference to the array, and a reference to a constant reads as that
    /// constant, so no separate deref step is needed.
    pub(super) fn try_ctfe_builtin_fold_a(&self, body: &Body, e: ExprId) -> Lattice {
        let ExprKind::Call { func_id, args, .. } = &body.exprs[e].kind else {
            return Lattice::Unevaluated;
        };
        let Some(builtin) = self.ctfe_builtins.and_then(|m| m.get(func_id)) else {
            return Lattice::Unevaluated;
        };
        match (builtin, args.as_slice()) {
            (CtfeBuiltin::ArrayLen, [arr]) => {
                let Lattice::Const(v) = self.operand_to_lattice_a(body, arr.expr) else {
                    return Lattice::Unevaluated;
                };
                v.seq_len().map_or(Lattice::Unevaluated, |len| {
                    Lattice::Const(Value::Int {
                        value: len as u64,
                        prim: PrimitiveType::I32,
                    })
                })
            }
            (CtfeBuiltin::ArrayGet, [arr, index]) => self.index_lattice(body, arr.expr, index.expr),
            (CtfeBuiltin::ArrayNew, [len]) => self.allocation_lattice(body, e, len.expr),
            (CtfeBuiltin::Select, [condition, if_true, if_false]) => {
                self.select_lattice(body, condition.expr, if_true.expr, if_false.expr)
            }
            // A write denotes nothing; the executor performs it as a
            // statement. Nor does a hint, which it steps past.
            (
                CtfeBuiltin::ArraySet
                | CtfeBuiltin::ArrayCopy
                | CtfeBuiltin::ColdPath
                | CtfeBuiltin::Select
                | CtfeBuiltin::ArrayLen
                | CtfeBuiltin::ArrayGet
                | CtfeBuiltin::ArrayNew,
                _,
            ) => Lattice::Unevaluated,
        }
    }

    /// The arm `select` picks. Both arms run at run time, so the one not taken
    /// has to compute rather than trap; a constant is exactly that.
    fn select_lattice(
        &self,
        body: &Body,
        condition: Operand,
        if_true: Operand,
        if_false: Operand,
    ) -> Lattice {
        let Lattice::Const(Value::Bool(condition)) = self.operand_lattice_folded_a(body, condition)
        else {
            return Lattice::Unevaluated;
        };
        let (Lattice::Const(if_true), Lattice::Const(if_false)) = (
            self.operand_lattice_folded_a(body, if_true),
            self.operand_lattice_folded_a(body, if_false),
        ) else {
            return Lattice::Unevaluated;
        };
        Lattice::Const(if condition { if_true } else { if_false })
    }

    /// The sequence `array_new(len)` allocates: `len` elements at the default
    /// `array.new_default` leaves. A negative or oversized length, or an
    /// element type with no compile-time default, is not a constant here — the
    /// call stays and traps or allocates at run time as written.
    fn allocation_lattice(&self, body: &Body, e: ExprId, len: Operand) -> Lattice {
        let Lattice::Const(len) = self.operand_to_lattice_a(body, len) else {
            return Lattice::Unevaluated;
        };
        let Some((len, PrimitiveType::I32)) = len.as_int() else {
            return Lattice::Unevaluated;
        };
        let array_type = body.exprs[e].type_id;
        let ResolvedType::BuiltinArray(element_type) = self.type_table.get(array_type) else {
            return Lattice::Unevaluated;
        };
        let (Ok(len), Some(default)) = (
            usize::try_from(len as i32),
            prim_of(*element_type, self.type_table).and_then(Value::default_of),
        ) else {
            return Lattice::Unevaluated;
        };
        // Before building: an allocation the value model rejects must not be
        // walked element by element first.
        if len > MAX_SEQ_ELEMENTS {
            return Lattice::Unevaluated;
        }
        Value::seq(array_type, vec![default; len]).map_or(Lattice::Unevaluated, Lattice::Const)
    }

    /// Fold a call to the value it computes. One that writes through a `&mut`
    /// parameter is never folded here: this projection is re-entrant, and a
    /// write applied twice is worse than one not folded at all. Those run at
    /// statement position, where the executor applies the writes.
    /// `Unevaluated` on any miss, so the original call — and any runtime trap
    /// inside it — survives.
    pub(super) fn try_call_fold_a(&mut self, body: &Body, e: ExprId) -> Lattice {
        if let lattice @ (Lattice::Const(_) | Lattice::NonConst) =
            self.try_ctfe_builtin_fold_a(body, e)
        {
            // `NonConst` here is an out-of-range read: keep the call so the
            // trap survives.
            return match lattice {
                Lattice::Const(v) => Lattice::Const(v),
                Lattice::NonConst | Lattice::Unevaluated => Lattice::Unevaluated,
            };
        }
        match self.run_call_a(body, e, false) {
            Some(run) => match run.result {
                c @ Lattice::Const(_) => c,
                Lattice::NonConst | Lattice::Unevaluated => Lattice::Unevaluated,
            },
            None => Lattice::Unevaluated,
        }
    }

    /// The callee a call names, and the operands bound to its parameters. A
    /// method's receiver is its first.
    fn call_target_a(&self, body: &Body, e: ExprId) -> Option<(CalleeKey, Vec<Operand>)> {
        match &body.exprs[e].kind {
            ExprKind::Call { func_id, args, .. } => {
                Some((*func_id, args.iter().map(|a| a.expr).collect()))
            }
            ExprKind::MethodCall {
                func_id,
                receiver,
                args,
                ..
            } => {
                let mut ops = Vec::with_capacity(args.len() + 1);
                ops.push(*receiver);
                ops.extend(args.iter().map(|a| a.expr));
                Some((*func_id, ops))
            }
            _ => None,
        }
    }

    /// Run a call in a compile-time frame: bind the parameters, execute the
    /// body, and report the value it returns along with what it leaves in each
    /// `&mut` parameter. `None` when the frame cannot run it.
    ///
    /// `may_write` is the caller's promise to apply the write-backs. Without it
    /// a callee taking a `&mut` parameter is refused outright, since running it
    /// would produce writes with nowhere to go.
    fn run_call_a(&mut self, body: &Body, e: ExprId, may_write: bool) -> Option<CallRun> {
        let callees = self.callees?;
        let (key, args) = self.call_target_a(body, e)?;
        let callee_rc = callees.get(&key)?;
        if self.call_stack.iter().any(|k| k == &key) {
            return None;
        }
        let callee = callee_rc.func.try_borrow().ok()?;
        if args.len() != callee.params.len() {
            return None;
        }
        if !may_write && callee.params.iter().any(|p| p.is_mut_ref) {
            return None;
        }
        let callee_body = callee.body.as_ref()?;
        if self.step_budget == 0 {
            return None;
        }
        // A unit callee denotes nothing, whatever its last statement
        // computed. Handing that value back would leave it on the stack where
        // the call stood and the module would fail to validate.
        let returns_unit = callee.return_type == crate::tir::TypeTable::UNIT;

        let mut bound: Vec<(u32, Value)> = Vec::with_capacity(args.len());
        let mut targets: Vec<(u32, u32, Vec<u32>)> = Vec::new();
        let mut places: Vec<(u32, Vec<u32>)> = Vec::new();
        for (arg, param) in args.iter().zip(&callee.params) {
            let place = place_of(body, *arg);
            let value = if param.is_mut_ref {
                let (root, path) = place.clone()?;
                let value = self.place_value_a(root, &path)?;
                targets.push((param.local_index, root, path));
                value
            } else {
                self.operand_lattice_folded_a(body, *arg).as_const()?
            };
            places.extend(place);
            bound.push((param.local_index, value));
        }
        // Each parameter binds its own snapshot and each write-back replays
        // whole, so two arguments naming the same storage would let the later
        // undo the earlier. Wado has no borrow checker, so that is ordinary
        // source: the frame declines it and the call runs.
        if targets
            .iter()
            .any(|(_, root, path)| overlapping_places(&places, *root, path) > 1)
        {
            return None;
        }

        self.step_budget -= 1;
        self.call_stack.push(key);
        let saved_env = std::mem::take(&mut self.env);
        // The scratch fold memo is scoped to this reduction; nested CTFE calls
        // get a fresh map and ids never cross scratch bodies.
        let saved_folds = std::mem::take(&mut self.scratch_folds);
        let saved_aliases = std::mem::take(&mut self.ref_global_aliases);
        // Local indices are per-function, so the caller's read-only-local set
        // says nothing about the callee's.
        let saved_aggregates = std::mem::take(&mut self.aggregate_locals);
        let saved_clobbered = std::mem::take(&mut self.ctfe_clobbered);
        // Execute on a scratch copy so the shared callee body, held under an
        // immutable `Ref`, is not mutated.
        let mut scratch = callee_body.nodes_only_clone();
        let writes = ExecutedWrites::in_frame(&scratch, self.ctfe_builtins, self.callees);
        self.aggregate_locals = aggregate_safe_locals(&scratch, &writes);
        self.ctfe_clobbered = clobbered_locals(&scratch, &writes);
        for (index, value) in bound {
            let lattice = if self.ctfe_clobbered.contains(index) {
                Lattice::NonConst
            } else {
                Lattice::Const(value)
            };
            self.env.insert(index, lattice);
        }
        let root = scratch.root;
        let flow = self.exec_block_a(&mut scratch, root);
        // Only a body that ran to the end leaves parameters worth reading.
        let completed = matches!(flow, Flow::Return(_) | Flow::Fallthrough(_));
        let result = match flow {
            Flow::Return(lattice) | Flow::Fallthrough(lattice) if !returns_unit => lattice,
            Flow::Return(_)
            | Flow::Fallthrough(_)
            | Flow::Break { .. }
            | Flow::Continue
            | Flow::Bail => Lattice::Unevaluated,
        };
        // Before the frame is torn down. A `&mut` parameter the callee left
        // untrackable has no value to write, and the run is refused rather
        // than losing it.
        let written: Option<Vec<(u32, Vec<u32>, Value)>> = completed
            .then(|| {
                targets
                    .into_iter()
                    .map(|(index, root, path)| {
                        let value = self.env.get(&index)?.as_const()?;
                        Some((root, path, value))
                    })
                    .collect::<Option<Vec<_>>>()
            })
            .flatten();
        self.env = saved_env;
        self.scratch_folds = saved_folds;
        self.ref_global_aliases = saved_aliases;
        self.aggregate_locals = saved_aggregates;
        self.ctfe_clobbered = saved_clobbered;
        self.call_stack.pop();
        Some(CallRun {
            result,
            writes: written?,
        })
    }

    /// An argument's value, folding the arithmetic it may still be spelled as:
    /// an argument reaches a call as written, and the structural projection
    /// alone reads only what already stands as a literal.
    fn operand_lattice_folded_a(&self, body: &Body, op: Operand) -> Lattice {
        match op.as_expr() {
            Some(e) => self.reduce_to_lattice_a(body, e),
            None => self.operand_to_lattice_a(body, op),
        }
    }

    /// The frame's value for a place, or `None` when it holds none.
    fn place_value_a(&self, root: u32, path: &[u32]) -> Option<Value> {
        let Lattice::Const(value) = self.env.get(&root)? else {
            return None;
        };
        path.iter().try_fold(value, |v, i| v.field(*i)).cloned()
    }

    /// Write a finished run's `&mut` parameters back into the caller's places.
    fn apply_writes_a(&mut self, writes: Vec<(u32, Vec<u32>, Value)>) -> Option<()> {
        for (root, path, value) in writes {
            if path.is_empty() {
                self.bind_ctfe_local(root, Lattice::Const(value));
            } else {
                self.update_place_a(root, &path, |_| Some(value))?;
            }
        }
        Some(())
    }
}
