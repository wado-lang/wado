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
use crate::nir::{FunctionRef, NirBinaryOp, NirLocal, NirUnaryOp};
use crate::tir::TypeId;
use crate::token::Span;

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
    pub expr: ExprId,
    pub is_mut: bool,
}

/// A struct-literal field.
#[derive(Debug, Clone)]
pub struct ArenaStructField {
    pub name: String,
    pub value: ExprId,
    pub field_index: u32,
}

/// A match arm.
#[derive(Debug, Clone)]
pub struct ArmData {
    pub pattern: PatId,
    pub guard: Option<ExprId>,
    pub body: ExprId,
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
    IntLiteral {
        value: u64,
        repr: String,
    },
    FloatLiteral {
        value: f64,
        repr: String,
    },
    BoolLiteral(bool),
    CharLiteral(char),
    StringLiteral(String),
    BytesLiteral(Vec<u8>),
    Null,
    Unit,
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
        value: ExprId,
    },
    Binary {
        left: ExprId,
        op: NirBinaryOp,
        right: ExprId,
    },
    Unary {
        op: NirUnaryOp,
        expr: ExprId,
    },
    Assign {
        target: ExprId,
        value: ExprId,
    },
    Cast {
        expr: ExprId,
        target_type: TypeId,
    },
    Call {
        func: FunctionRef,
        type_args: Vec<TypeId>,
        args: Vec<ArenaCallArg>,
    },
    CmRawCall {
        local_name: String,
        args: Vec<ExprId>,
    },
    MethodCall {
        receiver: ExprId,
        func: FunctionRef,
        type_args: Vec<TypeId>,
        args: Vec<ArenaCallArg>,
    },
    FieldAccess {
        expr: ExprId,
        field_index: u32,
        field_name: String,
    },
    Index {
        expr: ExprId,
        index: ExprId,
    },
    Block(BlockId),
    If {
        condition: ExprId,
        then_branch: BlockId,
        else_branch: Option<BlockId>,
    },
    Match {
        expr: ExprId,
        arms: Vec<ArmData>,
    },
    StructLiteral {
        struct_type: TypeId,
        struct_name: String,
        fields: Vec<ArenaStructField>,
    },
    TupleLiteral {
        elements: Vec<ExprId>,
    },
    ArrayLiteral {
        elements: Vec<ExprId>,
    },
    IndirectCall {
        callee: ExprId,
        args: Vec<ExprId>,
    },
    ClosureToCanonical {
        functor: ExprId,
        functor_id: u32,
        target_fn_type: TypeId,
        closure_module: crate::module_source::ModuleSource,
    },
    VariantConstruct {
        variant_type: TypeId,
        case_index: u32,
        case_name: String,
        payload: Option<ExprId>,
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
        expr: ExprId,
    },
    VariantTest {
        expr: ExprId,
        case_index: u32,
        case_name: String,
    },
    VariantPayload {
        expr: ExprId,
        case_index: u32,
        payload_type: TypeId,
    },
    Switch {
        scrutinee: ExprId,
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
        value: ExprId,
        skip_value_copy: bool,
    },
    Expr(ExprId),
    Return {
        value: Option<ExprId>,
    },
    If {
        condition: ExprId,
        then_block: BlockId,
        else_block: Option<BlockId>,
    },
    Loop {
        body: BlockId,
    },
    Break {
        label: Option<String>,
        value: Option<ExprId>,
    },
    Continue,
    LabeledBlock {
        label: String,
        block: BlockId,
    },
    LetDestructure {
        pattern: PatId,
        is_mut: bool,
        value: ExprId,
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
        expr: ExprId,
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
}

impl Body {
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
        }
    }

    /// A working copy that clones the node maps (`exprs` / `stmts` / `blocks` /
    /// `pats`) and `root` but drops the function-level metadata (`locals`,
    /// `address_taken_locals`, `stores_aliased_locals`). For read-only or
    /// in-place-reduce scratch use where that metadata is never consulted — e.g.
    /// niri's CTFE evaluator, which mutates the cloned node maps but reads only
    /// nodes. Node ids are preserved, so an id taken from `self` stays valid in
    /// the returned body. Cheaper than a full `clone()` when the dropped
    /// metadata is non-trivial (a real function body's `locals`) and unused.
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
            kind: StmtKind::Expr(e),
            span,
        });
        body.root = body.blocks.push(BlockNode {
            stmts: vec![s],
            span,
        });
        body
    }

    /// The expression id of a body that wraps a single expression — the form
    /// global initializers take, whose root block holds exactly one `Expr`
    /// statement. Panics if the body is not in that shape.
    pub fn sole_expr(&self) -> ExprId {
        let block = &self.blocks[self.root];
        assert_eq!(
            block.stmts.len(),
            1,
            "expr-wrapper body must hold exactly one statement"
        );
        match self.stmts[block.stmts[0]].kind {
            StmtKind::Expr(e) => e,
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

    /// The wrapped expression's id.
    pub fn expr(&self) -> ExprId {
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
                value: self.clone_expr(value),
            },
            ExprKind::Binary { left, op, right } => ExprKind::Binary {
                left: self.clone_expr(left),
                op,
                right: self.clone_expr(right),
            },
            ExprKind::Unary { op, expr } => ExprKind::Unary {
                op,
                expr: self.clone_expr(expr),
            },
            ExprKind::Assign { target, value } => ExprKind::Assign {
                target: self.clone_expr(target),
                value: self.clone_expr(value),
            },
            ExprKind::Cast { expr, target_type } => ExprKind::Cast {
                expr: self.clone_expr(expr),
                target_type,
            },
            ExprKind::Call {
                func,
                type_args,
                args,
            } => ExprKind::Call {
                func,
                type_args,
                args: args
                    .into_iter()
                    .map(|a| ArenaCallArg {
                        expr: self.clone_expr(a.expr),
                        is_mut: a.is_mut,
                    })
                    .collect(),
            },
            ExprKind::CmRawCall { local_name, args } => ExprKind::CmRawCall {
                local_name,
                args: args.into_iter().map(|a| self.clone_expr(a)).collect(),
            },
            ExprKind::MethodCall {
                receiver,
                func,
                type_args,
                args,
            } => ExprKind::MethodCall {
                receiver: self.clone_expr(receiver),
                func,
                type_args,
                args: args
                    .into_iter()
                    .map(|a| ArenaCallArg {
                        expr: self.clone_expr(a.expr),
                        is_mut: a.is_mut,
                    })
                    .collect(),
            },
            ExprKind::FieldAccess {
                expr,
                field_index,
                field_name,
            } => ExprKind::FieldAccess {
                expr: self.clone_expr(expr),
                field_index,
                field_name,
            },
            ExprKind::Index { expr, index } => ExprKind::Index {
                expr: self.clone_expr(expr),
                index: self.clone_expr(index),
            },
            ExprKind::Block(b) => ExprKind::Block(self.clone_block(b)),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => ExprKind::If {
                condition: self.clone_expr(condition),
                then_branch: self.clone_block(then_branch),
                else_branch: else_branch.map(|b| self.clone_block(b)),
            },
            ExprKind::Match { expr, arms } => ExprKind::Match {
                expr: self.clone_expr(expr),
                arms: arms
                    .into_iter()
                    .map(|a| ArmData {
                        pattern: self.clone_pat(a.pattern),
                        guard: a.guard.map(|g| self.clone_expr(g)),
                        body: self.clone_expr(a.body),
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
                        value: self.clone_expr(f.value),
                        field_index: f.field_index,
                    })
                    .collect(),
            },
            ExprKind::TupleLiteral { elements } => ExprKind::TupleLiteral {
                elements: elements.into_iter().map(|e| self.clone_expr(e)).collect(),
            },
            ExprKind::ArrayLiteral { elements } => ExprKind::ArrayLiteral {
                elements: elements.into_iter().map(|e| self.clone_expr(e)).collect(),
            },
            ExprKind::IndirectCall { callee, args } => ExprKind::IndirectCall {
                callee: self.clone_expr(callee),
                args: args.into_iter().map(|a| self.clone_expr(a)).collect(),
            },
            ExprKind::ClosureToCanonical {
                functor,
                functor_id,
                target_fn_type,
                closure_module,
            } => ExprKind::ClosureToCanonical {
                functor: self.clone_expr(functor),
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
                payload: payload.map(|p| self.clone_expr(p)),
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
                expr: self.clone_expr(expr),
            },
            ExprKind::VariantTest {
                expr,
                case_index,
                case_name,
            } => ExprKind::VariantTest {
                expr: self.clone_expr(expr),
                case_index,
                case_name,
            },
            ExprKind::VariantPayload {
                expr,
                case_index,
                payload_type,
            } => ExprKind::VariantPayload {
                expr: self.clone_expr(expr),
                case_index,
                payload_type,
            },
            ExprKind::Switch {
                scrutinee,
                min_value,
                arms,
                default,
            } => ExprKind::Switch {
                scrutinee: self.clone_expr(scrutinee),
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
                value: self.clone_expr(value),
                skip_value_copy,
            },
            StmtKind::Expr(e) => StmtKind::Expr(self.clone_expr(e)),
            StmtKind::Return { value } => StmtKind::Return {
                value: value.map(|e| self.clone_expr(e)),
            },
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => StmtKind::If {
                condition: self.clone_expr(condition),
                then_block: self.clone_block(then_block),
                else_block: else_block.map(|b| self.clone_block(b)),
            },
            StmtKind::Loop { body } => StmtKind::Loop {
                body: self.clone_block(body),
            },
            StmtKind::Break { label, value } => StmtKind::Break {
                label,
                value: value.map(|e| self.clone_expr(e)),
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
                value: self.clone_expr(value),
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
                expr: self.clone_expr(expr),
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
    pub fn for_each_child(&self, node: NodeRef, mut f: impl FnMut(NodeRef)) {
        match node {
            NodeRef::Block(b) => {
                for s in &self.blocks[b].stmts {
                    f(NodeRef::Stmt(*s));
                }
            }
            NodeRef::Stmt(s) => match &self.stmts[s].kind {
                StmtKind::Let { value, .. } => f(NodeRef::Expr(*value)),
                StmtKind::Expr(e) => f(NodeRef::Expr(*e)),
                StmtKind::Return { value } => {
                    if let Some(e) = value {
                        f(NodeRef::Expr(*e));
                    }
                }
                StmtKind::If {
                    condition,
                    then_block,
                    else_block,
                } => {
                    f(NodeRef::Expr(*condition));
                    f(NodeRef::Block(*then_block));
                    if let Some(b) = else_block {
                        f(NodeRef::Block(*b));
                    }
                }
                StmtKind::Loop { body } => f(NodeRef::Block(*body)),
                StmtKind::Break { value, .. } => {
                    if let Some(e) = value {
                        f(NodeRef::Expr(*e));
                    }
                }
                StmtKind::Continue => {}
                StmtKind::LabeledBlock { block, .. } => f(NodeRef::Block(*block)),
                StmtKind::LetDestructure { pattern, value, .. } => {
                    f(NodeRef::Pat(*pattern));
                    f(NodeRef::Expr(*value));
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
                PatKind::ConstantValue { expr } => f(NodeRef::Expr(*expr)),
            },
            NodeRef::Expr(e) => match &self.exprs[e].kind {
                ExprKind::IntLiteral { .. }
                | ExprKind::FloatLiteral { .. }
                | ExprKind::BoolLiteral(_)
                | ExprKind::CharLiteral(_)
                | ExprKind::StringLiteral(_)
                | ExprKind::BytesLiteral(_)
                | ExprKind::Null
                | ExprKind::Unit
                | ExprKind::Local { .. }
                | ExprKind::GlobalVarGet { .. }
                | ExprKind::EnumConstruct { .. } => {}
                ExprKind::GlobalVarSet { value, .. } => f(NodeRef::Expr(*value)),
                ExprKind::Binary { left, right, .. } => {
                    f(NodeRef::Expr(*left));
                    f(NodeRef::Expr(*right));
                }
                ExprKind::Unary { expr, .. }
                | ExprKind::Cast { expr, .. }
                | ExprKind::FieldAccess { expr, .. }
                | ExprKind::VariantTag { expr }
                | ExprKind::VariantTest { expr, .. }
                | ExprKind::VariantPayload { expr, .. } => f(NodeRef::Expr(*expr)),
                ExprKind::Assign { target, value } => {
                    f(NodeRef::Expr(*target));
                    f(NodeRef::Expr(*value));
                }
                ExprKind::Index { expr, index } => {
                    f(NodeRef::Expr(*expr));
                    f(NodeRef::Expr(*index));
                }
                ExprKind::Call { args, .. } => {
                    for a in args {
                        f(NodeRef::Expr(a.expr));
                    }
                }
                ExprKind::CmRawCall { args, .. } => {
                    for a in args {
                        f(NodeRef::Expr(*a));
                    }
                }
                ExprKind::MethodCall { receiver, args, .. } => {
                    f(NodeRef::Expr(*receiver));
                    for a in args {
                        f(NodeRef::Expr(a.expr));
                    }
                }
                ExprKind::Block(b) => f(NodeRef::Block(*b)),
                ExprKind::If {
                    condition,
                    then_branch,
                    else_branch,
                } => {
                    f(NodeRef::Expr(*condition));
                    f(NodeRef::Block(*then_branch));
                    if let Some(b) = else_branch {
                        f(NodeRef::Block(*b));
                    }
                }
                ExprKind::Match { expr, arms } => {
                    f(NodeRef::Expr(*expr));
                    for arm in arms {
                        f(NodeRef::Pat(arm.pattern));
                        if let Some(g) = arm.guard {
                            f(NodeRef::Expr(g));
                        }
                        f(NodeRef::Expr(arm.body));
                    }
                }
                ExprKind::StructLiteral { fields, .. } => {
                    for fld in fields {
                        f(NodeRef::Expr(fld.value));
                    }
                }
                ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                    for el in elements {
                        f(NodeRef::Expr(*el));
                    }
                }
                ExprKind::IndirectCall { callee, args } => {
                    f(NodeRef::Expr(*callee));
                    for a in args {
                        f(NodeRef::Expr(*a));
                    }
                }
                ExprKind::ClosureToCanonical { functor, .. } => f(NodeRef::Expr(*functor)),
                ExprKind::VariantConstruct { payload, .. } => {
                    if let Some(p) = payload {
                        f(NodeRef::Expr(*p));
                    }
                }
                ExprKind::LabeledBlock { block, .. } => f(NodeRef::Block(*block)),
                ExprKind::Switch {
                    scrutinee,
                    arms,
                    default,
                    ..
                } => {
                    f(NodeRef::Expr(*scrutinee));
                    for a in arms {
                        f(NodeRef::Block(*a));
                    }
                    f(NodeRef::Block(*default));
                }
            },
        }
    }
}
