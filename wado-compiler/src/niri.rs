//! NIR Interpreter (niri).
//!
//! Compile-time partial evaluator over the arena `Body`: it reduces what it can
//! and leaves a residual otherwise. Constant folding is the primary consumer;
//! branch pruning, constant propagation and compile-time function evaluation
//! reuse the same engine.
//!
//! Reduction is monotone — an expression only moves toward literal form, never
//! back — and idempotent. A literal leaf is left as written, so the source repr
//! (`0xFF`) survives a no-op pass.
//!
//! Each module below answers one question:
//!
//! - `lattice` — what an expression denotes.
//! - `frame` — what running a body does.
//! - `rewrite` — what becomes of an expression once its value is known.
//! - `trackability` — which locals a walk may hold a value for.
//! - `pattern` — whether a pattern matches a value.
//! - `place` — what a borrow or lvalue chain names.
//!
//! What the engine can evaluate is the WEP's to state, and it is maintained
//! there rather than here: `docs/wep-2026-04-27-nir-interpreter.md`.

use std::cell::RefCell;
use std::rc::Rc;

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

/// Identity of a callee in the [`CalleeMap`].
pub type CalleeKey = crate::nir::FuncId;

/// The callees a compile-time frame may run.
///
/// Membership answers whether a frame may *run* the callee at all —
/// [`is_ctfe_runnable`], decided once at construction and never re-checked.
/// Whether the call's value may be substituted for it is a different question,
/// answered per call: a unit callee denotes nothing, and one writing through a
/// `&mut` parameter runs only at statement position, where the executor applies
/// the write-backs. Arity, argument reduction and body shape are likewise
/// checked at fold time.
pub type CalleeMap = IndexMap<CalleeKey, Callee>;

/// A callee the engine may run, with the parameter facts the trackability
/// analysis needs answered without a borrow. Asking the function later answers
/// only when nobody holds `borrow_mut` on it, and a fold must not turn on
/// which function the visitor happens to be walking.
pub struct Callee {
    pub func: Rc<RefCell<NirFunction>>,
    pub mut_params: Vec<bool>,
    pub stored_params: Vec<bool>,
}

impl Callee {
    #[must_use]
    pub fn new(func: Rc<RefCell<NirFunction>>) -> Self {
        let (mut_params, stored_params) = {
            let borrowed = func.borrow();
            (
                borrowed.params.iter().map(|p| p.is_mut_ref).collect(),
                borrowed
                    .params
                    .iter()
                    .map(|p| borrowed.stores.contains(&p.name))
                    .collect(),
            )
        };
        Self {
            func,
            mut_params,
            stored_params,
        }
    }

    fn arity(&self) -> usize {
        self.mut_params.len()
    }

    /// A `&mut T` borrow is the only parameter kind that reaches the caller's
    /// storage. An index the signature does not have answers as one that does,
    /// so a call the map cannot account for is exempt from nothing.
    fn writes_param(&self, index: usize) -> bool {
        self.mut_params.get(index).copied().unwrap_or(true)
    }

    /// A stored parameter outlives the call, so naming its referent is not the
    /// passing read the other arguments are.
    fn reads_only(&self, index: usize) -> bool {
        self.stored_params.get(index).is_some_and(|stored| !stored)
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
}

impl CtfeBuiltin {
    /// Whether the builtin writes its first argument. A write is performed at
    /// statement position rather than folded, and only by a frame that owns
    /// the place it lands in.
    fn is_write(self) -> bool {
        match self {
            Self::ArraySet | Self::ArrayCopy => true,
            Self::ArrayGet | Self::ArrayLen | Self::ArrayNew | Self::ColdPath | Self::Select => {
                false
            }
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

mod frame;
mod lattice;
mod pattern;
mod place;
mod rewrite;
mod trackability;

use pattern::PatternMatch;
use trackability::{Reached, aggregate_safe_locals};

/// Commit sink for niri's body rewrites. The rewrite logic reads through
/// [`EditSink::body`] and commits every edit through the sink, so two backends
/// can share it: [`BodySink`] mutates a `Body` in place — used for throwaway
/// CTFE scratch bodies, where coherence with an engine's parent map / use
/// index is moot — while the optimize layer's `EngineSink` routes every edit
/// through `Engine::*` so the real body's maps stay coherent.
pub(crate) trait EditSink {
    fn body(&self) -> &Body;
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
/// `inline_hint` is not consulted: `#[inline(never)]` asks the optimizer to keep
/// the body out of line, which says nothing about compile-time knowability.
///
/// Nor is `stores`. What a callee keeps is a snapshot, sound exactly while
/// nothing can write the referent afterwards; `Reached` is what holds that.
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
/// Strictly stronger than [`is_ctfe_runnable`], which is what the
/// [`CalleeMap`] gates on: a frame runs a unit callee for the writes it
/// performs, so requiring a value here would refuse work the frame does. This
/// is the question a caller asks when it wants to hold the result — hoisting
/// a pure call's result to a global, where nothing remains to write through.
#[must_use]
pub fn is_ctfe_eligible(func: &NirFunction) -> bool {
    func.return_type != crate::tir::TypeTable::UNIT
        && func.stores.is_empty()
        && is_ctfe_runnable(func)
}

/// Everything the engine knows keyed by local index, which is per-function, so
/// entering a body means replacing the whole group and leaving one means
/// putting it back. Grouped rather than swapped field by field: a compile-time
/// frame exchanges all of it at once, and a member restored out of step with
/// its siblings would let one body read another's locals.
#[derive(Default)]
struct FrameState {
    /// Lattice values for the `let`-bound locals of the body being walked. An
    /// absent local reads as [`Lattice::Unevaluated`].
    env: IndexMap<u32, Lattice>,
    ref_global_aliases: IndexMap<u32, GlobalKey>,
    /// The body's [`aggregate_safe_locals`] — the only locals that may bind an
    /// aggregate constant. An unpopulated set refuses every aggregate binding.
    aggregate_locals: LocalSet,
    /// Locals a compile-time frame cannot track — see [`clobbered_locals`].
    /// Empty outside a frame.
    ctfe_clobbered: LocalSet,
    /// CTFE scratch-body fold memo, read back by
    /// [`Interpreter::expr_to_lattice`]. The scratch [`BodySink`] promotes
    /// nothing, so a fold has nowhere else to be recorded. Empty during
    /// real-body folding, where rewrites promote through the engine.
    scratch_folds: IndexMap<ExprId, Value>,
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

fn local_binds_to_global_ref(body: &Body, local: u32, key: &GlobalKey) -> bool {
    body.stmts
        .iter()
        .any(|(_, st)| let_ref_global(body, &st.kind) == Some((local, key.clone())))
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

    /// Record `body`'s `aggregate_safe_locals`. The driving visitor calls
    /// this once per function, next to [`Self::record_ref_global_aliases`].
    pub fn record_aggregate_locals(&mut self, body: &Body) {
        let writes = Reached::outside_frame(body, self.ctfe_builtins, self.callees);
        self.frame.aggregate_locals = aggregate_safe_locals(body, &writes);
    }

    pub fn record_ref_global_aliases(&mut self, body: &Body) {
        self.frame.ref_global_aliases.clear();
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

    /// Reset the per-function state. The driving visitor must call this before
    /// walking each function body: local indices are unique per function, not
    /// project-wide, so a previous function's bindings would otherwise read as
    /// this one's.
    ///
    /// The step budget resets here too, so one function with a long
    /// compile-time loop cannot decide whether the next one folds.
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
    pub fn bind_local(&mut self, index: u32, lattice: Lattice) {
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
