//! NIR skeleton arena (Layer 1).
//!
//! A per-function arena representation of a NIR body: every node lives in a
//! `PrimaryMap` keyed by a typed id (`ExprId` / `StmtId` / `BlockId` / `PatId`)
//! and references its children by id. This is the substrate the worklist
//! rewrite engine needs: stable handles that survive in-place edits.
//!
//! This module owns the representation, structural traversal
//! (`for_each_child`), and structural cloning (`clone_expr` / `clone_block`).
//! The parent map, the local use index, and the mutating edit API live on the
//! rewrite [`crate::nir_engine::Engine`] that consumes this arena, not on
//! `Body` itself (so they don't burden every body). `lower::translate` builds
//! the arena directly — there is no tree representation to convert from. See
//! `docs/wep-2026-06-05-nir-optimizer-architecture.md`.

use cranelift_entity::{PrimaryMap, entity_impl};

use crate::hashmap::IndexSet;
use crate::nir::{NirBinaryOp, NirLocal, NirUnaryOp};
use crate::nir_value_graph::{ValueId, ValueKind, ValuePool};
use crate::tir::TypeId;
use crate::token::Span;

/// An operand position in the skeleton — an expression's value, after operand
/// promotion. It is either a pure value living in the
/// function's [`ValuePool`] (literals, `Binary`, pure `Unary`, `Cast`, and the
/// `Local` / `FieldAccess` reads the graph resolves to a value), or an effectful
/// / control subtree kept in the skeleton (`Call`, allocation literals,
/// `If` / `Match` / `Block` value positions). Pure values no longer
/// occupy `ExprId` slots; the slot holds their `ValueId` directly.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Operand {
    /// A pure value in the function's [`ValuePool`].
    Value(ValueId),
    /// An effectful or control subtree kept in the skeleton.
    Expr(ExprId),
}

impl Operand {
    /// The skeleton subtree, if this operand is one. `None` for a promoted
    /// pure value (which has no `ExprId`).
    #[inline]
    pub fn as_expr(self) -> Option<ExprId> {
        match self {
            Operand::Expr(e) => Some(e),
            Operand::Value(_) => None,
        }
    }

    /// The promoted pure value, if this operand is one.
    #[inline]
    pub fn as_value(self) -> Option<ValueId> {
        match self {
            Operand::Value(v) => Some(v),
            Operand::Expr(_) => None,
        }
    }
}

impl From<ExprId> for Operand {
    #[inline]
    fn from(e: ExprId) -> Self {
        Operand::Expr(e)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExprId(u32);
entity_impl!(ExprId, "expr");

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StmtId(u32);
entity_impl!(StmtId, "stmt");

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(u32);
entity_impl!(BlockId, "block");

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PatId(u32);
entity_impl!(PatId, "pat");

/// A uniform handle to any arena node, used by the rewrite engine's worklist
/// and parent map. See `docs/wep-2026-06-05-nir-optimizer-architecture.md`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum NodeRef {
    Expr(ExprId),
    Stmt(StmtId),
    Block(BlockId),
    Pat(PatId),
}

/// A child slot of an arena node: an operand position — which may hold a
/// promoted value carrying no skeleton id — or a structural / non-operand id
/// child. [`Body::for_each_child`] and [`Body::for_each_operand`] are both
/// filters over this, so neither can miss a slot the other sees.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    Operand(Operand),
    Node(NodeRef),
}

/// An expression node.
#[derive(Debug, Clone)]
pub struct ExprNode {
    pub kind: ExprKind,
    pub type_id: TypeId,
    pub span: Span,
}

/// A statement node.
#[derive(Debug, Clone)]
pub struct StmtNode {
    pub kind: StmtKind,
    pub span: Span,
}

/// A block node.
#[derive(Debug, Clone)]
pub struct BlockNode {
    pub stmts: Vec<StmtId>,
    pub span: Span,
}

/// A pattern node.
#[derive(Debug, Clone)]
pub struct PatNode {
    pub kind: PatKind,
    pub span: Span,
}

/// A call argument with its parameter mutability flag.
#[derive(Debug, Clone)]
pub struct ArenaCallArg {
    pub expr: Operand,
    pub is_mut: bool,
}

/// A struct-literal field.
#[derive(Debug, Clone)]
pub struct ArenaStructField {
    pub name: String,
    pub value: Operand,
    pub field_index: u32,
}

/// A match arm.
#[derive(Debug, Clone)]
pub struct ArmData {
    pub pattern: PatId,
    pub guard: Option<Operand>,
    pub body: Operand,
    pub span: Span,
}

/// A struct destructuring field.
#[derive(Debug, Clone)]
pub struct ArenaStructPatternField {
    pub field_name: String,
    pub field_index: u32,
    pub pattern: PatId,
}

/// Expression kinds: leaf data is stored inline, children by id.
#[derive(Debug, Clone)]
pub enum ExprKind {
    /// Tombstone for an orphaned node: `become_expr` and the various
    /// move-out rewrites leave the vacated `ExprId` slot here. A `Dead` node
    /// has no parent and no children, so no analysis walk reaches it; it is
    /// reclaimed by DCE. (Distinct from the unit value, which is a pooled
    /// `ValueKind::Unit` operand.)
    Dead,
    PackedArray(Vec<u8>),
    Local {
        index: u32,
        name: String,
    },
    GlobalVarGet {
        module_source: crate::module_source::ModuleSource,
        name: String,
    },
    GlobalVarSet {
        module_source: crate::module_source::ModuleSource,
        name: String,
        value: Operand,
    },
    Binary {
        left: Operand,
        op: NirBinaryOp,
        right: Operand,
    },
    Unary {
        op: NirUnaryOp,
        expr: Operand,
    },
    Assign {
        target: ExprId,
        value: Operand,
    },
    Cast {
        expr: Operand,
        target_type: TypeId,
    },
    Call {
        /// The sole callee reference: the canonical [`FuncId`](crate::nir::FuncId),
        /// stamped at `lower` ("born resolved"), non-optional so a call is never
        /// transiently unresolved. The callee's name / module / monomorph / method
        /// identity lives only in the function record at this id (`store[id]`); the
        /// call node carries no `FunctionRef`.
        func_id: crate::nir::FuncId,
        type_args: Vec<TypeId>,
        /// Arguments in the callee's parameter order — a method's receiver is
        /// `args[0]`, so `args[i]` maps to `params[i]` for every call shape.
        args: Vec<ArenaCallArg>,
        /// Whether `args[0]` is the receiver of an instance method, carried down
        /// from [`crate::tir::TirExprKind::Call`] with its meaning intact.
        has_receiver: bool,
    },
    CmRawCall {
        local_name: String,
        args: Vec<Operand>,
    },
    FieldAccess {
        expr: Operand,
        field_index: u32,
        field_name: String,
    },
    Index {
        expr: Operand,
        index: Operand,
    },
    Block(BlockId),
    If {
        condition: Operand,
        then_branch: BlockId,
        else_branch: Option<BlockId>,
    },
    Match {
        expr: Operand,
        arms: Vec<ArmData>,
    },
    StructLiteral {
        struct_type: TypeId,
        struct_name: String,
        fields: Vec<ArenaStructField>,
    },
    TupleLiteral {
        elements: Vec<Operand>,
    },
    ArrayLiteral {
        elements: Vec<Operand>,
    },
    IndirectCall {
        callee: Operand,
        args: Vec<Operand>,
    },
    ClosureToCanonical {
        functor: Operand,
        functor_id: u32,
        target_fn_type: TypeId,
        closure_module: crate::module_source::ModuleSource,
    },
    VariantConstruct {
        variant_type: TypeId,
        case_index: u32,
        case_name: String,
        payload: Option<Operand>,
    },
    EnumConstruct {
        enum_type: TypeId,
        case_index: u32,
        case_name: String,
    },
    LabeledBlock {
        label: String,
        block: BlockId,
        result_type: TypeId,
    },
    VariantTag {
        expr: Operand,
    },
    VariantTest {
        expr: Operand,
        case_index: u32,
        case_name: String,
    },
    VariantPayload {
        expr: Operand,
        case_index: u32,
        payload_type: TypeId,
    },
    Switch {
        scrutinee: Operand,
        min_value: i64,
        arms: Vec<BlockId>,
        default: BlockId,
    },
}

impl ExprKind {
    /// Build `recv.m(args)`: the receiver heads the argument list, carrying the
    /// callee's `self` mutability as its own `is_mut`.
    pub fn method_call(
        func_id: crate::nir::FuncId,
        receiver: Operand,
        receiver_is_mut: bool,
        args: Vec<ArenaCallArg>,
    ) -> Self {
        let mut all = Vec::with_capacity(args.len() + 1);
        all.push(ArenaCallArg {
            expr: receiver,
            is_mut: receiver_is_mut,
        });
        all.extend(args);
        ExprKind::Call {
            func_id,
            type_args: Vec::new(),
            args: all,
            has_receiver: true,
        }
    }

    /// An instance-method call viewed as receiver plus the arguments after it.
    /// `None` for a free call.
    ///
    /// The split is a view, not storage: the node keeps one argument list in the
    /// callee's parameter order, so a pass that treats every argument alike
    /// (traversal, substitution, operand rewriting) matches `Call` directly and
    /// never needs this.
    pub fn as_method_call(&self) -> Option<(Operand, crate::nir::FuncId, &[ArenaCallArg])> {
        let ExprKind::Call {
            func_id,
            args,
            has_receiver: true,
            ..
        } = self
        else {
            return None;
        };
        let (receiver, rest) = args.split_first()?;
        Some((receiver.expr, *func_id, rest))
    }
}

/// Statement kinds.
#[derive(Debug, Clone)]
pub enum StmtKind {
    Let {
        name: String,
        local_index: u32,
        is_mut: bool,
        is_reactive: bool,
        type_id: TypeId,
        value: Operand,
        skip_value_copy: bool,
    },
    Expr(Operand),
    Return {
        value: Option<Operand>,
    },
    If {
        condition: Operand,
        then_block: BlockId,
        else_block: Option<BlockId>,
    },
    Loop {
        body: BlockId,
    },
    Break {
        label: Option<String>,
        value: Option<Operand>,
    },
    Continue,
    LabeledBlock {
        label: String,
        block: BlockId,
    },
    LetDestructure {
        pattern: PatId,
        is_mut: bool,
        value: Operand,
    },
}

/// Pattern kinds. Leaf payloads (`NirLiteralPattern`, range bounds) are
/// stored inline; nested patterns and the `ConstantValue` expression are ids.
#[derive(Debug, Clone)]
pub enum PatKind {
    Wildcard,
    Binding {
        name: String,
        local_index: u32,
        type_id: TypeId,
    },
    Literal(crate::nir::NirLiteralPattern),
    Tuple(Vec<PatId>, bool),
    Variant {
        enum_type: TypeId,
        variant_name: String,
        bindings: Vec<PatId>,
        payload_type: TypeId,
    },
    Enum {
        enum_type: TypeId,
        case_name: String,
        case_index: u32,
    },
    Struct {
        struct_type: TypeId,
        fields: Vec<ArenaStructPatternField>,
        has_rest: bool,
    },
    Or(Vec<PatId>),
    ConstantValue {
        expr: Operand,
    },
    Range {
        start: i128,
        end: i128,
        inclusive: bool,
        is_unsigned: bool,
    },
}

/// A dense set of the local indices a [`Body`] declares.
///
/// Local indices are dense within a body (`0..locals.len()`), so this is the
/// membership-only companion to `IndexSet<u32>`, without its allocation and
/// hashing. Worth having because the analyses rebuild one per function on
/// every optimizer iteration.
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

    /// Iterate members in ascending index order.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.words.iter().enumerate().flat_map(|(wi, &word)| {
            (0..64u32)
                .filter(move |&b| word & (1u64 << b) != 0)
                .map(move |b| wi as u32 * 64 + b)
        })
    }
}

/// A NIR body in arena form: one `PrimaryMap` per node category, a `root`
/// block, and the function-level facts later passes read beside the arena.
#[derive(Debug, Clone)]
pub struct Body {
    pub exprs: PrimaryMap<ExprId, ExprNode>,
    pub stmts: PrimaryMap<StmtId, StmtNode>,
    pub blocks: PrimaryMap<BlockId, BlockNode>,
    pub pats: PrimaryMap<PatId, PatNode>,
    pub root: BlockId,
    pub locals: Vec<NirLocal>,
    pub address_taken_locals: IndexSet<u32>,
    pub stores_aliased_locals: IndexSet<u32>,
    /// The function's pure-value graph — the source of truth for every
    /// [`Operand::Value`] in the skeleton. Built once
    /// by `lower::translate` and maintained in place by the optimizer's edits;
    /// never re-derived from the skeleton. Empty on a body built before operand
    /// promotion populates it.
    pub values: ValuePool,
    /// The per-function value graph (`value_of` + `loop_entry_values`), persisted
    /// here so it survives across optimizer passes instead of living as a
    /// per-`Engine`-session cache (build-once). `None`
    /// until the first value query builds it.
    pub value_graph: Option<crate::nir_value_graph::builder::ValueGraphBuild>,
}

impl Body {
    /// The type of an operand: the expr's `type_id` for `Operand::Expr`, or the
    /// promoted value's recorded source type for `Operand::Value` (WEP: operand
    /// promotion). Panics if a promoted value has no recorded type (a builder
    /// bug — every promotion records one).
    pub fn operand_type(&self, op: Operand) -> TypeId {
        match op {
            Operand::Expr(e) => self.exprs[e].type_id,
            Operand::Value(v) => self
                .values
                .type_of(v)
                .expect("promoted operand has no recorded type"),
        }
    }

    /// The raw integer bit pattern of a constant-int `Operand::Value`. `None` for
    /// an `Operand::Expr` or any non-int-constant operand. Width-agnostic (no type
    /// filter): callers that only need the value (capacities, zero-tests) avoid
    /// threading a `TypeTable`.
    pub fn operand_const_int(&self, op: Operand) -> Option<u64> {
        match self.values.kind(op.as_value()?) {
            ValueKind::Int(value, _) => Some(*value),
            _ => None,
        }
    }

    pub fn operand_const_bool(&self, op: Operand) -> Option<bool> {
        self.values.kind(op.as_value()?).as_bool()
    }

    /// The scalar value of a constant-char `Operand::Value`. `None` for an
    /// `Operand::Expr` or any non-char-constant operand.
    pub fn operand_const_char(&self, op: Operand) -> Option<char> {
        self.values.kind(op.as_value()?).as_char()
    }

    /// An empty body: no nodes and a placeholder `root` (set by the caller once
    /// the root block is built). Used by `lower::translate` as the canonical
    /// builder it pushes nodes into, and as a scratch arena for passes that
    /// build a working body of their own.
    pub fn empty() -> Self {
        Self {
            exprs: PrimaryMap::new(),
            stmts: PrimaryMap::new(),
            blocks: PrimaryMap::new(),
            pats: PrimaryMap::new(),
            root: BlockId::from_u32(0),
            locals: Vec::new(),
            address_taken_locals: IndexSet::default(),
            stores_aliased_locals: IndexSet::default(),
            values: ValuePool::new(),
            value_graph: None,
        }
    }

    /// A working copy of the node maps and `root` only — the function-level
    /// metadata (`locals`, the alias sets) is dropped, not cloned. For scratch
    /// use that mutates the nodes but never reads that metadata, e.g. niri's
    /// CTFE evaluator. Node ids are preserved, so ids from `self` stay valid.
    pub fn nodes_only_clone(&self) -> Self {
        Self {
            exprs: self.exprs.clone(),
            stmts: self.stmts.clone(),
            blocks: self.blocks.clone(),
            pats: self.pats.clone(),
            root: self.root,
            locals: Vec::new(),
            address_taken_locals: IndexSet::default(),
            stores_aliased_locals: IndexSet::default(),
            values: self.values.clone(),
            // Scratch clone (niri CTFE): the value graph is a per-function
            // optimizer artifact and is not carried into a node-only working copy.
            value_graph: None,
        }
    }

    /// Build a global-initializer-shaped body: one fresh expression node of
    /// `kind`, wrapped in a root block holding a single `Expr` statement.
    pub fn wrapping_expr(kind: ExprKind, type_id: TypeId, span: Span) -> Self {
        let mut body = Self::empty();
        let e = body.exprs.push(ExprNode {
            kind,
            type_id,
            span,
        });
        let s = body.stmts.push(StmtNode {
            kind: StmtKind::Expr(Operand::Expr(e)),
            span,
        });
        body.root = body.blocks.push(BlockNode {
            stmts: vec![s],
            span,
        });
        body
    }

    /// Like [`Body::wrapping_expr`] but the sole statement is a promoted pure
    /// value ([`Operand::Value`]) interned into the body's own pool. Used to
    /// build a single-value global initializer (e.g. a `Null` placeholder)
    /// directly in graph form, without a pure `ExprKind`.
    pub fn wrapping_value(
        kind: crate::nir_value_graph::ValueKind,
        type_id: TypeId,
        span: Span,
    ) -> Self {
        let mut body = Self::empty();
        let v = body.values.alloc_unshared(kind, type_id);
        let s = body.stmts.push(StmtNode {
            kind: StmtKind::Expr(Operand::Value(v)),
            span,
        });
        body.root = body.blocks.push(BlockNode {
            stmts: vec![s],
            span,
        });
        body
    }

    /// The value operand of a body that wraps a single expression — the form
    /// global initializers take, whose root block holds exactly one `Expr`
    /// statement. Panics if the body is not in that shape.
    pub fn sole_expr(&self) -> Operand {
        let block = &self.blocks[self.root];
        assert_eq!(
            block.stmts.len(),
            1,
            "expr-wrapper body must hold exactly one statement"
        );
        match self.stmts[block.stmts[0]].kind {
            StmtKind::Expr(op) => op,
            _ => panic!("expr-wrapper body statement must be an Expr"),
        }
    }
}

/// A NIR expression stored as an arena [`Body`] whose root block holds exactly
/// one `Expr` statement. This is how single-expression NIR positions (global
/// initializers) are represented so the optimizer engine and the arena passes
/// can operate on them uniformly, while the wrapped expression stays directly
/// reachable via [`ExprBody::expr`].
///
/// The newtype localizes the "root block = one `Expr` statement" invariant:
/// it is established at construction (`from_body` / `wrapping`) and read
/// through `expr()`, instead of every consumer rediscovering it via a bare
/// `Body::sole_expr` on a plain `Body`. Passes that need to run the rewrite
/// engine or mutate the body in place go through `body_mut()`, which keeps the
/// single-`Expr`-statement shape (engine rules at a global are expr-local).
#[derive(Debug, Clone)]
pub struct ExprBody {
    body: Body,
}

impl ExprBody {
    /// Wrap a `Body` that is already in single-`Expr`-statement form.
    pub fn from_body(body: Body) -> Self {
        debug_assert_eq!(
            body.blocks[body.root].stmts.len(),
            1,
            "ExprBody requires a single-statement root block"
        );
        Self { body }
    }

    /// Build an `ExprBody` from a single fresh expression node of `kind`.
    pub fn wrapping(kind: ExprKind, type_id: TypeId, span: Span) -> Self {
        Self {
            body: Body::wrapping_expr(kind, type_id, span),
        }
    }

    /// Build an `ExprBody` whose sole statement is a promoted pure value (see
    /// [`Body::wrapping_value`]).
    pub fn wrapping_value(
        kind: crate::nir_value_graph::ValueKind,
        type_id: TypeId,
        span: Span,
    ) -> Self {
        Self {
            body: Body::wrapping_value(kind, type_id, span),
        }
    }

    /// The wrapped value operand.
    pub fn expr(&self) -> Operand {
        self.body.sole_expr()
    }

    /// The underlying `Body` (for read-only arena traversal / lattice eval).
    pub fn body(&self) -> &Body {
        &self.body
    }

    /// The underlying `Body` for in-place rewrites (engine runs, call-site
    /// rewriting). Callers must preserve the single-`Expr`-statement root.
    pub fn body_mut(&mut self) -> &mut Body {
        &mut self.body
    }
}

/// Arena -> tree lowering.
impl Body {
    /// Deep-copy the expression subtree rooted at `id` into fresh arena nodes,
    /// returning the new root. The arena is a tree (one parent per node), so a
    /// rewrite that needs two references to one subtree must clone it rather
    /// than alias the id. This is the structural counterpart of
    /// `Engine::clone_expr` (which additionally maintains the engine's use
    /// index); passes not running under the engine clone through `Body`.
    pub fn clone_expr(&mut self, id: ExprId) -> ExprId {
        let node = self.exprs[id].clone();
        let kind = self.clone_expr_kind(node.kind);
        self.exprs.push(ExprNode {
            kind,
            type_id: node.type_id,
            span: node.span,
        })
    }

    /// Map every operand slot of a single expression node. A scoped rewrite
    /// (e.g. a loop subtree) collects the node ids and calls this on each.
    pub fn map_expr_operands(&mut self, id: ExprId, f: &mut impl FnMut(Operand) -> Operand) {
        match &mut self.exprs[id].kind {
            ExprKind::GlobalVarSet { value, .. } => *value = f(*value),
            ExprKind::Binary { left, right, .. } => {
                *left = f(*left);
                *right = f(*right);
            }
            ExprKind::Unary { expr, .. }
            | ExprKind::Cast { expr, .. }
            | ExprKind::FieldAccess { expr, .. }
            | ExprKind::VariantTag { expr }
            | ExprKind::VariantTest { expr, .. }
            | ExprKind::VariantPayload { expr, .. } => *expr = f(*expr),
            ExprKind::Assign { value, .. } => *value = f(*value),
            ExprKind::Index { expr, index } => {
                *expr = f(*expr);
                *index = f(*index);
            }
            ExprKind::Call { args, .. } => {
                for a in args {
                    a.expr = f(a.expr);
                }
            }
            ExprKind::CmRawCall { args, .. } => {
                for a in args {
                    *a = f(*a);
                }
            }
            ExprKind::If { condition, .. } => *condition = f(*condition),
            ExprKind::Match { expr, arms } => {
                *expr = f(*expr);
                for arm in arms {
                    if let Some(g) = arm.guard {
                        arm.guard = Some(f(g));
                    }
                    arm.body = f(arm.body);
                }
            }
            ExprKind::StructLiteral { fields, .. } => {
                for fld in fields {
                    fld.value = f(fld.value);
                }
            }
            ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                for el in elements {
                    *el = f(*el);
                }
            }
            ExprKind::IndirectCall { callee, args } => {
                *callee = f(*callee);
                for a in args {
                    *a = f(*a);
                }
            }
            ExprKind::ClosureToCanonical { functor, .. } => *functor = f(*functor),
            ExprKind::VariantConstruct { payload, .. } => {
                if let Some(p) = payload {
                    *payload = Some(f(*p));
                }
            }
            ExprKind::Switch { scrutinee, .. } => *scrutinee = f(*scrutinee),
            _ => {}
        }
    }

    /// Map every operand slot of a single statement node, the statement
    /// counterpart of [`Body::map_expr_operands`].
    pub fn map_stmt_operands(&mut self, id: StmtId, f: &mut impl FnMut(Operand) -> Operand) {
        match &mut self.stmts[id].kind {
            StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
                *value = f(*value);
            }
            StmtKind::Return { value } | StmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    *value = Some(f(*v));
                }
            }
            StmtKind::If { condition, .. } => *condition = f(*condition),
            _ => {}
        }
    }

    /// Take a node's content, leaving the slot `Dead` so every id pointing at it
    /// stays valid. The arena half of a node move — a caller holding an index
    /// (the rewrite engine's use index) layers its own bookkeeping on top.
    pub fn take_expr(&mut self, id: ExprId) -> ExprNode {
        let type_id = self.exprs[id].type_id;
        let span = self.exprs[id].span;
        std::mem::replace(
            &mut self.exprs[id],
            ExprNode {
                kind: ExprKind::Dead,
                type_id,
                span,
            },
        )
    }

    /// Deep-copy an operand. A promoted pure value is shared (the same pool
    /// backs the clone, so its `ValueId` stays valid); an effectful subtree is
    /// cloned into fresh nodes.
    fn clone_operand(&mut self, op: Operand) -> Operand {
        match op {
            Operand::Value(v) => Operand::Value(v),
            Operand::Expr(e) => Operand::Expr(self.clone_expr(e)),
        }
    }

    /// Deep-copy a block subtree into fresh arena nodes, returning the new
    /// block id. The block-level counterpart of [`Body::clone_expr`].
    pub fn clone_block(&mut self, id: BlockId) -> BlockId {
        let node = self.blocks[id].clone();
        let stmts = node.stmts.iter().map(|s| self.clone_stmt(*s)).collect();
        self.blocks.push(BlockNode {
            stmts,
            span: node.span,
        })
    }

    fn clone_stmt(&mut self, id: StmtId) -> StmtId {
        let node = self.stmts[id].clone();
        let kind = self.clone_stmt_kind(node.kind);
        self.stmts.push(StmtNode {
            kind,
            span: node.span,
        })
    }

    fn clone_pat(&mut self, id: PatId) -> PatId {
        let node = self.pats[id].clone();
        let kind = self.clone_pat_kind(node.kind);
        self.pats.push(PatNode {
            kind,
            span: node.span,
        })
    }

    fn clone_expr_kind(&mut self, kind: ExprKind) -> ExprKind {
        match kind {
            ExprKind::GlobalVarSet {
                module_source,
                name,
                value,
            } => ExprKind::GlobalVarSet {
                module_source,
                name,
                value: self.clone_operand(value),
            },
            ExprKind::Binary { left, op, right } => ExprKind::Binary {
                left: self.clone_operand(left),
                op,
                right: self.clone_operand(right),
            },
            ExprKind::Unary { op, expr } => ExprKind::Unary {
                op,
                expr: self.clone_operand(expr),
            },
            ExprKind::Assign { target, value } => ExprKind::Assign {
                target: self.clone_expr(target),
                value: self.clone_operand(value),
            },
            ExprKind::Cast { expr, target_type } => ExprKind::Cast {
                expr: self.clone_operand(expr),
                target_type,
            },
            ExprKind::Call {
                func_id,
                type_args,
                args,
                has_receiver,
            } => ExprKind::Call {
                func_id,
                type_args,
                args: args
                    .into_iter()
                    .map(|a| ArenaCallArg {
                        expr: self.clone_operand(a.expr),
                        is_mut: a.is_mut,
                    })
                    .collect(),
                has_receiver,
            },
            ExprKind::CmRawCall { local_name, args } => ExprKind::CmRawCall {
                local_name,
                args: args.into_iter().map(|a| self.clone_operand(a)).collect(),
            },
            ExprKind::FieldAccess {
                expr,
                field_index,
                field_name,
            } => ExprKind::FieldAccess {
                expr: self.clone_operand(expr),
                field_index,
                field_name,
            },
            ExprKind::Index { expr, index } => ExprKind::Index {
                expr: self.clone_operand(expr),
                index: self.clone_operand(index),
            },
            ExprKind::Block(b) => ExprKind::Block(self.clone_block(b)),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => ExprKind::If {
                condition: self.clone_operand(condition),
                then_branch: self.clone_block(then_branch),
                else_branch: else_branch.map(|b| self.clone_block(b)),
            },
            ExprKind::Match { expr, arms } => ExprKind::Match {
                expr: self.clone_operand(expr),
                arms: arms
                    .into_iter()
                    .map(|a| ArmData {
                        pattern: self.clone_pat(a.pattern),
                        guard: a.guard.map(|g| self.clone_operand(g)),
                        body: self.clone_operand(a.body),
                        span: a.span,
                    })
                    .collect(),
            },
            ExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields,
            } => ExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields: fields
                    .into_iter()
                    .map(|f| ArenaStructField {
                        name: f.name,
                        value: self.clone_operand(f.value),
                        field_index: f.field_index,
                    })
                    .collect(),
            },
            ExprKind::TupleLiteral { elements } => ExprKind::TupleLiteral {
                elements: elements
                    .into_iter()
                    .map(|e| self.clone_operand(e))
                    .collect(),
            },
            ExprKind::ArrayLiteral { elements } => ExprKind::ArrayLiteral {
                elements: elements
                    .into_iter()
                    .map(|e| self.clone_operand(e))
                    .collect(),
            },
            ExprKind::IndirectCall { callee, args } => ExprKind::IndirectCall {
                callee: self.clone_operand(callee),
                args: args.into_iter().map(|a| self.clone_operand(a)).collect(),
            },
            ExprKind::ClosureToCanonical {
                functor,
                functor_id,
                target_fn_type,
                closure_module,
            } => ExprKind::ClosureToCanonical {
                functor: self.clone_operand(functor),
                functor_id,
                target_fn_type,
                closure_module,
            },
            ExprKind::VariantConstruct {
                variant_type,
                case_index,
                case_name,
                payload,
            } => ExprKind::VariantConstruct {
                variant_type,
                case_index,
                case_name,
                payload: payload.map(|p| self.clone_operand(p)),
            },
            ExprKind::LabeledBlock {
                label,
                block,
                result_type,
            } => ExprKind::LabeledBlock {
                label,
                block: self.clone_block(block),
                result_type,
            },
            ExprKind::VariantTag { expr } => ExprKind::VariantTag {
                expr: self.clone_operand(expr),
            },
            ExprKind::VariantTest {
                expr,
                case_index,
                case_name,
            } => ExprKind::VariantTest {
                expr: self.clone_operand(expr),
                case_index,
                case_name,
            },
            ExprKind::VariantPayload {
                expr,
                case_index,
                payload_type,
            } => ExprKind::VariantPayload {
                expr: self.clone_operand(expr),
                case_index,
                payload_type,
            },
            ExprKind::Switch {
                scrutinee,
                min_value,
                arms,
                default,
            } => ExprKind::Switch {
                scrutinee: self.clone_operand(scrutinee),
                min_value,
                arms: arms.into_iter().map(|a| self.clone_block(a)).collect(),
                default: self.clone_block(default),
            },
            // Leaves carry no id children.
            leaf => leaf,
        }
    }

    fn clone_stmt_kind(&mut self, kind: StmtKind) -> StmtKind {
        match kind {
            StmtKind::Let {
                name,
                local_index,
                is_mut,
                is_reactive,
                type_id,
                value,
                skip_value_copy,
            } => StmtKind::Let {
                name,
                local_index,
                is_mut,
                is_reactive,
                type_id,
                value: self.clone_operand(value),
                skip_value_copy,
            },
            StmtKind::Expr(e) => StmtKind::Expr(self.clone_operand(e)),
            StmtKind::Return { value } => StmtKind::Return {
                value: value.map(|o| self.clone_operand(o)),
            },
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => StmtKind::If {
                condition: self.clone_operand(condition),
                then_block: self.clone_block(then_block),
                else_block: else_block.map(|b| self.clone_block(b)),
            },
            StmtKind::Loop { body } => StmtKind::Loop {
                body: self.clone_block(body),
            },
            StmtKind::Break { label, value } => StmtKind::Break {
                label,
                value: value.map(|o| self.clone_operand(o)),
            },
            StmtKind::Continue => StmtKind::Continue,
            StmtKind::LabeledBlock { label, block } => StmtKind::LabeledBlock {
                label,
                block: self.clone_block(block),
            },
            StmtKind::LetDestructure {
                pattern,
                is_mut,
                value,
            } => StmtKind::LetDestructure {
                pattern: self.clone_pat(pattern),
                is_mut,
                value: self.clone_operand(value),
            },
        }
    }

    fn clone_pat_kind(&mut self, kind: PatKind) -> PatKind {
        match kind {
            PatKind::Tuple(ps, rest) => {
                PatKind::Tuple(ps.into_iter().map(|p| self.clone_pat(p)).collect(), rest)
            }
            PatKind::Or(ps) => PatKind::Or(ps.into_iter().map(|p| self.clone_pat(p)).collect()),
            PatKind::Variant {
                enum_type,
                variant_name,
                bindings,
                payload_type,
            } => PatKind::Variant {
                enum_type,
                variant_name,
                bindings: bindings.into_iter().map(|p| self.clone_pat(p)).collect(),
                payload_type,
            },
            PatKind::Struct {
                struct_type,
                fields,
                has_rest,
            } => PatKind::Struct {
                struct_type,
                fields: fields
                    .into_iter()
                    .map(|f| ArenaStructPatternField {
                        field_name: f.field_name,
                        field_index: f.field_index,
                        pattern: self.clone_pat(f.pattern),
                    })
                    .collect(),
                has_rest,
            },
            PatKind::ConstantValue { expr } => PatKind::ConstantValue {
                expr: self.clone_operand(expr),
            },
            // Leaves carry no id children.
            leaf => leaf,
        }
    }
}

/// Structural navigation used by the rewrite engine (parent map + worklist).
impl Body {
    /// Collect every local with a live `&local` / `&mut local` in the body.
    /// The canonical `address_taken_locals` / `stores_aliased_locals` sets
    /// go stale after `inline` / `ref_elim` copy reference nodes, so
    /// alias-sensitive consumers union this scan in.
    pub fn collect_address_taken_locals(&self, out: &mut crate::hashmap::IndexSet<u32>) {
        let mut stack: Vec<NodeRef> = vec![NodeRef::Block(self.root)];
        while let Some(node) = stack.pop() {
            if let NodeRef::Expr(id) = node
                && let ExprKind::Unary {
                    op: crate::nir::NirUnaryOp::Ref | crate::nir::NirUnaryOp::MutRef,
                    expr: inner,
                } = &self.exprs[id].kind
                && let Some(inner) = inner.as_expr()
                && let ExprKind::Local { index, .. } = &self.exprs[inner].kind
            {
                out.insert(*index);
            }
            self.for_each_child(node, |c| stack.push(c));
        }
    }

    /// Invoke `f` on every operand slot of `node`, in source order — the
    /// mutable twin of [`Body::for_each_operand`]. Non-operand `ExprId` slots
    /// (`Assign::target`) and structural children (blocks, patterns) are not
    /// operands and are not visited.
    ///
    /// A `&mut` walk cannot be a filter over the shared [`Slot`] one, so this
    /// match restates the operand positions. A debug assertion compares its
    /// slot count against [`Body::for_each_operand`] on every call, so a variant
    /// taught to one and not the other fails the first test that reaches that
    /// node kind.
    pub fn for_each_operand_mut(&mut self, node: NodeRef, mut f: impl FnMut(&mut Operand)) {
        #[cfg(debug_assertions)]
        let expected = {
            let mut n = 0usize;
            self.for_each_operand(node, |_| n += 1);
            n
        };
        #[cfg(debug_assertions)]
        let mut seen = 0usize;
        #[cfg(debug_assertions)]
        let mut f = |o: &mut Operand| {
            seen += 1;
            f(o);
        };
        self.for_each_operand_mut_inner(node, &mut f);
        #[cfg(debug_assertions)]
        assert_eq!(
            seen, expected,
            "operand slots of {node:?} disagree between `for_each_operand` and \
             `for_each_operand_mut`"
        );
    }

    fn for_each_operand_mut_inner(&mut self, node: NodeRef, f: &mut impl FnMut(&mut Operand)) {
        match node {
            NodeRef::Block(_) => {}
            NodeRef::Pat(p) => {
                if let PatKind::ConstantValue { expr } = &mut self.pats[p].kind {
                    f(expr);
                }
            }
            NodeRef::Stmt(s) => match &mut self.stmts[s].kind {
                StmtKind::Let { value, .. }
                | StmtKind::Expr(value)
                | StmtKind::LetDestructure { value, .. } => f(value),
                StmtKind::Return { value } | StmtKind::Break { value, .. } => {
                    if let Some(o) = value {
                        f(o);
                    }
                }
                StmtKind::If { condition, .. } => f(condition),
                StmtKind::Loop { .. } | StmtKind::Continue | StmtKind::LabeledBlock { .. } => {}
            },
            NodeRef::Expr(e) => match &mut self.exprs[e].kind {
                ExprKind::GlobalVarSet { value, .. } => f(value),
                ExprKind::Binary { left, right, .. } => {
                    f(left);
                    f(right);
                }
                ExprKind::Unary { expr, .. }
                | ExprKind::Cast { expr, .. }
                | ExprKind::FieldAccess { expr, .. }
                | ExprKind::VariantTag { expr }
                | ExprKind::VariantTest { expr, .. }
                | ExprKind::VariantPayload { expr, .. }
                | ExprKind::Assign { value: expr, .. } => f(expr),
                ExprKind::Index { expr, index } => {
                    f(expr);
                    f(index);
                }
                ExprKind::Call { args, .. } => {
                    for a in args {
                        f(&mut a.expr);
                    }
                }
                ExprKind::CmRawCall { args, .. } => {
                    for a in args {
                        f(a);
                    }
                }
                ExprKind::If { condition, .. } => f(condition),
                ExprKind::Match { expr, arms } => {
                    f(expr);
                    for arm in arms {
                        if let Some(g) = &mut arm.guard {
                            f(g);
                        }
                        f(&mut arm.body);
                    }
                }
                ExprKind::StructLiteral { fields, .. } => {
                    for fld in fields {
                        f(&mut fld.value);
                    }
                }
                ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                    for el in elements {
                        f(el);
                    }
                }
                ExprKind::IndirectCall { callee, args } => {
                    f(callee);
                    for a in args {
                        f(a);
                    }
                }
                ExprKind::ClosureToCanonical { functor, .. } => f(functor),
                ExprKind::VariantConstruct { payload, .. } => {
                    if let Some(p) = payload {
                        f(p);
                    }
                }
                ExprKind::Switch { scrutinee, .. } => f(scrutinee),
                ExprKind::PackedArray(_)
                | ExprKind::Dead
                | ExprKind::Local { .. }
                | ExprKind::GlobalVarGet { .. }
                | ExprKind::EnumConstruct { .. }
                | ExprKind::Block(_)
                | ExprKind::LabeledBlock { .. } => {}
            },
        }
    }

    /// Replace one operand slot of `node` holding `Operand::Value(from)` with
    /// `new`, returning whether a slot changed. One slot per call, so a caller
    /// planting an expression mints a fresh node for each rather than hanging
    /// one id under two parents; loop until it returns `false`.
    pub fn replace_value_operand_once(
        &mut self,
        node: NodeRef,
        from: ValueId,
        new: Operand,
    ) -> bool {
        let mut done = false;
        self.for_each_operand_mut(node, |o| {
            if !done && *o == Operand::Value(from) {
                *o = new;
                done = true;
            }
        });
        done
    }

    /// Replace every direct operand child of `node` equal to `Operand::Expr(target)`
    /// with `new`, returning whether any slot changed. Used by the engine to promote
    /// a folded subtree to an `Operand::Value` in its parent (WEP: The Live
    /// `ValueGraph`).
    pub fn replace_operand_to(&mut self, node: NodeRef, target: ExprId, new: Operand) -> bool {
        let mut changed = false;
        self.for_each_operand_mut(node, |o| {
            if *o == Operand::Expr(target) {
                *o = new;
                changed = true;
            }
        });
        changed
    }

    /// The span of any arena node, so a rewrite that mints a node in a slot can
    /// keep the position of what it replaces.
    pub fn span_of(&self, node: NodeRef) -> Span {
        match node {
            NodeRef::Expr(e) => self.exprs[e].span,
            NodeRef::Stmt(s) => self.stmts[s].span,
            NodeRef::Block(b) => self.blocks[b].span,
            NodeRef::Pat(p) => self.pats[p].span,
        }
    }

    /// Invoke `f` on every id-bearing child of `node`, in source order.
    /// Arms / fields / call args are transparent (their inline child ids are
    /// visited directly). Leaf nodes invoke `f` zero times.
    ///
    /// A promoted pure value (`Operand::Value`) has no skeleton child, so an
    /// operand slot yields one only when it holds an `Operand::Expr`; a pass
    /// whose census must also see what a promoted value reads walks
    /// [`Body::for_each_operand`] beside this.
    pub fn for_each_child(&self, node: NodeRef, mut f: impl FnMut(NodeRef)) {
        self.for_each_slot(node, &mut |slot| match slot {
            Slot::Operand(Operand::Expr(e)) => f(NodeRef::Expr(e)),
            Slot::Operand(Operand::Value(_)) => {}
            Slot::Node(child) => f(child),
        });
    }

    /// Invoke `f` on every operand slot of `node`, in source order — including
    /// a promoted [`Operand::Value`], which [`Body::for_each_child`] drops for
    /// want of a skeleton node. Non-operand id slots (`Assign::target`) and
    /// structural children (blocks, patterns) are not operands and are skipped.
    ///
    /// This and [`Body::for_each_child`] filter one description of the node
    /// structure ([`Slot`]), so a variant added to the arena cannot be seen by
    /// one and missed by the other.
    pub fn for_each_operand(&self, node: NodeRef, mut f: impl FnMut(Operand)) {
        self.for_each_slot(node, &mut |slot| {
            if let Slot::Operand(o) = slot {
                f(o);
            }
        });
    }

    /// The node's shape, in source order: every operand slot and every
    /// structural / non-operand id child. The two shared walks above filter
    /// this; [`Body::for_each_operand_mut`] is the one restatement, and is
    /// checked against it.
    fn for_each_slot(&self, node: NodeRef, f: &mut impl FnMut(Slot)) {
        match node {
            NodeRef::Block(b) => {
                for s in &self.blocks[b].stmts {
                    f(Slot::Node(NodeRef::Stmt(*s)));
                }
            }
            NodeRef::Stmt(s) => match &self.stmts[s].kind {
                StmtKind::Let { value, .. } => f(Slot::Operand(*value)),
                StmtKind::Expr(e) => f(Slot::Operand(*e)),
                StmtKind::Return { value } => {
                    if let Some(o) = value {
                        f(Slot::Operand(*o));
                    }
                }
                StmtKind::If {
                    condition,
                    then_block,
                    else_block,
                } => {
                    f(Slot::Operand(*condition));
                    f(Slot::Node(NodeRef::Block(*then_block)));
                    if let Some(b) = else_block {
                        f(Slot::Node(NodeRef::Block(*b)));
                    }
                }
                StmtKind::Loop { body } => f(Slot::Node(NodeRef::Block(*body))),
                StmtKind::Break { value, .. } => {
                    if let Some(o) = value {
                        f(Slot::Operand(*o));
                    }
                }
                StmtKind::Continue => {}
                StmtKind::LabeledBlock { block, .. } => f(Slot::Node(NodeRef::Block(*block))),
                StmtKind::LetDestructure { pattern, value, .. } => {
                    f(Slot::Node(NodeRef::Pat(*pattern)));
                    f(Slot::Operand(*value));
                }
            },
            NodeRef::Pat(p) => match &self.pats[p].kind {
                PatKind::Wildcard
                | PatKind::Binding { .. }
                | PatKind::Literal(_)
                | PatKind::Enum { .. }
                | PatKind::Range { .. } => {}
                PatKind::Tuple(ps, _) | PatKind::Or(ps) => {
                    for p in ps {
                        f(Slot::Node(NodeRef::Pat(*p)));
                    }
                }
                PatKind::Variant { bindings, .. } => {
                    for p in bindings {
                        f(Slot::Node(NodeRef::Pat(*p)));
                    }
                }
                PatKind::Struct { fields, .. } => {
                    for fld in fields {
                        f(Slot::Node(NodeRef::Pat(fld.pattern)));
                    }
                }
                PatKind::ConstantValue { expr } => f(Slot::Operand(*expr)),
            },
            NodeRef::Expr(e) => match &self.exprs[e].kind {
                ExprKind::PackedArray(_)
                | ExprKind::Dead
                | ExprKind::Local { .. }
                | ExprKind::GlobalVarGet { .. }
                | ExprKind::EnumConstruct { .. } => {}
                ExprKind::GlobalVarSet { value, .. } => f(Slot::Operand(*value)),
                ExprKind::Binary { left, right, .. } => {
                    f(Slot::Operand(*left));
                    f(Slot::Operand(*right));
                }
                ExprKind::Unary { expr, .. }
                | ExprKind::Cast { expr, .. }
                | ExprKind::FieldAccess { expr, .. }
                | ExprKind::VariantTag { expr }
                | ExprKind::VariantTest { expr, .. }
                | ExprKind::VariantPayload { expr, .. } => f(Slot::Operand(*expr)),
                ExprKind::Assign { target, value } => {
                    f(Slot::Node(NodeRef::Expr(*target)));
                    f(Slot::Operand(*value));
                }
                ExprKind::Index { expr, index } => {
                    f(Slot::Operand(*expr));
                    f(Slot::Operand(*index));
                }
                ExprKind::Call { args, .. } => {
                    for a in args {
                        f(Slot::Operand(a.expr));
                    }
                }
                ExprKind::CmRawCall { args, .. } => {
                    for a in args {
                        f(Slot::Operand(*a));
                    }
                }
                ExprKind::Block(b) => f(Slot::Node(NodeRef::Block(*b))),
                ExprKind::If {
                    condition,
                    then_branch,
                    else_branch,
                } => {
                    f(Slot::Operand(*condition));
                    f(Slot::Node(NodeRef::Block(*then_branch)));
                    if let Some(b) = else_branch {
                        f(Slot::Node(NodeRef::Block(*b)));
                    }
                }
                ExprKind::Match { expr, arms } => {
                    f(Slot::Operand(*expr));
                    for arm in arms {
                        f(Slot::Node(NodeRef::Pat(arm.pattern)));
                        if let Some(g) = arm.guard {
                            f(Slot::Operand(g));
                        }
                        f(Slot::Operand(arm.body));
                    }
                }
                ExprKind::StructLiteral { fields, .. } => {
                    for fld in fields {
                        f(Slot::Operand(fld.value));
                    }
                }
                ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                    for el in elements {
                        f(Slot::Operand(*el));
                    }
                }
                ExprKind::IndirectCall { callee, args } => {
                    f(Slot::Operand(*callee));
                    for a in args {
                        f(Slot::Operand(*a));
                    }
                }
                ExprKind::ClosureToCanonical { functor, .. } => f(Slot::Operand(*functor)),
                ExprKind::VariantConstruct { payload, .. } => {
                    if let Some(p) = payload {
                        f(Slot::Operand(*p));
                    }
                }
                ExprKind::LabeledBlock { block, .. } => f(Slot::Node(NodeRef::Block(*block))),
                ExprKind::Switch {
                    scrutinee,
                    arms,
                    default,
                    ..
                } => {
                    f(Slot::Operand(*scrutinee));
                    for a in arms {
                        f(Slot::Node(NodeRef::Block(*a)));
                    }
                    f(Slot::Node(NodeRef::Block(*default)));
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expr(body: &mut Body, kind: ExprKind) -> ExprId {
        body.exprs.push(ExprNode {
            kind,
            type_id: TypeId(0),
            span: Span::default(),
        })
    }

    #[test]
    fn for_each_operand_yields_the_promoted_value_for_each_child_drops() {
        let mut body = Body::empty();
        let left = expr(
            &mut body,
            ExprKind::Local {
                index: 0,
                name: "a".to_string(),
            },
        );
        let right = body.values.canonical_local(1, TypeId(0));
        let add = expr(
            &mut body,
            ExprKind::Binary {
                left: Operand::Expr(left),
                op: NirBinaryOp::Add,
                right: Operand::Value(right),
            },
        );

        let mut children = Vec::new();
        body.for_each_child(NodeRef::Expr(add), |c| children.push(c));
        assert_eq!(children, vec![NodeRef::Expr(left)]);

        let mut operands = Vec::new();
        body.for_each_operand(NodeRef::Expr(add), |o| operands.push(o));
        assert_eq!(
            operands,
            vec![Operand::Expr(left), Operand::Value(right)],
            "operand order must match the skeleton child order"
        );
    }

    #[test]
    fn for_each_operand_skips_non_operand_id_slots() {
        let mut body = Body::empty();
        let target = expr(
            &mut body,
            ExprKind::Local {
                index: 0,
                name: "a".to_string(),
            },
        );
        let value = body.values.canonical_local(1, TypeId(0));
        let assign = expr(
            &mut body,
            ExprKind::Assign {
                target,
                value: Operand::Value(value),
            },
        );

        let mut operands = Vec::new();
        body.for_each_operand(NodeRef::Expr(assign), |o| operands.push(o));
        assert_eq!(operands, vec![Operand::Value(value)]);
    }
}
