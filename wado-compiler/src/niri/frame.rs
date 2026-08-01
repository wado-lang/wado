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

use crate::const_eval::Value;
use crate::name::RefKind;
use crate::nir_arena::{
    BlockId, Body, ExprId, ExprKind, ExprNode, NodeRef, Operand, StmtId, StmtKind, StmtNode,
};
use crate::nir_visitor::NirRefVisitor;
use crate::tir::TypeTable;

use super::place::{borrowed_place_operand, overlapping_places, place_of};
use super::region::{region_free_reads, region_shape};
use super::trackability::{Reached, aggregate_safe_locals, clobbered_locals};
use super::{
    COPY_CHARGE_DIVISOR, CallRun, CalleeKey, CtfeBuiltin, FrameState, Interpreter, Lattice,
};

impl FrameState {
    /// The state a call's body runs under: what `body` lets a frame track, with
    /// the parameters already bound. A parameter the body can reach through
    /// another handle binds nothing, so a stale constant cannot outlive the
    /// write. Nothing keyed by the caller's local or expression ids carries
    /// over.
    /// Whether `index` takes part in a live place alias, as the alias handle
    /// or as the place it names. Either role makes a value snapshot of the
    /// local unsound to hand out.
    fn alias_involves(&self, index: u32) -> bool {
        self.place_aliases.contains_key(&index)
            || self.place_aliases.values().any(|(root, _)| *root == index)
    }

    fn for_call(
        body: &Body,
        reached: &Reached,
        params: impl IntoIterator<Item = (u32, Value)>,
    ) -> Self {
        let ctfe_clobbered = clobbered_locals(body, reached);
        let mut state = Self {
            aggregate_locals: aggregate_safe_locals(body, reached),
            ..Self::default()
        };
        for (index, value) in params {
            let lattice = if ctfe_clobbered.contains(index) {
                Lattice::NonConst
            } else {
                Lattice::Const(value)
            };
            state.env.insert(index, lattice);
        }
        state.ctfe_clobbered = ctfe_clobbered;
        state
    }
}

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

/// The local `op` writes out, through the casts monomorphization leaves. A
/// deref or a projection is not one: those name storage reached *through* a
/// value rather than the value itself.
fn named_local(body: &Body, op: Operand) -> Option<u32> {
    match &body.exprs[op.as_expr()?].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::Cast { expr, .. } => named_local(body, *expr),
        _ => None,
    }
}

/// Whether two `&mut` arguments name overlapping storage. Each parameter binds
/// its own snapshot and each write-back replays whole, so the later write would
/// undo the earlier. Wado has no borrow checker, so this is ordinary source: the
/// frame declines to run the call rather than mis-run it.
fn aliased_write_targets(targets: &[(u32, u32, Vec<u32>)], places: &[(u32, Vec<u32>)]) -> bool {
    targets
        .iter()
        .any(|(_, root, path)| overlapping_places(places, *root, path) > 1)
}

impl Interpreter<'_> {
    fn exec_block(&mut self, body: &mut Body, block: BlockId) -> Flow {
        let stmts = body.blocks[block].stmts.clone();
        let mut value = Lattice::Unevaluated;
        for s in stmts {
            match self.exec_stmt(body, s) {
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
    fn exec_stmt(&mut self, body: &mut Body, s: StmtId) -> Flow {
        if self.step_budget == 0 {
            return Flow::Bail;
        }
        self.step_budget -= 1;
        match &body.stmts[s].kind {
            StmtKind::Let {
                local_index,
                is_mut,
                value,
                ..
            } => {
                let (index, is_mut, value) = (*local_index, *is_mut, *value);
                // A fresh binding forks the local's storage; an alias taken
                // over the same index would keep pointing at the old one.
                if self
                    .frame
                    .place_aliases
                    .values()
                    .any(|(root, _)| *root == index)
                {
                    return Flow::Bail;
                }
                if !is_mut && let Some(named) = self.aliased_operand(body, value) {
                    return match self.record_place_alias(body, index, named) {
                        Some(()) => Flow::Fallthrough(Lattice::Unevaluated),
                        None => Flow::Bail,
                    };
                }
                let lattice = match value.as_expr().and_then(|e| self.exec_call_stmt(body, e)) {
                    Some(Flow::Fallthrough(lattice)) => lattice,
                    Some(Flow::Bail | Flow::Return(_) | Flow::Break { .. } | Flow::Continue) => {
                        return Flow::Bail;
                    }
                    None => self.eval_operand(body, value),
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
                self.exec_expr_stmt(body, op)
            }
            StmtKind::Return { value } => match *value {
                None => Flow::Return(Lattice::Unevaluated),
                Some(op) => match self.eval_operand(body, op) {
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
                match self.eval_operand(body, condition) {
                    Lattice::Const(Value::Bool(true)) => self.exec_block(body, then_block),
                    Lattice::Const(Value::Bool(false)) => match else_block {
                        Some(eb) => self.exec_block(body, eb),
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
                let value = self.eval_optional_operand(body, value);
                Flow::Break { label, value }
            }
            StmtKind::Continue => Flow::Continue,
            StmtKind::Loop { body: block } => {
                let block = *block;
                self.exec_loop(body, block)
            }
            StmtKind::LabeledBlock { label, block } => {
                let (label, block) = (label.clone(), *block);
                match self.exec_block(body, block) {
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
    fn exec_loop(&mut self, body: &mut Body, block: BlockId) -> Flow {
        let snapshot = LoopSnapshot::capture(body, block);
        loop {
            if self.step_budget == 0 {
                return Flow::Bail;
            }
            self.step_budget -= 1;
            match self.exec_block(body, block) {
                Flow::Fallthrough(_) | Flow::Continue => {}
                Flow::Break { label: None, .. } => {
                    return Flow::Fallthrough(Lattice::Unevaluated);
                }
                other => return other,
            }
            snapshot.restore(body);
            self.frame.scratch_folds.clear();
            self.frame.region_misses.clear();
        }
    }

    /// An assignment updates the environment; anything else contributes its
    /// value as the block's result.
    fn exec_expr_stmt(&mut self, body: &mut Body, op: Operand) -> Flow {
        if let Some(e) = op.as_expr()
            && let Some(flow) = self.exec_builtin_stmt(body, e)
        {
            return flow;
        }
        if let Some(e) = op.as_expr()
            && let ExprKind::Assign { target, value } = &body.exprs[e].kind
        {
            let (target, value) = (*target, *value);
            return self.exec_store(body, target, value);
        }
        if let Some(e) = op.as_expr()
            && let Some(flow) = self.exec_call_stmt(body, e)
        {
            return flow;
        }
        match self.eval_operand(body, op) {
            lattice @ Lattice::Const(_) => Flow::Fallthrough(lattice),
            Lattice::NonConst | Lattice::Unevaluated => Flow::Bail,
        }
    }

    /// Run a call at statement position for the writes it performs. `None`
    /// when the expression is not a call the frame knows.
    fn exec_call_stmt(&mut self, body: &Body, e: ExprId) -> Option<Flow> {
        let (key, _) = self.call_target(body, e)?;
        if !self.callees.is_some_and(|c| c.contains_key(&key)) {
            return None;
        }
        let Some(run) = self.run_call(body, e, true) else {
            return Some(Flow::Bail);
        };
        match self.apply_writes(run.writes) {
            Some(()) => Some(Flow::Fallthrough(run.result)),
            None => Some(Flow::Bail),
        }
    }

    /// Perform `place = value`, updating the frame's value for the place's
    /// root. A target naming no place, or a projection into a root the frame
    /// holds no constant for, bails.
    fn exec_store(&mut self, body: &mut Body, target: ExprId, value: Operand) -> Flow {
        let Some((root, path)) = self.frame_place_of(body, target.into()) else {
            return Flow::Bail;
        };
        let Lattice::Const(value) = self.eval_operand(body, value) else {
            return Flow::Bail;
        };
        if path.is_empty() {
            self.bind_ctfe_local(root, Lattice::Const(value));
            return Flow::Fallthrough(Lattice::Unevaluated);
        }
        match self.update_place(root, &path, |_| Some(value)) {
            Some(()) => Flow::Fallthrough(Lattice::Unevaluated),
            None => Flow::Bail,
        }
    }

    /// Rebind the frame's value for `root` with the value at `path` replaced by
    /// what `update` makes of it. `None` when the frame holds no constant for
    /// the root — which is what confines a write to values the engine itself
    /// built — or when the path does not reach a value the update applies to.
    fn update_place(
        &mut self,
        root: u32,
        path: &[u32],
        update: impl FnOnce(&Value) -> Option<Value>,
    ) -> Option<()> {
        let Lattice::Const(container) = self.frame.env.get(&root)?.clone() else {
            return None;
        };
        let target = path.iter().try_fold(&container, |v, i| v.field(*i))?;
        let updated = container.with_path(path, update(target)?)?;
        self.bind_ctfe_local(root, Lattice::Const(updated));
        Some(())
    }

    /// Perform a builtin at statement position, updating the frame's value for
    /// the place a write lands in. `None` when the statement is not a builtin
    /// the engine knows.
    fn exec_builtin_stmt(&mut self, body: &mut Body, e: ExprId) -> Option<Flow> {
        let ExprKind::Call { func_id, args, .. } = &body.exprs[e].kind else {
            return None;
        };
        let builtin = *self.ctfe_builtins.and_then(|m| m.get(func_id))?;
        let args: Vec<Operand> = args.iter().map(|a| a.expr).collect();
        match builtin {
            CtfeBuiltin::ArraySet => Some(self.exec_element_write(body, &args)),
            CtfeBuiltin::ArrayCopy => Some(self.exec_run_write(body, &args)),
            CtfeBuiltin::ColdPath => Some(Flow::Fallthrough(Lattice::Unevaluated)),
            CtfeBuiltin::ArrayGet
            | CtfeBuiltin::ArrayLen
            | CtfeBuiltin::ArrayNew
            | CtfeBuiltin::Select => None,
        }
    }

    fn exec_element_write(&mut self, body: &mut Body, args: &[Operand]) -> Flow {
        let [seq, index, element] = *args else {
            return Flow::Bail;
        };
        let Some((root, path)) = self.frame_place_of(body, seq) else {
            return Flow::Bail;
        };
        let (Lattice::Const(index), Lattice::Const(element)) = (
            self.eval_operand(body, index),
            self.eval_operand(body, element),
        ) else {
            return Flow::Bail;
        };
        let Some((index, _)) = index.as_int() else {
            return Flow::Bail;
        };
        match self.update_place(root, &path, |seq| seq.with_element(index, element)) {
            Some(()) => Flow::Fallthrough(Lattice::Unevaluated),
            None => Flow::Bail,
        }
    }

    /// Splices `len` of `src`'s elements into the destination. A run either
    /// side cannot supply is left to trap at run time.
    fn exec_run_write(&mut self, body: &mut Body, args: &[Operand]) -> Flow {
        let [destination, at, source, from, len] = *args else {
            return Flow::Bail;
        };
        let Some((root, path)) = self.frame_place_of(body, destination) else {
            return Flow::Bail;
        };
        let (Lattice::Const(at), Lattice::Const(source), Lattice::Const(from), Lattice::Const(len)) = (
            self.eval_operand(body, at),
            self.eval_operand(body, source),
            self.eval_operand(body, from),
            self.eval_operand(body, len),
        ) else {
            return Flow::Bail;
        };
        let (Some((at, _)), Some((from, _)), Some((len, _))) =
            (at.as_int(), from.as_int(), len.as_int())
        else {
            return Flow::Bail;
        };
        match self.update_place(root, &path, |destination| {
            destination.with_run(at, &source, from, len)
        }) {
            Some(()) => Flow::Fallthrough(Lattice::Unevaluated),
            None => Flow::Bail,
        }
    }

    fn eval_optional_operand(&mut self, body: &mut Body, op: Option<Operand>) -> Lattice {
        match op {
            Some(op) => self.eval_operand(body, op),
            None => Lattice::Unevaluated,
        }
    }

    /// Reducing in place first is what lets a nested call fold before the
    /// operand is projected.
    fn eval_operand(&mut self, body: &mut Body, op: Operand) -> Lattice {
        match op.as_expr() {
            Some(e) => {
                self.reduce_in_place(body, e);
                self.reduce_to_lattice(body, e)
            }
            None => self.operand_to_lattice(body, op),
        }
    }

    /// Bind a local inside a compile-time frame. A local the frame may reach
    /// through another handle keeps no value.
    ///
    /// Both outcomes go through [`Interpreter::bind_local`], which is where a
    /// binding displaces a place alias the index held. A clobbered local that
    /// wrote its `NonConst` straight into the environment would leave the
    /// alias standing, and a later write through it would land in the local
    /// the alias still named.
    fn bind_ctfe_local(&mut self, index: u32, lattice: Lattice) {
        let lattice = if self.frame.ctfe_clobbered.contains(index) {
            Lattice::NonConst
        } else {
            lattice
        };
        self.bind_local(index, lattice);
    }

    /// The operand a `let` binds as a place rather than as a value: a borrow
    /// names the place it is taken over, and a bare local that already names
    /// one rebinds the same place — copying a reference copies the reference,
    /// not its referent, so both handles must reach the same storage.
    ///
    /// Nothing else qualifies. A projection out of an alias reads *through*
    /// the reference and yields a value, and `*p` is the same act spelled with
    /// a deref — so the RHS is matched as a local written out, not through
    /// [`place_of`], which peels a deref precisely because a write target
    /// wants the storage behind it.
    fn aliased_operand(&self, body: &Body, value: Operand) -> Option<Operand> {
        if let Some((_, borrowed)) = borrowed_place_operand(body, value) {
            return Some(borrowed);
        }
        let index = named_local(body, value)?;
        self.frame
            .place_aliases
            .contains_key(&index)
            .then_some(value)
    }

    /// Resolve `index` to the place `inner` borrows, so later reads and writes
    /// through it reach the borrowed storage rather than a copy. Recorded
    /// pre-flattened through the aliases already in effect, so a chain never
    /// needs chasing. An alias over its own storage — an index reuse this
    /// frame cannot express — refuses instead.
    fn record_place_alias(&mut self, body: &Body, index: u32, inner: Operand) -> Option<()> {
        let (root, path) = self.frame_place_of(body, inner)?;
        if root == index {
            return None;
        }
        self.frame.env.swap_remove(&index);
        self.frame.place_aliases.insert(index, (root, path));
        Some(())
    }

    /// [`place_of`] with the frame's borrow aliases applied: a chain rooted at
    /// an alias re-roots at the place the alias was taken over.
    pub(super) fn frame_place_of(&self, body: &Body, op: Operand) -> Option<(u32, Vec<u32>)> {
        let (root, mut path) = place_of(body, op)?;
        match self.frame.place_aliases.get(&root) {
            Some((alias_root, alias_path)) => {
                let mut full = alias_path.clone();
                full.append(&mut path);
                Some((*alias_root, full))
            }
            None => Some((root, path)),
        }
    }

    /// Fold a call to the value it computes. One that writes through a `&mut`
    /// parameter is never folded here: this projection is re-entrant, and a
    /// write applied twice is worse than one not folded at all. Those run at
    /// statement position, where the executor applies the writes.
    /// `Unevaluated` on any miss, so the original call — and any runtime trap
    /// inside it — survives.
    pub(super) fn try_call_fold(&mut self, body: &Body, e: ExprId) -> Lattice {
        if let lattice @ (Lattice::Const(_) | Lattice::NonConst) =
            self.try_ctfe_builtin_fold(body, e)
        {
            return match lattice {
                Lattice::Const(v) => Lattice::Const(v),
                Lattice::NonConst | Lattice::Unevaluated => Lattice::Unevaluated,
            };
        }
        match self.run_call(body, e, false) {
            Some(run) => match run.result {
                c @ Lattice::Const(_) => c,
                Lattice::NonConst | Lattice::Unevaluated => Lattice::Unevaluated,
            },
            None => Lattice::Unevaluated,
        }
    }

    /// The callee a call names, and the operands bound to its parameters. A
    /// method's receiver is its first.
    fn call_target(&self, body: &Body, e: ExprId) -> Option<(CalleeKey, Vec<Operand>)> {
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
    ///
    /// The body runs on a scratch copy, so the callee's shared arena — held
    /// under an immutable borrow — is never mutated.
    fn run_call(&mut self, body: &Body, e: ExprId, may_write: bool) -> Option<CallRun> {
        let callees = self.callees?;
        let (key, args) = self.call_target(body, e)?;
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
        let returns_unit = callee.return_type == TypeTable::UNIT;

        let mut bound: Vec<(u32, Value)> = Vec::with_capacity(args.len());
        let mut targets: Vec<(u32, u32, Vec<u32>)> = Vec::new();
        let mut places: Vec<(u32, Vec<u32>)> = Vec::new();
        for (arg, param) in args.iter().zip(&callee.params) {
            let place = self.frame_place_of(body, *arg);
            let value = if param.is_mut_ref {
                let (root, path) = place.clone()?;
                let value = self.place_value(root, &path)?;
                targets.push((param.local_index, root, path));
                value
            } else {
                self.operand_lattice_folded(body, *arg).as_const()?
            };
            places.extend(place);
            bound.push((param.local_index, value));
        }
        if aliased_write_targets(&targets, &places) {
            return None;
        }

        self.step_budget -= 1;
        self.call_stack.push(key);
        let mut scratch = callee_body.nodes_only_clone();
        let reached = Reached::in_frame(&scratch, self.ctfe_builtins, self.callees);
        let caller = self.swap_frame(FrameState::for_call(&scratch, &reached, bound));
        let run = self.exec_frame(&mut scratch, targets, returns_unit);
        self.swap_frame(caller);
        self.call_stack.pop();
        run
    }

    /// Run the scratch body already installed as the current frame, reporting
    /// what it returned and what it left in each `&mut` parameter.
    ///
    /// `None` rather than an empty write list when a `&mut` parameter has no
    /// value to write: losing it would leave the caller's place holding what
    /// the program never produced.
    ///
    /// `returns_unit` discards the result — a unit callee denotes nothing, and
    /// a value left where the call stood fails validation.
    ///
    /// Every fallible step of a call lives here, so the caller restores its own
    /// frame unconditionally on return.
    fn exec_frame(
        &mut self,
        scratch: &mut Body,
        targets: Vec<(u32, u32, Vec<u32>)>,
        returns_unit: bool,
    ) -> Option<CallRun> {
        let root = scratch.root;
        let flow = self.exec_block(scratch, root);
        let completed = matches!(flow, Flow::Return(_) | Flow::Fallthrough(_));
        let result = match flow {
            Flow::Return(lattice) | Flow::Fallthrough(lattice) if !returns_unit => lattice,
            Flow::Return(_)
            | Flow::Fallthrough(_)
            | Flow::Break { .. }
            | Flow::Continue
            | Flow::Bail => Lattice::Unevaluated,
        };
        if !completed {
            return None;
        }
        let writes = targets
            .into_iter()
            .map(|(index, root, path)| {
                let value = self.frame.env.get(&index)?.as_const()?;
                Some((root, path, value))
            })
            .collect::<Option<Vec<_>>>()?;
        Some(CallRun { result, writes })
    }

    /// The constant a self-contained region denotes: the block runs as a frame
    /// the engine starts from scratch, since a region that builds its value in
    /// locals of its own and touches only those is as self-contained as a call
    /// body. An outer local the region only reads is seeded from the walker's
    /// environment when it holds a constant there — a value snapshot at the
    /// region's own flow point, sound because the region cannot write it back:
    /// [`region_free_reads`] rejects write positions and reference-typed
    /// mentions, and a local involved in a live place alias is refused here.
    /// `None` when the block is not such a region, or when running
    /// it does not finish on a constant — the block survives as written, so
    /// anything it would do at run time still happens there.
    ///
    /// A region of reference type is refused whatever it reads: its value
    /// would materialize as a fresh literal where the program yields an alias,
    /// and `ref.eq` can tell the two apart.
    ///
    /// Safe on the re-entrant projection path even though it executes writes:
    /// self-containment confines every write to locals of the scratch run, so
    /// nothing is performed twice against the program's own state. A run an
    /// earlier visit already made answers from the fold memo, since the
    /// scratch sink promotes nothing and the region would otherwise be re-run
    /// at every visit.
    pub(super) fn try_region_fold(&mut self, body: &Body, e: ExprId) -> Option<Value> {
        let (block, label) = region_shape(body, e)?;
        if let Some(value) = self.frame.scratch_folds.get(&e) {
            return Some(value.clone());
        }
        if RefKind::from_resolved(self.type_table.get(body.exprs[e].type_id)).is_some() {
            return None;
        }
        if self.frame.region_misses.contains(&e) {
            return None;
        }
        let free = region_free_reads(
            body,
            block,
            self.callees,
            self.ctfe_builtins,
            self.type_table,
        )?;
        let mut seeds: Vec<(u32, Value)> = Vec::with_capacity(free.len());
        for index in free {
            if self.frame.alias_involves(index) {
                crate::compiler_trace!("region_seed", "region {e:?}: local {index} is aliased");
                return None;
            }
            let Some(Lattice::Const(value)) = self.frame.env.get(&index) else {
                crate::compiler_trace!(
                    "region_seed",
                    "region {e:?}: local {index} not constant in the walker env"
                );
                return None;
            };
            seeds.push((index, value.clone()));
        }
        self.charge_body_copy(body)?;
        let mut scratch = body.nodes_only_clone();
        scratch.root = block;
        let reached = Reached::in_frame(&scratch, self.ctfe_builtins, self.callees);
        let region = FrameState::for_call(&scratch, &reached, seeds);
        let caller = self.swap_frame(region);
        let flow = self.exec_block(&mut scratch, block);
        self.swap_frame(caller);
        let lattice = match flow {
            Flow::Fallthrough(lattice) => lattice,
            Flow::Break {
                label: Some(broke),
                value,
            } if label == Some(broke.as_str()) => value,
            Flow::Break { .. } | Flow::Return(_) | Flow::Continue | Flow::Bail => {
                crate::compiler_trace!("region_seed", "region {e:?}: run abandoned");
                self.frame.region_misses.insert(e);
                return None;
            }
        };
        let value = lattice.as_const();
        if value.is_none() {
            self.frame.region_misses.insert(e);
        }
        value
    }

    /// Charge the step budget for the copy of `body` a region run makes before
    /// it executes a statement. Uncharged, a region that reaches the run and
    /// then abandons costs a whole-body copy per visit, which makes const
    /// folding quadratic in the size of a function full of templates. `None`
    /// when the budget cannot cover it.
    fn charge_body_copy(&mut self, body: &Body) -> Option<()> {
        let cost = u32::try_from(body.exprs.len() / COPY_CHARGE_DIVISOR)
            .unwrap_or(u32::MAX)
            .max(1);
        self.step_budget = self.step_budget.checked_sub(cost)?;
        Some(())
    }

    /// The frame's value for a place, or `None` when it holds none.
    pub(super) fn place_value(&self, root: u32, path: &[u32]) -> Option<Value> {
        let Lattice::Const(value) = self.frame.env.get(&root)? else {
            return None;
        };
        path.iter().try_fold(value, |v, i| v.field(*i)).cloned()
    }

    /// Write a finished run's `&mut` parameters back into the caller's places.
    fn apply_writes(&mut self, writes: Vec<(u32, Vec<u32>, Value)>) -> Option<()> {
        for (root, path, value) in writes {
            if path.is_empty() {
                self.bind_ctfe_local(root, Lattice::Const(value));
            } else {
                self.update_place(root, &path, |_| Some(value))?;
            }
        }
        Some(())
    }
}
