//! NIR Interpreter (niri): a compile-time partial evaluator over the arena
//! `Body`, reducing what it can and leaving a residual otherwise. Reduction is
//! monotone and idempotent. Each submodule answers one question — `lattice`,
//! `frame`, `rewrite`, `trackability`, `pattern`, `place`, `region`, `callee`.
//! What it can evaluate: `docs/wep-2026-04-27-nir-interpreter.md`.

use crate::const_eval::Value;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::{NirFunction, NirUnaryOp};
use crate::nir_arena::{
    BlockId, BlockNode, Body, ExprId, ExprKind, ExprNode, LocalSet, Operand, PatId, PatKind,
    StmtId, StmtKind, StmtNode,
};
use crate::nir_value_graph::ValueKind;
use crate::tir::{TypeId, TypeTable};

/// Three-state lattice over compile-time evaluation results, ordered
/// `Unevaluated` ⊑ `Const(v)` ⊑ `NonConst` — the SCCP lattice with
/// `Unevaluated` as Bottom and `NonConst` as Top.
///
/// Three states rather than `Option<Value>`: one `None` for both "not computed
/// yet" and "known not to be a constant" cannot say whether a re-attempt would
/// succeed, which makes memoization unsound.
#[derive(Debug, Clone, PartialEq)]
pub enum Lattice {
    /// No information yet. Default for un-bound locals and for node kinds the
    /// engine does not evaluate.
    Unevaluated,
    /// Provably reduces to this value.
    Const(Value),
    /// Cannot be a reusable constant: a `let mut` binding, a runtime-only
    /// result, or a fold over `NonConst` operands.
    NonConst,
}

impl Lattice {
    /// The value when `Const`, else `None`. Pattern-match the variant instead
    /// where the `Unevaluated` / `NonConst` distinction matters.
    ///
    /// Owned rather than borrowed, and that is not the copy it reads as: a
    /// [`Value`] keeps its aggregate and sequence backings behind `Rc`, so the
    /// clone is a refcount bump whatever the value's size. There is no
    /// borrowing variant because there would be nothing to win by it.
    #[must_use]
    pub fn as_const(&self) -> Option<Value> {
        match self {
            Self::Const(v) => Some(v.clone()),
            Self::Unevaluated | Self::NonConst => None,
        }
    }

    /// The SCCP join over `Unevaluated ⊑ Const(v) ⊑ NonConst`, used to merge
    /// the arms of a branch whose condition is not constant.
    ///
    /// An `Unevaluated` arm contributes nothing, which is the infeasible-edge
    /// rule: it is only sound where that arm really is unreachable, so a
    /// reachable one is promoted before it gets here (see
    /// `arm_lattice_for_feasible_join`).
    #[must_use]
    pub fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unevaluated, x) | (x, Self::Unevaluated) => x,
            (Self::NonConst, _) | (_, Self::NonConst) => Self::NonConst,
            (Self::Const(a), Self::Const(b)) if a.denotes_same(&b) => Self::Const(a),
            (Self::Const(_), Self::Const(_)) => Self::NonConst,
        }
    }
}

/// The builtins the engine evaluates. An element read or write reaches NIR as
/// a call to one of these, not as an `Index` node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtfeBuiltin {
    ArrayGet,
    ArrayLen,
    ArrayNew,
    ArraySet,
    ArrayCopy,
    ArrayClonePrefix,
    ColdPath,
    Select,
    I32AsChar,
}

impl CtfeBuiltin {
    /// Whether the builtin writes its first argument. A write is performed at
    /// statement position rather than folded, and only by a frame that owns
    /// the place it lands in.
    fn is_write(self) -> bool {
        match self {
            Self::ArraySet | Self::ArrayCopy => true,
            Self::ArrayGet
            | Self::ArrayLen
            | Self::ArrayNew
            | Self::ArrayClonePrefix
            | Self::ColdPath
            | Self::Select
            | Self::I32AsChar => false,
        }
    }
}

/// Which sequence builtin each callee id is, for the ids that are one.
pub type CtfeBuiltinMap = IndexMap<CalleeKey, CtfeBuiltin>;

/// Identity of a module-scope global, as `ExprKind::GlobalVarGet` names one.
pub type GlobalKey = (ModuleSource, String);

/// Lattice values for module-scope globals, built once per pass by reducing
/// each non-`mut` global's initializer. A mutable global is recorded
/// [`Lattice::NonConst`]; an absent key reads as [`Lattice::Unevaluated`], the
/// same convention as an un-bound local.
pub type GlobalEnv = IndexMap<GlobalKey, Lattice>;

/// Known constant field values of module-scope globals, keyed by global then
/// field name. It knows fields no initializer shows — such as the length body
/// globalization records for a hoisted sequence.
pub type GlobalFieldEnv = IndexMap<GlobalKey, IndexMap<String, Value>>;

/// Ceiling on total CTFE work, charged per call entry, statement, and loop
/// iteration. Reset per function so what one function spends cannot decide
/// whether the next one folds.
pub const DEFAULT_STEP_BUDGET: u32 = 10_000;

mod callee;
mod frame;
mod lattice;
mod pattern;
mod place;
mod region;
mod rewrite;
mod trackability;

pub use callee::{Callee, CalleeKey, CalleeMap};
use pattern::PatternMatch;
use trackability::Trackability;

/// Commit sink for niri's body rewrites, so one set of rewrites serves two
/// backends: [`BodySink`] over a throwaway CTFE body, and the optimize layer's
/// `EngineSink`, which keeps the real body's maps coherent.
pub trait EditSink {
    fn body(&self) -> &Body;
    /// Replace `e`'s kind. The new kind's children must already be parented to
    /// `e` (literals have none); use [`EditSink::become_expr`] to move an
    /// existing node's content into `e`.
    fn replace_kind(&mut self, e: ExprId, kind: ExprKind);
    /// Promote `e` to the folded pure scalar `value`, reporting whether the edit
    /// was applied. An aggregate is declined — the
    /// pool models scalars only — as is every value on the scratch backend,
    /// whose reads recompute through the lattice and need no write-back.
    fn replace_with_value(&mut self, e: ExprId, value: Value) -> bool;
    /// Intern a constant into the function's value pool and return it as an
    /// operand. A scalar has no literal-node form, so a synthesized one in an
    /// operand position goes through the pool.
    fn const_operand(&mut self, kind: ValueKind, type_id: TypeId) -> Operand;
    /// Make `dst` take `src`'s content (`dst` becomes `src`).
    fn become_expr(&mut self, dst: ExprId, src: ExprId);
    fn alloc_expr(&mut self, kind: ExprKind, type_id: TypeId, span: crate::token::Span) -> ExprId;
    fn alloc_stmt(&mut self, kind: StmtKind, span: crate::token::Span) -> StmtId;
    fn alloc_block(&mut self, stmts: Vec<StmtId>, span: crate::token::Span) -> BlockId;
    fn set_block_stmts(&mut self, block: BlockId, stmts: Vec<StmtId>);
}

/// In-place [`EditSink`] over a raw `Body`. Used for CTFE scratch reduction,
/// where the body is discarded after the value is read, so duplicated child
/// references and stale parent links do not matter.
pub struct BodySink<'a> {
    pub body: &'a mut Body,
}

impl EditSink for BodySink<'_> {
    fn body(&self) -> &Body {
        self.body
    }
    fn replace_kind(&mut self, e: ExprId, kind: ExprKind) {
        self.body.exprs[e].kind = kind;
    }
    fn replace_with_value(&mut self, _e: ExprId, _value: Value) -> bool {
        false
    }
    fn const_operand(&mut self, kind: ValueKind, type_id: TypeId) -> Operand {
        Operand::Value(self.body.values.alloc_unshared(kind, type_id))
    }
    fn become_expr(&mut self, dst: ExprId, src: ExprId) {
        let node = self.body.exprs[src].clone();
        self.body.exprs[dst] = node;
    }
    fn alloc_expr(&mut self, kind: ExprKind, type_id: TypeId, span: crate::token::Span) -> ExprId {
        self.body.exprs.push(ExprNode {
            kind,
            type_id,
            span,
        })
    }
    fn alloc_stmt(&mut self, kind: StmtKind, span: crate::token::Span) -> StmtId {
        self.body.stmts.push(StmtNode { kind, span })
    }
    fn alloc_block(&mut self, stmts: Vec<StmtId>, span: crate::token::Span) -> BlockId {
        self.body.blocks.push(BlockNode { stmts, span })
    }
    fn set_block_stmts(&mut self, block: BlockId, stmts: Vec<StmtId>) {
        self.body.blocks[block].stmts = stmts;
    }
}

/// Whether a compile-time frame can run `func`'s body: pure, and concrete —
/// `type_params` and `impl_type_params` empty, since CTFE runs after
/// monomorphization.
///
/// Neither `inline_hint` nor `stores` is consulted: where a body is placed says
/// nothing about compile-time knowability, and a storing callee still runs for
/// the writes it performs — `run_call` refuses only its result.
#[must_use]
pub fn is_ctfe_runnable(func: &NirFunction) -> bool {
    func.effects.is_empty()
        && func.body.is_some()
        && !func.is_cm_binding
        && !func.is_dispatch_wrapper
        && !func.is_cm_export
        && !func.is_async
        && func.task_return_type.is_none()
        && func.type_params.is_empty()
        && func.impl_type_params.is_empty()
}

/// Whether `func`'s call can be replaced by the value it computes: runnable,
/// producing a value at all, and keeping no reference past the call.
///
/// Strictly stronger than [`is_ctfe_runnable`], which the [`CalleeMap`] gates
/// on: a frame runs a unit callee for the writes it performs, so requiring a
/// value there would refuse work the frame does. This is what a caller asks
/// when it wants to hold the result.
#[must_use]
pub fn is_ctfe_eligible(func: &NirFunction) -> bool {
    func.return_type != crate::tir::TypeTable::UNIT
        && func.stores.is_empty()
        && is_ctfe_runnable(func)
}

/// Everything the engine knows about the body it is walking, which is
/// per-function, so entering a body means replacing the whole group and leaving
/// one means putting it back. Grouped rather than swapped field by field: a
/// compile-time frame exchanges all of it at once, and a member restored out of
/// step with its siblings would let one body read another's locals.
///
/// Most of it is keyed by local index; the two memos at the end are keyed by
/// `ExprId`, and both belong to the body those ids index — which is what makes
/// swapping the group wholesale the only sound way to enter a frame.
#[derive(Default)]
struct FrameState {
    /// Lattice values for the `let`-bound locals of the body being walked. An
    /// absent local reads as [`Lattice::Unevaluated`].
    env: IndexMap<u32, Lattice>,
    /// Locals a `let` bound to `&GLOBAL`, with the statement that bound each
    /// one so a read through the alias can re-derive the fact rather than
    /// trust it. Nothing about a body identifies it across an in-place fold —
    /// the arena grows as nodes are interned — so the witness is the binding
    /// itself, which a different body does not carry at that id.
    ref_global_aliases: IndexMap<u32, (StmtId, GlobalKey)>,
    /// Locals a frame's `let` bound to a borrow of a local place, resolved to
    /// the place borrowed — flattened at record time, so a chain never needs
    /// chasing. Reads through one project the place's current value and writes
    /// land in it, which is what keeps a borrow from copying its referent.
    /// Populated only during frame execution; empty everywhere else.
    place_aliases: IndexMap<u32, (u32, Vec<u32>)>,
    /// Locals this frame saw a `let` bind to a borrow, mapped to the local the
    /// referent lives in — `&mut xs[i]` reduces to a borrow of an accessor
    /// result, which still names `xs` — or `None` where the borrow resolved to
    /// nothing nameable. An absent local is a reference from outside the frame,
    /// a parameter above all, whose referent no tracked local holds.
    ref_roots: IndexMap<u32, Option<u32>>,
    /// The body's [`aggregate_safe_locals`] — the only locals that may bind an
    /// aggregate constant. An unpopulated set refuses every aggregate binding.
    aggregate_locals: LocalSet,
    /// Locals a compile-time frame cannot track — see [`clobbered_locals`].
    /// Empty outside a frame.
    ctfe_clobbered: LocalSet,
    alias_classes: AliasClasses,
    /// What this frame folded a node to, read back by
    /// [`Interpreter::expr_to_lattice`]. Load-bearing on both backends: the
    /// scratch body promotes nothing, and on a real body the committing rewrite
    /// consumes the node that produced the value, so an enclosing fold has
    /// nowhere else to read it. Cleared wherever the environment restarts.
    scratch_folds: IndexMap<ExprId, Value>,
    /// Regions whose run this frame already attempted and abandoned. A seed's
    /// value is fixed for the frame's flow (a reassigned local is never
    /// `Const` here), so a failed run stays failed and re-running it would
    /// re-pay the body copy at every visit. Cleared with [`Self::scratch_folds`]
    /// wherever the environment restarts.
    ///
    /// Unlike [`Self::scratch_folds`] this records no value, so it is kept on
    /// the real body too: the worst an entry left over from a rewritten node
    /// can cost is a fold nobody attempts.
    region_misses: IndexSet<ExprId>,
}

/// What the engine knows beyond the body in front of it. Every field is
/// optional and every absence costs folds rather than correctness: no callee map
/// leaves a `Call` [`Lattice::Unevaluated`], no global env leaves a
/// `GlobalVarGet` so, and so on. The compiler runs the engine both with the
/// program-wide view and with only what running a call needs.
#[derive(Default, Clone, Copy)]
pub(crate) struct ProgramFacts<'a> {
    pub(crate) callees: Option<&'a CalleeMap>,
    pub(crate) ctfe_builtins: Option<&'a CtfeBuiltinMap>,
    globals: Option<&'a GlobalEnv>,
    global_fields: Option<&'a GlobalFieldEnv>,
}

/// Partial evaluator over the arena `Body`.
pub struct Interpreter<'a> {
    type_table: &'a TypeTable,
    frame: FrameState,
    facts: ProgramFacts<'a>,
    /// Hard ceiling on CTFE work before bailing. On zero, further attempts
    /// return `Unevaluated`.
    step_budget: u32,
    /// The callees whose bodies the engine is currently evaluating, in entry
    /// order. A `Call` to a key already on the stack reports `Unevaluated`
    /// immediately, so direct (`f → f`) and indirect (`f → g → f`) recursion
    /// terminate without consuming budget. The `RefCell` borrow guard cannot
    /// serve this role, since it permits concurrent immutable borrows.
    call_stack: Vec<CalleeKey>,
    /// Runs this walk abandoned. A run is a function of these three alone — a
    /// frame starts empty and the rest is fixed for the pass — so a failed one
    /// stays failed, and re-running it re-pays a whole-body copy for the same
    /// refusal.
    call_misses: Vec<CallMiss>,
}

struct CallMiss {
    callee: CalleeKey,
    may_write: bool,
    args: Vec<Value>,
}

/// Ceiling on remembered misses; the list is scanned linearly. Dropping one
/// costs a re-run, never an answer.
const MAX_CALL_MISSES: usize = 64;

fn let_ref_global(body: &Body, stmt: &StmtKind) -> Option<(u32, GlobalKey)> {
    let StmtKind::Let {
        local_index,
        value,
        is_mut,
        ..
    } = stmt
    else {
        return None;
    };
    if *is_mut {
        return None;
    }
    let ve = value.as_expr()?;
    let ExprKind::Unary {
        op: NirUnaryOp::Ref,
        expr,
    } = &body.exprs[ve].kind
    else {
        return None;
    };
    let ge = expr.as_expr()?;
    let ExprKind::GlobalVarGet {
        module_source,
        name,
    } = &body.exprs[ge].kind
    else {
        return None;
    };
    Some((*local_index, (module_source.clone(), name.clone())))
}

/// What running a call left behind: the value it returned, and what each `&mut`
/// parameter holds at return, paired with the caller place it belongs in.
struct CallRun {
    result: Lattice,
    writes: Vec<(u32, Vec<u32>, Value)>,
}

impl<'a> Interpreter<'a> {
    #[must_use]
    pub fn new(type_table: &'a TypeTable) -> Self {
        Self {
            type_table,
            frame: FrameState::default(),
            facts: ProgramFacts::default(),
            step_budget: DEFAULT_STEP_BUDGET,
            call_stack: Vec::new(),
            call_misses: Vec::new(),
        }
    }

    /// What is left of the CTFE work budget: a declined fold and an exhausted
    /// budget look alike from outside.
    #[must_use]
    pub fn step_budget(&self) -> u32 {
        self.step_budget
    }

    fn call_missed(&self, callee: CalleeKey, may_write: bool, args: &[Value]) -> bool {
        self.call_misses
            .iter()
            .any(|miss| miss.callee == callee && miss.may_write == may_write && miss.args == args)
    }

    fn record_call_miss(&mut self, callee: CalleeKey, may_write: bool, args: Vec<Value>) {
        if self.call_misses.len() < MAX_CALL_MISSES {
            self.call_misses.push(CallMiss {
                callee,
                may_write,
                args,
            });
        }
    }

    /// Install the [`CalleeMap`]. Without it, every `Call` node remains
    /// [`Lattice::Unevaluated`] — the engine has no body to look up.
    pub fn with_callees(&mut self, callees: &'a CalleeMap) -> &mut Self {
        self.facts.callees = Some(callees);
        self
    }

    /// Install the sequence-builtin lookup. Without it, an element or length
    /// read stays opaque.
    pub fn with_ctfe_builtins(&mut self, ctfe_builtins: &'a CtfeBuiltinMap) -> &mut Self {
        self.facts.ctfe_builtins = Some(ctfe_builtins);
        self
    }

    /// Install the [`GlobalEnv`]. Without it, every `GlobalVarGet` node remains
    /// [`Lattice::Unevaluated`] — the engine has no initializer lattice to look
    /// up.
    pub fn with_globals(&mut self, globals: &'a GlobalEnv) -> &mut Self {
        self.facts.globals = Some(globals);
        self
    }

    /// Install the [`GlobalFieldEnv`]. Without this,
    /// `FieldAccess(GlobalVarGet(_), _)` stays [`Lattice::Unevaluated`].
    /// Mirrors [`with_globals`](Self::with_globals).
    pub fn with_global_fields(&mut self, global_fields: &'a GlobalFieldEnv) -> &mut Self {
        self.facts.global_fields = Some(global_fields);
        self
    }

    /// Override the per-pass CTFE step budget (default
    /// [`DEFAULT_STEP_BUDGET`]).
    pub fn set_step_budget(&mut self, budget: u32) -> &mut Self {
        self.step_budget = budget;
        self
    }

    /// Install `state`, handing back what it displaced. Every per-local fact
    /// moves together, so there is no window in which one body's locals are
    /// read against another's — which is what a compile-time frame needs, and
    /// what makes leaving one a single statement that cannot be skipped.
    fn swap_frame(&mut self, state: FrameState) -> FrameState {
        std::mem::replace(&mut self.frame, state)
    }

    fn global_field(&self, key: &GlobalKey, field_name: &str) -> Lattice {
        self.facts
            .global_fields
            .and_then(|m| m.get(key))
            .and_then(|m| m.get(field_name))
            .cloned()
            .map_or(Lattice::Unevaluated, Lattice::Const)
    }

    pub fn record_alias_classes(&mut self, classes: AliasClasses) {
        self.frame.alias_classes = classes;
    }

    /// Record which of `body`'s locals may bind an aggregate constant. The
    /// driving visitor calls this once per function, next to
    /// [`Self::record_ref_global_aliases`].
    pub fn record_aggregate_locals(&mut self, body: &Body) {
        self.frame.aggregate_locals =
            Trackability::outside_frame(body, self.facts, self.type_table).aggregate_locals;
    }

    /// Record which locals a `let` bound to `&GLOBAL`.
    ///
    /// An index more than one binder names keeps none: the scan reads the whole
    /// arena — which is what lets a read fold through a binding an in-place
    /// rewrite displaced — so it cannot order two binders against each other.
    /// Pattern bindings count, since index reuse across a `let` and a match arm
    /// is real.
    pub fn record_ref_global_aliases(&mut self, body: &Body) {
        self.frame.ref_global_aliases.clear();
        let mut seen: IndexSet<u32> = IndexSet::default();
        let mut rebound: IndexSet<u32> = IndexSet::default();
        for (_, st) in &body.stmts {
            if let StmtKind::Let { local_index, .. } = &st.kind
                && !seen.insert(*local_index)
            {
                rebound.insert(*local_index);
            }
        }
        for (_, pat) in &body.pats {
            if let PatKind::Binding { local_index, .. } = &pat.kind
                && !seen.insert(*local_index)
            {
                rebound.insert(*local_index);
            }
        }
        for (id, st) in &body.stmts {
            if let Some((local, key)) = let_ref_global(body, &st.kind)
                && !rebound.contains(&local)
            {
                self.frame.ref_global_aliases.insert(local, (id, key));
            }
        }
    }

    /// Reset the per-function state, which the driving visitor must do before
    /// each body: local indices are per-function, so one function's bindings
    /// would otherwise read as the next one's. The step budget resets too, so
    /// a long compile-time loop cannot decide whether later functions fold.
    pub fn enter_function(&mut self) {
        self.step_budget = DEFAULT_STEP_BUDGET;
        self.frame = FrameState::default();
        self.call_misses.clear();
        debug_assert!(
            self.call_stack.is_empty(),
            "niri call_stack leaked across function boundary",
        );
    }

    /// Record a lattice value for a `let`-bound local: [`Lattice::Const`] for an
    /// immutable binding whose RHS reduced, [`Lattice::NonConst`] for `let mut`
    /// or an RHS that did not, and for an aggregate
    /// [`Self::record_aggregate_locals`] could not prove unaliased. The binding
    /// displaces any place alias the index held, and never restores it.
    pub fn bind_local(&mut self, index: u32, lattice: Lattice) {
        self.frame.place_aliases.swap_remove(&index);
        let unbacked_aggregate = matches!(&lattice, Lattice::Const(v) if !v.is_scalar())
            && !self.frame.aggregate_locals.contains(index);
        let lattice = if unbacked_aggregate {
            Lattice::NonConst
        } else {
            lattice
        };
        self.frame.env.insert(index, lattice);
    }

    /// The locals a match arm's pattern binds, with the values they take under
    /// a constant `scrutinee`. Empty unless the pattern definitely matches — an
    /// undecided pattern binds nothing knowable.
    #[must_use]
    pub fn arm_bindings(&self, body: &Body, scrutinee: Operand, pattern: PatId) -> PatBindings {
        let Lattice::Const(value) = self.operand_to_lattice(body, scrutinee) else {
            return PatBindings::new();
        };
        let mut binds = PatBindings::new();
        match self.pattern_matches(body, &value, pattern, &mut binds) {
            PatternMatch::Yes => binds,
            PatternMatch::No | PatternMatch::Unknown => PatBindings::new(),
        }
    }

    /// Install `binds` for the walk of one match arm. Pass the returned scope
    /// to [`Self::leave_arm`] to put the environment back.
    pub fn enter_arm(&mut self, binds: &PatBindings) -> ArmScope {
        let scope = ArmScope(
            binds
                .iter()
                .map(|(index, _)| (*index, self.frame.env.get(index).cloned()))
                .collect(),
        );
        for (index, value) in binds {
            self.bind_local(*index, Lattice::Const(value.clone()));
        }
        scope
    }

    /// The guard's value with `binds` in scope. The walker already reduced the
    /// guard there, so this is a read; scoping it again keeps the read from
    /// resolving a binding's index against whatever holds it outside the arm.
    fn guard_under_bindings(
        &mut self,
        body: &Body,
        guard: Operand,
        binds: &PatBindings,
    ) -> Option<bool> {
        let scope = self.enter_arm(binds);
        let value = self.operand_to_lattice(body, guard).as_const();
        self.leave_arm(scope);
        value.and_then(|v| v.as_bool())
    }

    pub fn leave_arm(&mut self, scope: ArmScope) {
        for (index, previous) in scope.0 {
            match previous {
                Some(lattice) => self.frame.env.insert(index, lattice),
                None => self.frame.env.swap_remove(&index),
            };
        }
    }

    /// Mark a local as definitely non-constant from this point on, as an
    /// `x = expr` assignment makes it. The new value is not tracked; only the
    /// prior binding is dropped.
    pub fn invalidate_local(&mut self, index: u32) {
        self.frame.env.insert(index, Lattice::NonConst);
    }

    /// Drop what the frame holds for the storage `place` writes into and for
    /// every local naming it: the frame tracks values per local, not per object.
    pub fn invalidate_place(&mut self, body: &Body, place: Operand) {
        let Some((root, _)) = self.frame_place_of(body, place) else {
            // A place no frame root names — through a call result, a global —
            // may still be reachable from a tracked local.
            self.invalidate_every_local();
            return;
        };
        // A write through a reference lands in its referent, so dropping the
        // reference alone would leave the referent's stale value foldable.
        // `frame_place_of` re-roots to that referent for a borrow taken over a
        // place the frame can name; one taken over anything else — an accessor
        // result, as `&mut xs[i]` reduces to — comes back still rooted at the
        // reference, and where it points is unknown.
        // A write through a reference lands in its referent, so dropping the
        // reference alone would leave the referent's stale value foldable.
        // `frame_place_of` re-roots to that referent for a borrow taken over a
        // place the frame can spell; `ref_roots` names the local for one taken
        // over an accessor result; anything else points somewhere unknown.
        if place::place_of(body, place).is_some_and(|(raw, _)| raw == root)
            && place_roots_at_reference(body, place, self.type_table)
        {
            match self.frame.ref_roots.get(&root).copied() {
                Some(Some(target)) => {
                    self.invalidate_local(target);
                    for member in self.frame.alias_classes.members(target).to_vec() {
                        self.invalidate_local(member);
                    }
                    return;
                }
                Some(None) => {
                    self.invalidate_every_local();
                    return;
                }
                // A reference the frame never saw bound points outside it — a
                // parameter above all — so no tracked local holds its referent.
                None => {}
            }
        }
        self.invalidate_local(root);
        for member in self.frame.alias_classes.members(root).to_vec() {
            self.invalidate_local(member);
        }
    }

    /// Record that `index` borrows into `target`'s storage at a path the frame
    /// cannot spell. Cleared by a rebind of `index`.
    pub fn record_ref_root(&mut self, index: u32, borrow: Option<Option<u32>>) {
        match borrow {
            Some(target) => self.frame.ref_roots.insert(index, target),
            None => self.frame.ref_roots.swap_remove(&index),
        };
    }

    /// Drop every tracked value: a write whose destination the frame cannot
    /// name may have landed in any of them.
    fn invalidate_every_local(&mut self) {
        for lattice in self.frame.env.values_mut() {
            *lattice = Lattice::NonConst;
        }
    }
}

/// Whether the place chain bottoms out at a reference — a write through it
/// reaches the referent, not the handle.
fn place_roots_at_reference(body: &Body, place: Operand, type_table: &TypeTable) -> bool {
    let Some(mut e) = place.as_expr() else {
        return false;
    };
    loop {
        match &body.exprs[e].kind {
            ExprKind::FieldAccess { expr: inner, .. }
            | ExprKind::Index { expr: inner, .. }
            | ExprKind::Cast { expr: inner, .. }
            | ExprKind::VariantPayload { expr: inner, .. }
            | ExprKind::Unary {
                op: NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref,
                expr: inner,
            } => match inner.as_expr() {
                Some(next) => e = next,
                None => return false,
            },
            _ => {
                return matches!(
                    type_table.get(body.exprs[e].type_id),
                    crate::tir::ResolvedType::Ref(_) | crate::tir::ResolvedType::MutRef(_)
                );
            }
        }
    }
}

/// Locals that may name one another's storage.
#[derive(Default, Clone, Debug)]
pub struct AliasClasses {
    root_of: IndexMap<u32, u32>,
    members_of: IndexMap<u32, Vec<u32>>,
}

impl AliasClasses {
    #[must_use]
    pub fn new(root_of: IndexMap<u32, u32>, members_of: IndexMap<u32, Vec<u32>>) -> Self {
        Self {
            root_of,
            members_of,
        }
    }

    /// The locals sharing `local`'s storage, itself included. Empty for a local
    /// no copy aliases.
    #[must_use]
    pub fn members(&self, local: u32) -> &[u32] {
        self.root_of
            .get(&local)
            .and_then(|root| self.members_of.get(root))
            .map_or(&[], Vec::as_slice)
    }
}

/// The locals a matched pattern binds, paired with the scrutinee sub-values
/// they take.
pub type PatBindings = Vec<(u32, Value)>;

/// The environment entries [`Interpreter::enter_arm`] displaced, restored by
/// [`Interpreter::leave_arm`].
pub struct ArmScope(Vec<(u32, Option<Lattice>)>);
