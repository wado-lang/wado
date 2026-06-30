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
//! `docs/wep-2026-06-05-nir-skeleton-arena.md`.

use cranelift_entity::{PrimaryMap, entity_impl};

use crate::hashmap::IndexSet;
use crate::nir::{NirBinaryOp, NirLocal, NirUnaryOp};
use crate::nir_value_graph::{ValueId, ValueKind, ValuePool};
use crate::tir::TypeId;
use crate::token::Span;

/// An operand position in the skeleton — an expression's value, after operand
/// promotion (WEP: The Live `ValueGraph`). It is either a pure value living in the
/// function's [`ValuePool`] (literals, `Binary`, pure `Unary`, `Cast`, and the
/// `Local` / `FieldAccess` reads the graph resolves to a value), or an effectful
/// / control subtree kept in the skeleton (`Call`, `MethodCall`, allocation
/// literals, `If` / `Match` / `Block` value positions). Pure values no longer
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
/// and parent map. See `docs/wep-2026-06-05-nir-rewrite-engine-design.md`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum NodeRef {
    Expr(ExprId),
    Stmt(StmtId),
    Block(BlockId),
    Pat(PatId),
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
        args: Vec<ArenaCallArg>,
    },
    CmRawCall {
        local_name: String,
        args: Vec<Operand>,
    },
    MethodCall {
        receiver: Operand,
        func_id: crate::nir::FuncId,
        type_args: Vec<TypeId>,
        args: Vec<ArenaCallArg>,
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
    /// [`Operand::Value`] in the skeleton (WEP: The Live `ValueGraph`). Built once
    /// by `lower::translate` and maintained in place by the optimizer's edits;
    /// never re-derived from the skeleton. Empty on a body built before operand
    /// promotion populates it.
    pub values: ValuePool,
    /// The per-function value graph (`value_of` + `loop_entry_values`), persisted
    /// here so it survives across optimizer passes instead of living as a
    /// per-`Engine`-session cache (WEP: The Live `ValueGraph`, build-once). `None`
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

    /// The value of a constant-bool `Operand::Value` (e.g. a condition driven to
    /// a literal by `condition_implication`). `None` for any other operand.
    pub fn operand_const_bool(&self, op: Operand) -> Option<bool> {
        match self.values.kind(op.as_value()?) {
            ValueKind::Bool(b) => Some(*b),
            _ => None,
        }
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

    /// Apply `f` to every operand slot in the body, replacing it in place. Used
    /// by promotion to lift pure operands to `Operand::Value` (WEP: The Live
    /// `ValueGraph`). `f` is the only mutator; it does not borrow the body.
    pub fn map_operands(&mut self, mut f: impl FnMut(Operand) -> Operand) {
        let eids: Vec<ExprId> = self.exprs.keys().collect();
        for id in eids {
            self.map_expr_operands(id, &mut f);
        }
        let sids: Vec<StmtId> = self.stmts.keys().collect();
        for id in sids {
            self.map_stmt_operands(id, &mut f);
        }
        let pids: Vec<PatId> = self.pats.keys().collect();
        for id in pids {
            if let PatKind::ConstantValue { expr } = &mut self.pats[id].kind {
                *expr = f(*expr);
            }
        }
    }

    /// Map every operand slot of a single expression node. The per-node half of
    /// [`Body::map_operands`]; a scoped rewrite (e.g. a loop subtree) collects
    /// the node ids and calls this on each.
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
            ExprKind::Call { args, .. } | ExprKind::MethodCall { args, .. } => {
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

    /// Map every operand slot of a single statement node (the per-node half of
    /// [`Body::map_operands`]).
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

    /// Read-only visit of every operand slot in the body — the `&self` mirror of
    /// [`Body::map_operands`]. Visits all slots (including any orphaned subtree
    /// not yet DCE'd), which over-approximates "live" but stays conservative for
    /// liveness queries.
    pub fn for_each_operand(&self, mut f: impl FnMut(Operand)) {
        for id in self.exprs.keys() {
            match &self.exprs[id].kind {
                ExprKind::GlobalVarSet { value, .. } => f(*value),
                ExprKind::Binary { left, right, .. } => {
                    f(*left);
                    f(*right);
                }
                ExprKind::Unary { expr, .. }
                | ExprKind::Cast { expr, .. }
                | ExprKind::FieldAccess { expr, .. }
                | ExprKind::VariantTag { expr }
                | ExprKind::VariantTest { expr, .. }
                | ExprKind::VariantPayload { expr, .. } => f(*expr),
                ExprKind::Assign { value, .. } => f(*value),
                ExprKind::Index { expr, index } => {
                    f(*expr);
                    f(*index);
                }
                ExprKind::Call { args, .. } | ExprKind::MethodCall { args, .. } => {
                    for a in args {
                        f(a.expr);
                    }
                }
                ExprKind::CmRawCall { args, .. } => {
                    for a in args {
                        f(*a);
                    }
                }
                ExprKind::If { condition, .. } => f(*condition),
                ExprKind::Match { expr, arms } => {
                    f(*expr);
                    for arm in arms {
                        if let Some(g) = arm.guard {
                            f(g);
                        }
                        f(arm.body);
                    }
                }
                ExprKind::StructLiteral { fields, .. } => {
                    for fld in fields {
                        f(fld.value);
                    }
                }
                ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                    for el in elements {
                        f(*el);
                    }
                }
                ExprKind::IndirectCall { callee, args } => {
                    f(*callee);
                    for a in args {
                        f(*a);
                    }
                }
                ExprKind::ClosureToCanonical { functor, .. } => f(*functor),
                ExprKind::VariantConstruct { payload, .. } => {
                    if let Some(p) = payload {
                        f(*p);
                    }
                }
                ExprKind::Switch { scrutinee, .. } => f(*scrutinee),
                _ => {}
            }
        }
        for id in self.stmts.keys() {
            match &self.stmts[id].kind {
                StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => f(*value),
                StmtKind::Return { value } | StmtKind::Break { value, .. } => {
                    if let Some(v) = value {
                        f(*v);
                    }
                }
                StmtKind::If { condition, .. } => f(*condition),
                _ => {}
            }
        }
        for id in self.pats.keys() {
            if let PatKind::ConstantValue { expr } = &self.pats[id].kind {
                f(*expr);
            }
        }
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
            },
            ExprKind::CmRawCall { local_name, args } => ExprKind::CmRawCall {
                local_name,
                args: args.into_iter().map(|a| self.clone_operand(a)).collect(),
            },
            ExprKind::MethodCall {
                receiver,
                func_id,
                type_args,
                args,
            } => ExprKind::MethodCall {
                receiver: self.clone_operand(receiver),
                func_id,
                type_args,
                args: args
                    .into_iter()
                    .map(|a| ArenaCallArg {
                        expr: self.clone_operand(a.expr),
                        is_mut: a.is_mut,
                    })
                    .collect(),
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
    /// Invoke `f` on every id-bearing child of `node`, in source order.
    /// Arms / fields / call args are transparent (their inline child ids are
    /// visited directly). Leaf nodes invoke `f` zero times.
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

    /// Replace every direct operand child of `node` equal to `Operand::Expr(target)`
    /// with `new`, returning whether any slot changed. Covers exactly the operand
    /// positions [`Body::for_each_child`] descends through as `op_child`; non-operand
    /// `ExprId` slots (`Assign::target`) and structural children (blocks, patterns)
    /// are untouched. Used by the engine to promote a folded subtree to an
    /// `Operand::Value` in its parent (WEP: The Live `ValueGraph`).
    pub fn replace_operand_to(&mut self, node: NodeRef, target: ExprId, new: Operand) -> bool {
        let mut changed = false;
        let mut swap = |o: &mut Operand| {
            if *o == Operand::Expr(target) {
                *o = new;
                changed = true;
            }
        };
        match node {
            NodeRef::Block(_) => {}
            NodeRef::Pat(p) => {
                if let PatKind::ConstantValue { expr } = &mut self.pats[p].kind {
                    swap(expr);
                }
            }
            NodeRef::Stmt(s) => match &mut self.stmts[s].kind {
                StmtKind::Let { value, .. }
                | StmtKind::Expr(value)
                | StmtKind::LetDestructure { value, .. } => swap(value),
                StmtKind::Return { value } | StmtKind::Break { value, .. } => {
                    if let Some(o) = value {
                        swap(o);
                    }
                }
                StmtKind::If { condition, .. } => swap(condition),
                StmtKind::Loop { .. } | StmtKind::Continue | StmtKind::LabeledBlock { .. } => {}
            },
            NodeRef::Expr(e) => match &mut self.exprs[e].kind {
                ExprKind::GlobalVarSet { value, .. } => swap(value),
                ExprKind::Binary { left, right, .. } => {
                    swap(left);
                    swap(right);
                }
                ExprKind::Unary { expr, .. }
                | ExprKind::Cast { expr, .. }
                | ExprKind::FieldAccess { expr, .. }
                | ExprKind::VariantTag { expr }
                | ExprKind::VariantTest { expr, .. }
                | ExprKind::VariantPayload { expr, .. }
                | ExprKind::Assign { value: expr, .. } => swap(expr),
                ExprKind::Index { expr, index } => {
                    swap(expr);
                    swap(index);
                }
                ExprKind::Call { args, .. } => {
                    for a in args {
                        swap(&mut a.expr);
                    }
                }
                ExprKind::MethodCall { receiver, args, .. } => {
                    swap(receiver);
                    for a in args {
                        swap(&mut a.expr);
                    }
                }
                ExprKind::CmRawCall { args, .. } => {
                    for a in args {
                        swap(a);
                    }
                }
                ExprKind::If { condition, .. } => swap(condition),
                ExprKind::Match { expr, arms } => {
                    swap(expr);
                    for arm in arms {
                        if let Some(g) = &mut arm.guard {
                            swap(g);
                        }
                        swap(&mut arm.body);
                    }
                }
                ExprKind::StructLiteral { fields, .. } => {
                    for fld in fields {
                        swap(&mut fld.value);
                    }
                }
                ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                    for el in elements {
                        swap(el);
                    }
                }
                ExprKind::IndirectCall { callee, args } => {
                    swap(callee);
                    for a in args {
                        swap(a);
                    }
                }
                ExprKind::ClosureToCanonical { functor, .. } => swap(functor),
                ExprKind::VariantConstruct { payload, .. } => {
                    if let Some(p) = payload {
                        swap(p);
                    }
                }
                ExprKind::Switch { scrutinee, .. } => swap(scrutinee),
                _ => {}
            },
        }
        changed
    }

    pub fn for_each_child(&self, node: NodeRef, mut f: impl FnMut(NodeRef)) {
        // A promoted pure value (`Operand::Value`) has no skeleton child; only an
        // `Operand::Expr` yields one.
        fn op_child<F: FnMut(NodeRef)>(o: Operand, f: &mut F) {
            if let Operand::Expr(e) = o {
                f(NodeRef::Expr(e));
            }
        }
        match node {
            NodeRef::Block(b) => {
                for s in &self.blocks[b].stmts {
                    f(NodeRef::Stmt(*s));
                }
            }
            NodeRef::Stmt(s) => match &self.stmts[s].kind {
                StmtKind::Let { value, .. } => op_child(*value, &mut f),
                StmtKind::Expr(e) => op_child(*e, &mut f),
                StmtKind::Return { value } => {
                    if let Some(o) = value {
                        op_child(*o, &mut f);
                    }
                }
                StmtKind::If {
                    condition,
                    then_block,
                    else_block,
                } => {
                    op_child(*condition, &mut f);
                    f(NodeRef::Block(*then_block));
                    if let Some(b) = else_block {
                        f(NodeRef::Block(*b));
                    }
                }
                StmtKind::Loop { body } => f(NodeRef::Block(*body)),
                StmtKind::Break { value, .. } => {
                    if let Some(o) = value {
                        op_child(*o, &mut f);
                    }
                }
                StmtKind::Continue => {}
                StmtKind::LabeledBlock { block, .. } => f(NodeRef::Block(*block)),
                StmtKind::LetDestructure { pattern, value, .. } => {
                    f(NodeRef::Pat(*pattern));
                    op_child(*value, &mut f);
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
                        f(NodeRef::Pat(*p));
                    }
                }
                PatKind::Variant { bindings, .. } => {
                    for p in bindings {
                        f(NodeRef::Pat(*p));
                    }
                }
                PatKind::Struct { fields, .. } => {
                    for fld in fields {
                        f(NodeRef::Pat(fld.pattern));
                    }
                }
                PatKind::ConstantValue { expr } => op_child(*expr, &mut f),
            },
            NodeRef::Expr(e) => match &self.exprs[e].kind {
                ExprKind::PackedArray(_)
                | ExprKind::Dead
                | ExprKind::Local { .. }
                | ExprKind::GlobalVarGet { .. }
                | ExprKind::EnumConstruct { .. } => {}
                ExprKind::GlobalVarSet { value, .. } => op_child(*value, &mut f),
                ExprKind::Binary { left, right, .. } => {
                    op_child(*left, &mut f);
                    op_child(*right, &mut f);
                }
                ExprKind::Unary { expr, .. }
                | ExprKind::Cast { expr, .. }
                | ExprKind::FieldAccess { expr, .. }
                | ExprKind::VariantTag { expr }
                | ExprKind::VariantTest { expr, .. }
                | ExprKind::VariantPayload { expr, .. } => op_child(*expr, &mut f),
                ExprKind::Assign { target, value } => {
                    f(NodeRef::Expr(*target));
                    op_child(*value, &mut f);
                }
                ExprKind::Index { expr, index } => {
                    op_child(*expr, &mut f);
                    op_child(*index, &mut f);
                }
                ExprKind::Call { args, .. } => {
                    for a in args {
                        op_child(a.expr, &mut f);
                    }
                }
                ExprKind::CmRawCall { args, .. } => {
                    for a in args {
                        op_child(*a, &mut f);
                    }
                }
                ExprKind::MethodCall { receiver, args, .. } => {
                    op_child(*receiver, &mut f);
                    for a in args {
                        op_child(a.expr, &mut f);
                    }
                }
                ExprKind::Block(b) => f(NodeRef::Block(*b)),
                ExprKind::If {
                    condition,
                    then_branch,
                    else_branch,
                } => {
                    op_child(*condition, &mut f);
                    f(NodeRef::Block(*then_branch));
                    if let Some(b) = else_branch {
                        f(NodeRef::Block(*b));
                    }
                }
                ExprKind::Match { expr, arms } => {
                    op_child(*expr, &mut f);
                    for arm in arms {
                        f(NodeRef::Pat(arm.pattern));
                        if let Some(g) = arm.guard {
                            op_child(g, &mut f);
                        }
                        op_child(arm.body, &mut f);
                    }
                }
                ExprKind::StructLiteral { fields, .. } => {
                    for fld in fields {
                        op_child(fld.value, &mut f);
                    }
                }
                ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                    for el in elements {
                        op_child(*el, &mut f);
                    }
                }
                ExprKind::IndirectCall { callee, args } => {
                    op_child(*callee, &mut f);
                    for a in args {
                        op_child(*a, &mut f);
                    }
                }
                ExprKind::ClosureToCanonical { functor, .. } => op_child(*functor, &mut f),
                ExprKind::VariantConstruct { payload, .. } => {
                    if let Some(p) = payload {
                        op_child(*p, &mut f);
                    }
                }
                ExprKind::LabeledBlock { block, .. } => f(NodeRef::Block(*block)),
                ExprKind::Switch {
                    scrutinee,
                    arms,
                    default,
                    ..
                } => {
                    op_child(*scrutinee, &mut f);
                    for a in arms {
                        f(NodeRef::Block(*a));
                    }
                    f(NodeRef::Block(*default));
                }
            },
        }
    }
}
