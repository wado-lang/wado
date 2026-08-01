//! NIR Interpreter (niri).
//!
//! Compile-time partial evaluator over the arena `Body`: it reduces what it can
//! and leaves a residual otherwise.
//!
//! Reduction is monotone and idempotent — an expression only moves toward
//! literal form — so a literal leaf survives a no-op pass as written.
//!
//! Each module answers one question:
//!
//! - `lattice` — what an expression denotes.
//! - `frame` — what running a body does.
//! - `rewrite` — what becomes of an expression once its value is known.
//! - `trackability` — which locals a walk may hold a value for.
//! - `pattern` — whether a pattern matches a value.
//! - `place` — what a borrow or lvalue chain names.
//! - `region` — which blocks are self-contained enough to run as a frame.
//!
//! What the engine can evaluate is stated in
//! `docs/wep-2026-04-27-nir-interpreter.md`, not here.

use crate::const_eval::Value;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::{NirFunction, NirUnaryOp};
use crate::nir_arena::{
    BlockId, BlockNode, Body, ExprId, ExprKind, ExprNode, LocalSet, NodeRef, Operand, PatId,
    StmtId, StmtKind, StmtNode,
};
use crate::nir_value_graph::ValueKind;
use crate::nir_visitor::NirRefVisitor;
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

/// How many body nodes a region run's copy charges as one step. The copy is
/// real work the budget must see, but it is bulk memory rather than
/// interpretation, so it costs a fraction of what executing that many
/// statements would.
pub const COPY_CHARGE_DIVISOR: usize = 16;

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
pub(crate) trait EditSink {
    fn body(&self) -> &Body;
    /// Whether a value this sink declines has to be remembered for later
    /// lattice reads.
    ///
    /// Only the scratch backend needs it: it promotes nothing, so a decline
    /// there loses the fold outright. A real body keeps the node it folded
    /// from, so a later read recomputes the same constant — and a value memo
    /// keyed by `ExprId` would go stale the moment a rewrite gives that id new
    /// content.
    fn memoizes_declined_folds(&self) -> bool {
        false
    }
    /// Replace `e`'s kind. The new kind's children must already be parented to
    /// `e` (literals have none); use [`EditSink::become_expr`] to move an
    /// existing node's content into `e`.
    fn replace_kind(&mut self, e: ExprId, kind: ExprKind);
    /// Promote `e` to the folded pure scalar `value`, reporting whether the edit
    /// was applied (WEP: The Live `ValueGraph`). An aggregate is declined — the
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
pub(crate) struct BodySink<'a> {
    pub body: &'a mut Body,
}

impl EditSink for BodySink<'_> {
    fn body(&self) -> &Body {
        self.body
    }
    fn memoizes_declined_folds(&self) -> bool {
        true
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
/// Neither `inline_hint` nor `stores` is consulted. Where a body is placed says
/// nothing about compile-time knowability, and what a callee keeps is a
/// snapshot that `Reached` is what holds sound.
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
    ref_global_aliases: IndexMap<u32, GlobalKey>,
    /// The body [`Self::ref_global_aliases`] was recorded for, so a read
    /// through one can check it is still that body.
    alias_body: Option<BodyShape>,
    /// Locals a frame's `let` bound to a borrow of a local place, resolved to
    /// the place borrowed — flattened at record time, so a chain never needs
    /// chasing. Reads through one project the place's current value and writes
    /// land in it, which is what keeps a borrow from copying its referent.
    /// Populated only during frame execution; empty everywhere else.
    place_aliases: IndexMap<u32, (u32, Vec<u32>)>,
    /// The body's [`aggregate_safe_locals`] — the only locals that may bind an
    /// aggregate constant. An unpopulated set refuses every aggregate binding.
    aggregate_locals: LocalSet,
    /// Locals a compile-time frame cannot track — see [`clobbered_locals`].
    /// Empty outside a frame.
    ctfe_clobbered: LocalSet,
    /// CTFE scratch-body fold memo, read back by
    /// [`Interpreter::expr_to_lattice`]. The scratch [`BodySink`] promotes
    /// nothing, so a fold has nowhere else to be recorded.
    ///
    /// Written only for a sink that asks for it
    /// ([`EditSink::memoizes_declined_folds`]), which is the scratch backend
    /// alone: a real body keeps the node the value was folded from, so a later
    /// read recomputes it, and remembering a value against an `ExprId` the
    /// engine may hand new content to is how a memo goes stale.
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

/// Partial evaluator over the arena `Body`.
pub struct Interpreter<'a> {
    type_table: &'a TypeTable,
    frame: FrameState,
    /// When `None`, a `Call` node stays [`Lattice::Unevaluated`].
    callees: Option<&'a CalleeMap>,
    /// When `None`, an `array_get` / `array_len` call is just an opaque call.
    ctfe_builtins: Option<&'a CtfeBuiltinMap>,
    /// When `None`, a `GlobalVarGet` stays [`Lattice::Unevaluated`].
    globals: Option<&'a GlobalEnv>,
    /// When `None`, `FieldAccess(GlobalVarGet(_), _)` stays
    /// [`Lattice::Unevaluated`].
    global_fields: Option<&'a GlobalFieldEnv>,
    /// Hard ceiling on CTFE work before bailing. On zero, further attempts
    /// return `Unevaluated`.
    step_budget: u32,
    /// The callees whose bodies the engine is currently evaluating, in entry
    /// order. A `Call` to a key already on the stack reports `Unevaluated`
    /// immediately, so direct (`f → f`) and indirect (`f → g → f`) recursion
    /// terminate without consuming budget. The `RefCell` borrow guard cannot
    /// serve this role, since it permits concurrent immutable borrows.
    call_stack: Vec<CalleeKey>,
}

/// A cheap witness that a body is the one a per-function fact was recorded
/// for. Not an identity — two bodies can agree — but the mix-up those facts
/// have to survive is a scratch body belonging to another function, and that
/// differs. Cheap because the check runs wherever such a fact is read, which
/// is every projection through a `&GLOBAL` alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BodyShape {
    exprs: usize,
    stmts: usize,
    blocks: usize,
    root: BlockId,
}

impl BodyShape {
    fn of(body: &Body) -> Self {
        Self {
            exprs: body.exprs.len(),
            stmts: body.stmts.len(),
            blocks: body.blocks.len(),
            root: body.root,
        }
    }
}

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

/// Every expression id reachable from the body root, in arena order — or every
/// expression, for a bare-expression body with no block structure.
pub(crate) fn reachable_exprs(body: &Body) -> Vec<ExprId> {
    struct Collect(Vec<ExprId>);
    impl NirRefVisitor for Collect {
        fn visit_node(&mut self, body: &Body, node: NodeRef) {
            if let NodeRef::Expr(e) = node {
                self.0.push(e);
            }
            self.walk_node(body, node);
        }
    }
    if body.blocks.is_empty() {
        return body.exprs.iter().map(|(e, _)| e).collect();
    }
    let mut collect = Collect(Vec::new());
    collect.visit_node(body, NodeRef::Block(body.root));
    collect.0
}

impl<'a> Interpreter<'a> {
    #[must_use]
    pub fn new(type_table: &'a TypeTable) -> Self {
        Self {
            type_table,
            frame: FrameState::default(),
            callees: None,
            ctfe_builtins: None,
            globals: None,
            global_fields: None,
            step_budget: DEFAULT_STEP_BUDGET,
            call_stack: Vec::new(),
        }
    }

    /// Install the [`CalleeMap`]. Without it, every `Call` node remains
    /// [`Lattice::Unevaluated`] — the engine has no body to look up.
    pub fn with_callees(&mut self, callees: &'a CalleeMap) -> &mut Self {
        self.callees = Some(callees);
        self
    }

    /// Install the sequence-builtin lookup. Without it, an element or length
    /// read stays opaque.
    pub fn with_ctfe_builtins(&mut self, ctfe_builtins: &'a CtfeBuiltinMap) -> &mut Self {
        self.ctfe_builtins = Some(ctfe_builtins);
        self
    }

    /// Install the [`GlobalEnv`]. Without it, every `GlobalVarGet` node remains
    /// [`Lattice::Unevaluated`] — the engine has no initializer lattice to look
    /// up.
    pub fn with_globals(&mut self, globals: &'a GlobalEnv) -> &mut Self {
        self.globals = Some(globals);
        self
    }

    /// Install the [`GlobalFieldEnv`]. Without this,
    /// `FieldAccess(GlobalVarGet(_), _)` stays [`Lattice::Unevaluated`].
    /// Mirrors [`with_globals`](Self::with_globals).
    pub fn with_global_fields(&mut self, global_fields: &'a GlobalFieldEnv) -> &mut Self {
        self.global_fields = Some(global_fields);
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
        self.global_fields
            .and_then(|m| m.get(key))
            .and_then(|m| m.get(field_name))
            .cloned()
            .map_or(Lattice::Unevaluated, Lattice::Const)
    }

    /// Record which of `body`'s locals may bind an aggregate constant. The
    /// driving visitor calls this once per function, next to
    /// [`Self::record_ref_global_aliases`].
    pub fn record_aggregate_locals(&mut self, body: &Body) {
        self.frame.aggregate_locals =
            Trackability::outside_frame(body, self.ctfe_builtins, self.callees).aggregate_locals;
    }

    pub fn record_ref_global_aliases(&mut self, body: &Body) {
        self.frame.ref_global_aliases.clear();
        self.frame.alias_body = Some(BodyShape::of(body));
        let mut seen: IndexSet<u32> = IndexSet::default();
        for (_, st) in &body.stmts {
            if let Some((local, key)) = let_ref_global(body, &st.kind) {
                if seen.insert(local) {
                    self.frame.ref_global_aliases.insert(local, key);
                } else {
                    self.frame.ref_global_aliases.swap_remove(&local);
                }
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
        debug_assert!(
            self.call_stack.is_empty(),
            "niri call_stack leaked across function boundary",
        );
    }

    /// Record a lattice value for a `let`-bound local: [`Lattice::Const`] for an
    /// immutable binding whose RHS reduced, [`Lattice::NonConst`] for `let mut`
    /// or an RHS that did not.
    ///
    /// An aggregate constant is only recorded for a local
    /// [`Self::record_aggregate_locals`] proved unreachable through any other
    /// handle; otherwise it degrades to [`Lattice::NonConst`].
    ///
    /// A binding displaces a place alias the index may have held: reads must
    /// see the binding, not project through the stale alias. The alias is not
    /// restored when a scope ends — a read that then finds nothing abandons a
    /// fold, which is the sound direction.
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
}

/// The locals a matched pattern binds, paired with the scrutinee sub-values
/// they take.
pub type PatBindings = Vec<(u32, Value)>;

/// The environment entries [`Interpreter::enter_arm`] displaced, restored by
/// [`Interpreter::leave_arm`].
pub struct ArmScope(Vec<(u32, Option<Lattice>)>);
