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

use crate::const_eval::{Value, is_int_prim, is_signed_int, prim_of};
use crate::hashmap::IndexMap;
use crate::hashmap::IndexSet;
use crate::module_source::ModuleSource;
use crate::nir::{NirBinaryOp, NirFunction, NirUnaryOp};
use crate::nir_arena::{
    ArmData, BlockId, BlockNode, Body, ExprId, ExprKind, ExprNode, NodeRef, Operand, PatId,
    PatKind, StmtId, StmtKind, StmtNode,
};
use crate::nir_value_graph::ValueKind;
use crate::nir_visitor::NirRefVisitor;
use crate::tir::{PrimitiveType, TypeId, TypeTable};

/// Three-state lattice over compile-time evaluation results.
///
/// Ordering: `Unevaluated` ⊑ `Const(v)` ⊑ `NonConst`. Equivalent to the
/// classical SCCP lattice (Wegman & Zadeck, 1991): `Unevaluated` ↔
/// `Bottom`, `NonConst` ↔ `Top`. Names favour readability over the
/// academic terms — readers familiar with the abstract-interpretation
/// literature can mentally substitute Bottom/Top.
///
/// Why three states: `Option<Value>` (the previous design) collapsed
/// "I haven't computed this yet" and "I know this isn't a constant"
/// into the same `None`, which makes memoization unsound — a cached
/// `None` can't say whether a re-attempt would succeed. The lattice
/// fixes this at the type level.
#[derive(Debug, Clone, PartialEq)]
pub enum Lattice {
    /// No information yet. Default for un-bound locals and NIR kinds
    /// the engine doesn't currently understand (e.g. a `Call` whose
    /// callee isn't pure-foldable, `Block` past a single tail
    /// expression).
    Unevaluated,
    /// Provably reduces to this value.
    Const(Value),
    /// Cannot be a reusable constant: a `let mut` binding, the result
    /// of a runtime-only operation (e.g. `x = …`, division by zero),
    /// or a fold whose operands are themselves `NonConst`.
    NonConst,
}

impl Lattice {
    /// Project to `Some(v)` only when the result is `Const`. The right
    /// shorthand for callers whose only question is "do you have a
    /// literal for me?" — the `Unevaluated` / `NonConst` distinction
    /// is collapsed into `None`. When that distinction matters
    /// (memoization, SCCP-style joins), pattern-match the variant
    /// directly instead of going through this projection.
    #[must_use]
    pub fn as_const(&self) -> Option<Value> {
        match self {
            Self::Const(v) => Some(v.clone()),
            Self::Unevaluated | Self::NonConst => None,
        }
    }

    /// Join two lattice values. This is the SCCP join (least upper bound)
    /// over the chain `Unevaluated ⊑ Const(v) ⊑ NonConst`:
    ///
    /// - `Unevaluated ⊔ x = x` — an `Unevaluated` arm is treated as
    ///   contributing no information (e.g. an `if true { … }` whose
    ///   `else` is unreachable / absent: the join with the executed arm
    ///   carries the executed arm's value out).
    /// - `Const(v) ⊔ Const(v) = Const(v)` — both arms agree, the
    ///   surrounding expression has that value regardless of which arm
    ///   ran. This is what lets `if cond { 5 } else { 5 }` collapse to
    ///   `5` when the condition is effect-free.
    /// - `Const(a) ⊔ Const(b) = NonConst` (when `a ≠ b`) — arms
    ///   disagree, the merged value is non-constant.
    /// - `NonConst ⊔ _ = NonConst` — top is absorbing.
    ///
    /// The operation is commutative, associative, and idempotent, as
    /// required of a lattice join. Used by [`Interpreter::reduce_local`]
    /// to merge the lattice of an `if` expression's two arms when the
    /// condition is non-constant.
    #[must_use]
    pub fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unevaluated, x) | (x, Self::Unevaluated) => x,
            (Self::NonConst, _) | (_, Self::NonConst) => Self::NonConst,
            (Self::Const(a), Self::Const(b)) if a == b => Self::Const(a),
            (Self::Const(_), Self::Const(_)) => Self::NonConst,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Callee map
// ──────────────────────────────────────────────────────────────────────────────

/// Identity of a callee in the [`CalleeMap`]. Mirrors the shape produced
/// by `FunctionRef::full_name` so the interpreter can look up a `Call`
/// node's target without re-deriving the format.
pub type CalleeKey = crate::nir::FuncId;

/// Map of CTFE-eligible callees, keyed by canonical [`crate::nir::FuncId`].
///
/// Values are [`Rc<RefCell<NirFunction>>`] handles aliased with
/// [`crate::flat_package::FlatPackage::functions`], not body clones, so
/// rebuilding the map every optimizer iteration costs only refcount
/// bumps. The interpreter reads each callee via
/// [`std::cell::RefCell::try_borrow`]; the failure path catches the
/// case where the visitor is currently holding `borrow_mut` on this
/// same function (i.e. self-recursive calls inside the function being
/// walked). CTFE-internal recursion across nested folds is handled
/// separately by `Interpreter::call_stack`, since `try_borrow` permits
/// concurrent immutable borrows.
///
/// The purity / CTFE-safety gate is decided once at map construction
/// time by [`is_ctfe_eligible`], and the interpreter never re-checks
/// it. Body-shape and per-call validity (arity match, all args
/// reduce, single recognized tail expression) are checked at fold
/// time, not here.
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

    fn writes_receiver(&self) -> bool {
        self.writes_param(0)
    }

    fn arity(&self) -> usize {
        self.mut_params.len()
    }

    /// A `&mut T` borrow is the only parameter kind that reaches the caller's
    /// storage. An index the signature does not have answers as one that does:
    /// nothing about a call the map cannot account for is exempt.
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

// ──────────────────────────────────────────────────────────────────────────────
// Global env
// ──────────────────────────────────────────────────────────────────────────────

/// Identity of a global variable in the [`GlobalEnv`]. Mirrors the
/// `(module_source, name)` shape carried by `ExprKind::GlobalVarGet`
/// so the interpreter can look up a `GlobalVarGet` node directly.
pub type GlobalKey = (ModuleSource, String);

/// Lattice values for module-scope globals.
///
/// Populated once per pass by the driving visitor from
/// [`crate::flat_package::FlatPackage::globals`] — typically by
/// reducing each non-`mut` global's initializer through a fresh
/// [`Interpreter`] (so initializers like `1 + 2`, `i32::MAX - 1`, or
/// pure-call expressions all collapse to `Const(_)`). Mutable globals
/// are mapped to [`Lattice::NonConst`] so reads through niri stay
/// conservative even while the global is in scope.
///
/// The map is read at every `GlobalVarGet` lookup; absent keys default
/// to [`Lattice::Unevaluated`] (the engine simply doesn't know — same
/// rule as un-bound locals).
pub type GlobalEnv = IndexMap<GlobalKey, Lattice>;

/// Known constant field values of module-scope globals, keyed by global then
/// field name. It lets
/// `FieldAccess(GlobalVarGet(X), f)` fold to a constant when `X` is an
/// immutable global whose `f` field is statically known — e.g. the
/// [`SeqField::Len`](crate::compiler_item::SeqField) length of an immutable
/// sequence global hoisted by body globalization.
pub type GlobalFieldEnv = IndexMap<GlobalKey, IndexMap<String, Value>>;

/// Ceiling on total CTFE work, charged per call entry, statement, and loop
/// iteration. Reset per function so what one function spends cannot decide
/// whether the next one folds.
pub const DEFAULT_STEP_BUDGET: u32 = 10_000;

// ──────────────────────────────────────────────────────────────────────────────
// Field knowledge
// ──────────────────────────────────────────────────────────────────────────────

mod frame;
mod lattice;
mod pattern;
mod place;
mod rewrite;
mod trackability;

use trackability::{Reached, aggregate_safe_locals};

/// A dense set of local indices, backed by a bitset indexed by the local
/// index itself.
///
/// Local indices within a function body are dense (`0..locals.len()`), so
/// this replaces an `IndexSet<u32>` used purely for membership with a
/// hash-free bitset — the same idea as [`crate::tir::TypeSet`]. The alias
/// analysis rebuilds these sets for every function on every const-fold
/// iteration, so dropping the per-grow allocation + hashing of an
/// `IndexSet` is worthwhile.
#[derive(Default, Clone, Debug)]
pub struct LocalSet {
    words: Vec<u64>,
}

impl LocalSet {
    /// An empty set pre-sized to hold `locals` indices without regrowing.
    #[must_use]
    pub fn with_capacity(locals: usize) -> Self {
        Self {
            words: vec![0; locals.div_ceil(64)],
        }
    }

    fn slot(index: u32) -> (usize, u64) {
        ((index / 64) as usize, 1u64 << (index % 64))
    }

    /// Insert `index`, returning `true` if it was not already present.
    pub fn insert(&mut self, index: u32) -> bool {
        let (word, mask) = Self::slot(index);
        if word >= self.words.len() {
            self.words.resize(word + 1, 0);
        }
        let newly = self.words[word] & mask == 0;
        self.words[word] |= mask;
        newly
    }

    /// Whether `index` is a member.
    #[must_use]
    pub fn contains(&self, index: u32) -> bool {
        let (word, mask) = Self::slot(index);
        self.words.get(word).is_some_and(|w| w & mask != 0)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }

    /// Iterate members in ascending index order.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.words.iter().enumerate().flat_map(|(wi, &word)| {
            (0..64u32)
                .filter(move |&b| word & (1u64 << b) != 0)
                .map(move |b| wi as u32 * 64 + b)
        })
    }
}

/// Per-function alias / aliasing-trackability annotations.
///
/// Computed once per function by [`crate::optimize::alias::build_alias_info`]
/// (from the function's stable `address_taken_locals` /
/// `stores_aliased_locals` plus a body walk that catches transient inlined-in
/// copies) and consumed by the engine [`ValueGraph`] builder
/// ([`crate::optimize::alias::builder_alias_sets`]) to bound heap-write
/// invalidation at the right granularity.
///
/// - `aliased`: locals reachable through some other handle (`&x`,
///   `&mut x`, captured by a closure, struct-field-stored, etc.).
/// - `untrackable`: locals whose aliasing escapes the analysis (e.g.
///   stashed across a `stores`-annotated callee).
/// - `alias_groups`: union-find groups of locals connected by
///   reference-typed `let dst = src` copies (`Box<T>`, `List<T>`,
///   `&T`, `&mut T`).
///
/// [`ValueGraph`]: crate::nir_value_graph
#[derive(Default, Clone, Debug)]
pub struct AliasInfo {
    pub aliased: LocalSet,
    pub untrackable: LocalSet,
    pub alias_groups: IndexMap<u32, IndexSet<u32>>,
}

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
    /// Promote `e` to the folded pure scalar `value`, returning whether the edit
    /// was applied (WEP: The Live `ValueGraph`). The engine backend swaps `e`'s
    /// parent operand to an `Operand::Value`, and declines an aggregate value —
    /// the pool models scalars only, so a constant struct stays in skeleton form
    /// and only the scalars projected out of it are promoted. The scratch CTFE
    /// backend is a no-op (`false`) — its reads recompute the value through the
    /// value lattice, so it needs no node write-back.
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
        // Scratch CTFE has no parent map to promote a constant into and pure
        // scalars no longer have a literal-node form; the value is recomputed on
        // read (`reduce_to_lattice_a` → `try_fold_a` over operands + env), so the
        // write-back is a no-op here.
        false
    }
    fn const_operand(&mut self, kind: ValueKind, type_id: TypeId) -> Operand {
        Operand::Value(self.body.values.alloc_unshared(kind, type_id))
    }
    fn become_expr(&mut self, dst: ExprId, src: ExprId) {
        // Clone src's whole node into dst (the original short-circuit rewrite
        // did `body.exprs[e] = body.exprs[keep].clone()`); the scratch body is
        // discarded, so the shared child references and the still-live `src`
        // node are harmless.
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
/// `inline_hint` is deliberately not consulted: `#[inline(never)]` asks the
/// optimizer to keep the body out of line, which says nothing about whether
/// the result is knowable at compile time.
///
/// Nor is `stores`. The engine has no reference values — an argument reduces to
/// its referent's value — so what the callee keeps is a snapshot, sound exactly
/// while nothing can write the referent afterwards. [`Reached`] is what
/// holds that: a stored argument is the one read it does not exempt.
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
/// and producing a value at all.
#[must_use]
pub fn is_ctfe_eligible(func: &NirFunction) -> bool {
    // A unit call has no value to substitute for it.
    func.return_type != crate::tir::TypeTable::UNIT
        && func.stores.is_empty()
        && is_ctfe_runnable(func)
}

// ──────────────────────────────────────────────────────────────────────────────
// Interpreter
// ──────────────────────────────────────────────────────────────────────────────

/// Partial evaluator over the arena `Body`.
///
/// Holds the type table needed to resolve operand widths, a per-function
/// `env` mapping local indices to lattice values, an optional
/// [`CalleeMap`] of pure-eligible callees, a step budget, and a
/// `call_stack` of in-flight CTFE frames for recursion detection.
pub struct Interpreter<'a> {
    type_table: &'a TypeTable,
    /// Lattice values for `let`-bound locals in the *current function*.
    /// Populated by the driving visitor via [`bind_local`] /
    /// [`invalidate_local`]; cleared via [`enter_function`]. Reads of
    /// `ExprKind::Local` consult this map during folding.
    ///
    /// Locals not present in the map default to [`Lattice::Unevaluated`].
    ///
    /// [`bind_local`]: Self::bind_local
    /// [`invalidate_local`]: Self::invalidate_local
    /// [`enter_function`]: Self::enter_function
    env: IndexMap<u32, Lattice>,
    ref_global_aliases: IndexMap<u32, GlobalKey>,
    /// The current function's [`aggregate_safe_locals`] — the only locals that
    /// may bind an aggregate constant. Cleared by [`Self::enter_function`], so
    /// an unpopulated set simply refuses every aggregate binding.
    aggregate_locals: LocalSet,
    /// Locals of the CTFE frame currently executing that another handle may
    /// write — see [`clobbered_locals`]. Empty outside a frame.
    ctfe_clobbered: LocalSet,
    /// CTFE scratch-body fold memo: `expr → folded constant`. The scratch
    /// [`BodySink`] cannot promote a fold to an `Operand::Value` (no parent map)
    /// and pure scalars have no literal-node form, so a fold is recorded here and
    /// read back by [`Self::expr_to_lattice_a`]. Scoped to one `try_call_fold_a`
    /// call (saved/cleared around the scratch reduction); empty during real-body
    /// folding, where rewrites promote through the engine instead.
    scratch_folds: IndexMap<ExprId, Value>,
    /// Pre-built map of CTFE-eligible callees. When `None`, `Call` nodes
    /// stay [`Lattice::Unevaluated`]. The visitor populates this once
    /// per pass via [`with_callees`].
    ///
    /// [`with_callees`]: Self::with_callees
    callees: Option<&'a CalleeMap>,
    /// When `None`, an `array_get` / `array_len` call is just an opaque call.
    ctfe_builtins: Option<&'a CtfeBuiltinMap>,
    /// Pre-built lattice values for module-scope globals. When `None`,
    /// every `GlobalVarGet` stays [`Lattice::Unevaluated`]. The visitor
    /// populates this once per pass via [`with_globals`].
    ///
    /// [`with_globals`]: Self::with_globals
    globals: Option<&'a GlobalEnv>,
    /// Pre-built constant field values of module-scope globals. When `None`,
    /// `FieldAccess(GlobalVarGet(_), _)` stays [`Lattice::Unevaluated`]. The
    /// visitor populates this once per pass via [`with_global_fields`].
    ///
    /// [`with_global_fields`]: Self::with_global_fields
    global_fields: Option<&'a GlobalFieldEnv>,
    /// Hard ceiling on the number of productive CTFE call entries
    /// before bailing. Decremented once per successful body evaluation;
    /// on zero, further attempts return `Unevaluated`.
    step_budget: u32,
    /// Keys of the callees whose bodies the engine is currently
    /// evaluating, in entry order. A `Call` to a key already on the
    /// stack reports `Unevaluated` immediately, so direct (`f → f`)
    /// and indirect (`f → g → f`) recursion terminate without
    /// consuming budget. The `RefCell` borrow guard cannot serve this
    /// role on its own because `try_borrow` permits concurrent
    /// immutable borrows.
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

/// Every expression id reachable from the body root, in arena order.
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
    // A body with no block structure is a bare expression, nothing orphaned.
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
            env: IndexMap::default(),
            ref_global_aliases: IndexMap::default(),
            aggregate_locals: LocalSet::default(),
            ctfe_clobbered: LocalSet::default(),
            scratch_folds: IndexMap::default(),
            callees: None,
            ctfe_builtins: None,
            globals: None,
            global_fields: None,
            step_budget: DEFAULT_STEP_BUDGET,
            call_stack: Vec::new(),
        }
    }

    /// Install the [`CalleeMap`]. Without this, every `Call` node
    /// remains [`Lattice::Unevaluated`] — the engine has no body to
    /// look up.
    ///
    /// Lifetime: the map outlives this interpreter (the visitor builds
    /// it once per pass, hands a borrow in, and discards both at end of
    /// pass).
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

    /// Install the [`GlobalEnv`]. Without this, every `GlobalVarGet`
    /// node remains [`Lattice::Unevaluated`] — the engine has no
    /// initializer lattice to look up.
    ///
    /// Lifetime: the map outlives this interpreter (the visitor builds
    /// it once per pass, hands a borrow in, and discards both at end of
    /// pass), mirroring [`with_callees`].
    ///
    /// [`with_callees`]: Self::with_callees
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
    /// [`DEFAULT_STEP_BUDGET`]). Called rarely — primarily by tests
    /// exercising the budget-exhaustion path.
    pub fn set_step_budget(&mut self, budget: u32) -> &mut Self {
        self.step_budget = budget;
        self
    }

    fn global_field(&self, key: &GlobalKey, field_name: &str) -> Lattice {
        self.global_fields
            .and_then(|m| m.get(key))
            .and_then(|m| m.get(field_name))
            .cloned()
            .map_or(Lattice::Unevaluated, Lattice::Const)
    }

    /// Record `body`'s [`aggregate_safe_locals`]. The driving visitor calls
    /// this once per function, next to [`Self::record_ref_global_aliases`].
    pub fn record_aggregate_locals(&mut self, body: &Body) {
        let writes = Reached::outside_frame(body, self.ctfe_builtins, self.callees);
        self.aggregate_locals = aggregate_safe_locals(body, &writes);
    }

    pub fn record_ref_global_aliases(&mut self, body: &Body) {
        self.ref_global_aliases.clear();
        let mut seen: IndexSet<u32> = IndexSet::default();
        for (_, st) in &body.stmts {
            if let Some((local, key)) = let_ref_global(body, &st.kind) {
                if seen.insert(local) {
                    self.ref_global_aliases.insert(local, key);
                } else {
                    self.ref_global_aliases.swap_remove(&local);
                }
            }
        }
    }

    /// Reset the per-function environment. The driving visitor must call
    /// this before walking each function body; otherwise a previous
    /// function's bindings would leak into the next one (local indices
    /// are unique per function, not project-wide).
    ///
    /// Asserts the recursion guard is clear — a leaked entry would mean
    /// a previous walk panicked mid-call.
    ///
    /// The step budget resets here, so one function with a long compile-time
    /// loop cannot decide whether the functions walked after it fold.
    pub fn enter_function(&mut self) {
        self.step_budget = DEFAULT_STEP_BUDGET;
        self.env.clear();
        self.ref_global_aliases.clear();
        self.aggregate_locals = LocalSet::default();
        self.ctfe_clobbered = LocalSet::default();
        self.scratch_folds.clear();
        debug_assert!(
            self.call_stack.is_empty(),
            "niri call_stack leaked across function boundary",
        );
    }

    /// Record a lattice value for a `let`-bound local. The driving
    /// visitor calls this after walking a `Let` statement: pass
    /// [`Lattice::Const`] for an immutable binding whose RHS reduced,
    /// [`Lattice::NonConst`] for `let mut` or any RHS that could not be
    /// reduced.
    ///
    /// An aggregate constant is only recorded for a local
    /// [`Self::record_aggregate_locals`] proved unreachable through any other
    /// handle; otherwise it degrades to [`Lattice::NonConst`].
    pub fn bind_local(&mut self, index: u32, lattice: Lattice) {
        let unbacked_aggregate = matches!(&lattice, Lattice::Const(v) if !v.is_scalar())
            && !self.aggregate_locals.contains(index);
        let lattice = if unbacked_aggregate {
            Lattice::NonConst
        } else {
            lattice
        };
        self.env.insert(index, lattice);
    }

    /// The locals a match arm's pattern binds, with the values they take under
    /// a constant `scrutinee`. Empty unless the pattern definitely matches — an
    /// undecided pattern binds nothing knowable. The walker installs these
    /// ([`Self::enter_arm`]) around the arm's guard and body so both reduce
    /// under the values the arm would see at runtime.
    #[must_use]
    pub fn arm_bindings(&self, body: &Body, scrutinee: Operand, pattern: PatId) -> PatBindings {
        let Lattice::Const(value) = self.operand_to_lattice_a(body, scrutinee) else {
            return PatBindings::new();
        };
        let mut binds = PatBindings::new();
        match self.pattern_matches_a(body, &value, pattern, &mut binds) {
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
                .map(|(index, _)| (*index, self.env.get(index).cloned()))
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
        let value = self.operand_to_lattice_a(body, guard).as_const();
        self.leave_arm(scope);
        value.and_then(|v| v.as_bool())
    }

    pub fn leave_arm(&mut self, scope: ArmScope) {
        for (index, previous) in scope.0 {
            match previous {
                Some(lattice) => self.env.insert(index, lattice),
                None => self.env.swap_remove(&index),
            };
        }
    }

    /// Mark a local as definitely non-constant from this point on. The
    /// driving visitor calls this when it sees an `x = expr` assignment.
    /// Conservative — we don't track flow-sensitive new values, just
    /// invalidate the prior binding.
    pub fn invalidate_local(&mut self, index: u32) {
        self.env.insert(index, Lattice::NonConst);
    }
}

/// Whether the subtree under `op` reads any of the locals `binds` binds.
fn operand_reads_any_local(body: &Body, op: Operand, binds: &PatBindings) -> bool {
    struct Reads<'a> {
        binds: &'a PatBindings,
        found: bool,
    }
    impl NirRefVisitor for Reads<'_> {
        fn visit_node(&mut self, body: &Body, node: NodeRef) {
            if let NodeRef::Expr(e) = node
                && let ExprKind::Local { index, .. } = &body.exprs[e].kind
                && self.binds.iter().any(|(bound, _)| bound == index)
            {
                self.found = true;
            }
            self.walk_node(body, node);
        }
    }
    let Some(expr) = op.as_expr() else {
        return false;
    };
    let mut visitor = Reads {
        binds,
        found: false,
    };
    visitor.visit_node(body, NodeRef::Expr(expr));
    visitor.found
}

/// `Some(v)` ↦ `Const(v)`, `None` ↦ `NonConst`. Used at the boundary
/// where a numeric-evaluation helper that still returns `Option<Value>`
/// (because its failure modes are runtime traps, not "haven't tried")
/// flows back into the lattice surface.
fn option_to_lattice(opt: Option<Value>) -> Lattice {
    match opt {
        Some(v) => Lattice::Const(v),
        None => Lattice::NonConst,
    }
}

/// Outcome of testing a pattern against a constant scrutinee
/// [`Value`]. The three states mirror the pattern's contribution to
/// SCCP feasibility in [`Interpreter::match_lattice`]:
///
/// - `Yes` — the pattern provably matches; later arms are infeasible
///   edges.
/// - `No` — the pattern provably does not match; this arm is an
///   infeasible edge.
/// - `Unknown` — the engine cannot decide (an unmodelled pattern
///   shape, a guard the engine doesn't analyze, a `ConstantValue`
///   whose inner expression doesn't reduce). The arm stays in play
///   and contributes to the join with all later arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatternMatch {
    Yes,
    No,
    Unknown,
}

/// The locals a matched pattern binds, paired with the scrutinee sub-values
/// they take.
pub type PatBindings = Vec<(u32, Value)>;

/// The environment entries [`Interpreter::enter_arm`] displaced, restored by
/// [`Interpreter::leave_arm`].
pub struct ArmScope(Vec<(u32, Option<Lattice>)>);

fn bool_to_match(b: bool) -> PatternMatch {
    if b {
        PatternMatch::Yes
    } else {
        PatternMatch::No
    }
}

/// Join a slice of lattice values via [`Lattice::join`]. Empty input
/// returns [`Lattice::Unevaluated`] (the join's identity).
fn join_all(lats: &[Lattice]) -> Lattice {
    let mut acc = Lattice::Unevaluated;
    for l in lats {
        acc = acc.join(l.clone());
    }
    acc
}

/// Compare an integer value (raw bits + prim) against a signed i128
/// pattern literal. Returns `true` iff the values are equal under
/// the prim's signedness interpretation.
fn int_value_matches_i128(value: u64, prim: PrimitiveType, pat: i128) -> bool {
    let Some(v) = int_value_as_i128(value, prim) else {
        return false;
    };
    v == pat
}

/// Compare an integer value (raw bits + prim) against an unsigned
/// u128 pattern literal.
fn int_value_matches_u128(value: u64, prim: PrimitiveType, pat: u128) -> bool {
    if is_signed_int(prim) {
        // Signed value cannot represent values outside i64 range
        // anyway; reinterpret as unsigned for comparison.
        let v = value as i64;
        if v < 0 {
            return false;
        }
        u128::from(v as u64) == pat
    } else {
        u128::from(value) == pat
    }
}

/// Convert a (raw bits, prim) integer into an i128, sign- or
/// zero-extending per the prim's signedness. Returns `None` for
/// non-integer prims.
fn int_value_as_i128(value: u64, prim: PrimitiveType) -> Option<i128> {
    if !is_int_prim(prim) {
        return None;
    }
    if is_signed_int(prim) {
        // Stored as sign-extended i64 → widen to i128.
        Some(i128::from(value as i64))
    } else {
        Some(i128::from(value))
    }
}

/// Decide whether a (raw bits, prim) integer falls inside a range
/// pattern. Returns `false` for non-integer prims and for negative
/// signed values against an unsigned-typed range (which by
/// construction starts at zero or higher); otherwise returns the
/// usual half-open / closed range membership test in i128 space.
fn range_matches_int(
    value: u64,
    prim: PrimitiveType,
    start: i128,
    end: i128,
    inclusive: bool,
    is_unsigned_pat: bool,
) -> bool {
    if !is_int_prim(prim) {
        return false;
    }
    let v: i128 = if is_unsigned_pat || !is_signed_int(prim) {
        // Treat the value as unsigned. For a signed prim with negative
        // bits, the unsigned reinterpretation differs — fall back to
        // sign-extended comparison, then ensure it stays nonneg before
        // entering an unsigned range check.
        if is_signed_int(prim) {
            let signed = i128::from(value as i64);
            if signed < 0 {
                // The pattern is unsigned; a negative scrutinee can't
                // be in `[start, end]` when start ≥ 0.
                return false;
            }
            signed
        } else {
            i128::from(value)
        }
    } else {
        i128::from(value as i64)
    };
    if inclusive {
        v >= start && v <= end
    } else {
        v >= start && v < end
    }
}

/// Adjust a block's raw lattice value before feeding it into an
/// arm-feasible-join (the `if` non-constant-condition path).
///
/// `Lattice::Unevaluated` from `block_lattice` means "we couldn't
/// analyze this block's value" — which is fine when the block is the
/// chosen branch of a constant-condition `if` (the other arm is an
/// infeasible edge, so the result really is "we don't know"), but
/// becomes unsound under a non-constant condition: that arm is
/// reachable, the absence of a known value means SCCP-Top
/// (`NonConst`), not infeasibility. Promote here so that a subsequent
/// `Lattice::join` cannot let an `Unevaluated` arm be silently
/// absorbed by a `Const` peer (`join(Unevaluated, Const(v)) → Const(v)`
/// is the infeasible-edge rule, valid only when the Unevaluated edge
/// really is unreachable).
fn arm_lattice_for_feasible_join(lat: Lattice) -> Lattice {
    match lat {
        Lattice::Unevaluated => Lattice::NonConst,
        other => other,
    }
}

/// Whether the arms cover every scrutinee (a guardless catch-all exists).
fn is_provably_exhaustive_a(body: &Body, arms: &[ArmData]) -> bool {
    arms.iter()
        .any(|a| a.guard.is_none() && pattern_is_catch_all_a(body, a.pattern))
}

fn pattern_is_catch_all_a(body: &Body, pat: PatId) -> bool {
    match &body.pats[pat].kind {
        PatKind::Wildcard | PatKind::Binding { .. } => true,
        PatKind::Or(alts) => alts.iter().any(|p| pattern_is_catch_all_a(body, *p)),
        _ => false,
    }
}

/// Simplify a short-circuit one operand already decides. The neutral element
/// keeps the other operand (`true && x` / `false || x` — and their mirrors —
/// become `x`); the absorbing element becomes the result (`false && x` /
/// `true || x` become `false` / `true`).
fn rewrite_short_circuit_via<S: EditSink>(sink: &mut S, e: ExprId) -> bool {
    if let Some(absorbing) = absorbing_short_circuit(sink.body(), e) {
        return sink.replace_with_value(e, Value::Bool(absorbing));
    }
    let body = sink.body();
    let keep: Operand = match &body.exprs[e].kind {
        ExprKind::Binary { left, op, right } => {
            let (left, op, right) = (*left, *op, *right);
            match (operand_bool(body, left), op, operand_bool(body, right)) {
                (Some(false), NirBinaryOp::Or, _) | (Some(true), NirBinaryOp::And, _) => right,
                (_, NirBinaryOp::Or, Some(false)) | (_, NirBinaryOp::And, Some(true)) => left,
                _ => return false,
            }
        }
        _ => return false,
    };
    // Become the kept operand. The other operand is left orphaned. A constant
    // `keep` (a fully-constant short-circuit) is left to the const-fold path.
    let Some(keep_e) = keep.as_expr() else {
        return false;
    };
    sink.become_expr(e, keep_e);
    true
}

/// The value a short-circuit collapses to when one operand is its absorbing
/// element — `true` for `||`, `false` for `&&`. `None` unless the *other*
/// operand is discardable: `x || true` still evaluates `x` first, so deleting
/// it is only sound when it can neither trap nor be observed.
fn absorbing_short_circuit(body: &Body, e: ExprId) -> Option<bool> {
    let ExprKind::Binary { left, op, right } = &body.exprs[e].kind else {
        return None;
    };
    let (left, op, right) = (*left, *op, *right);
    let absorbing = match op {
        NirBinaryOp::Or => true,
        NirBinaryOp::And => false,
        _ => return None,
    };
    let discarded = if operand_bool(body, left) == Some(absorbing) {
        right
    } else if operand_bool(body, right) == Some(absorbing) {
        left
    } else {
        return None;
    };
    is_discardable_operand_a(body, discarded).then_some(absorbing)
}

/// Whether `e` can be *deleted* outright: side-effect-free like
/// [`is_speculatable_a`], and trap-free on top of that.
///
/// The two differ where a trap is possible. `is_speculatable_a` admits
/// `FieldAccess` and `Cast`, which is right for its callers — they *reorder* an
/// expression, so a trap it would raise still happens. Deleting the expression
/// erases the trap, which the program is entitled to observe.
fn is_discardable_a(body: &Body, e: ExprId) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Local { .. } => true,
        ExprKind::Binary { left, op, right } => {
            !matches!(op, NirBinaryOp::Div | NirBinaryOp::Mod)
                && is_discardable_operand_a(body, *left)
                && is_discardable_operand_a(body, *right)
        }
        ExprKind::Unary { op, expr: inner } => {
            !matches!(op, NirUnaryOp::Deref) && is_discardable_operand_a(body, *inner)
        }
        _ => false,
    }
}

/// Operand form of [`is_discardable_a`]: a promoted pure value (a constant) is
/// always discardable.
fn is_discardable_operand_a(body: &Body, op: crate::nir_arena::Operand) -> bool {
    op.as_expr().is_none_or(|e| is_discardable_a(body, e))
}

/// The boolean value of an operand: a promoted `ValueKind::Bool` in the pool.
/// `None` for any other operand.
fn operand_bool(body: &Body, op: Operand) -> Option<bool> {
    match body.values.kind(op.as_value()?) {
        ValueKind::Bool(b) => Some(*b),
        _ => None,
    }
}

/// Recognize `match X { Case => true, _ => false }` as an equality test.
fn try_match_bool_discriminator_a(
    body: &Body,
    arms: &[(Option<Operand>, PatId, Operand, crate::token::Span)],
) -> Option<EnumEqReplacement> {
    let [yes_arm, no_arm] = arms else {
        return None;
    };
    if yes_arm.0.is_some() || no_arm.0.is_some() {
        return None;
    }
    if !matches!(body.pats[no_arm.1].kind, PatKind::Wildcard) {
        return None;
    }
    if operand_bool(body, yes_arm.2) != Some(true) {
        return None;
    }
    if operand_bool(body, no_arm.2) != Some(false) {
        return None;
    }
    let PatKind::Enum {
        enum_type,
        case_name,
        case_index,
    } = &body.pats[yes_arm.1].kind
    else {
        return None;
    };
    Some(EnumEqReplacement {
        enum_type: *enum_type,
        case_index: *case_index,
        case_name: case_name.clone(),
        span: yes_arm.3,
    })
}

/// Whether `e` can be evaluated out of order (side-effect-free, cannot trap).
fn is_speculatable_a(body: &Body, e: ExprId) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Local { .. } => true,
        ExprKind::Binary { left, op, right } => {
            !matches!(op, NirBinaryOp::Div | NirBinaryOp::Mod)
                && is_speculatable_operand_a(body, *left)
                && is_speculatable_operand_a(body, *right)
        }
        ExprKind::Unary { op, expr: inner } => {
            !matches!(op, NirUnaryOp::Deref) && is_speculatable_operand_a(body, *inner)
        }
        ExprKind::Cast { expr: inner, .. } => is_speculatable_operand_a(body, *inner),
        ExprKind::FieldAccess { expr: inner, .. } => is_speculatable_operand_a(body, *inner),
        _ => false,
    }
}

/// Operand form of [`is_speculatable_a`]: a promoted pure value (constant)
/// is always speculatable.
fn is_speculatable_operand_a(body: &Body, op: crate::nir_arena::Operand) -> bool {
    op.as_expr().is_none_or(|e| is_speculatable_a(body, e))
}

// ──────────────────────────────────────────────────────────────────────────────
// `match X { Pat => true, _ => false }` discriminator collapse
// ──────────────────────────────────────────────────────────────────────────────

/// The replacement shape produced by `try_match_bool_discriminator`. The
/// scrutinee box is plugged in by the caller once it has taken ownership
/// of the original `Match` expression.
///
/// Only the `EnumEq` shape exists today; `PatKind::Variant` is left
/// intact because synthesising the matching `VariantTest` requires a
/// variant→case-index lookup that the pattern itself doesn't carry
/// (the WIR builder resolves it via the variant decl's case list,
/// which the interpreter doesn't carry today). The fpfmt motivator
/// (`SpecialKind`) is an `enum`, so the Enum-only scope is sufficient
/// for this PR; expanding to `Variant` is a follow-up.
struct EnumEqReplacement {
    enum_type: TypeId,
    case_index: u32,
    case_name: String,
    span: crate::token::Span,
}

impl EnumEqReplacement {}
