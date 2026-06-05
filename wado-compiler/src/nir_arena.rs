//! NIR skeleton arena (Layer 1).
//!
//! A per-function arena representation of a NIR body. Where `nir.rs` nests
//! children through `Box<NirExpr>` / `Vec<NirStmt>`, this stores every node in
//! a `PrimaryMap` keyed by a typed id (`ExprId` / `StmtId` / `BlockId` /
//! `PatId`) and references children by id. This is the substrate the
//! worklist rewrite engine needs: stable handles that survive in-place edits.
//!
//! Phase 1 (see `docs/wep-2026-06-05-nir-skeleton-arena.md`) provides only the
//! representation and the tree <-> arena converters. The parent map, the local
//! use index, and the mutating edit API land in a later phase, with the engine
//! that consumes them.

use cranelift_entity::{PrimaryMap, entity_impl};

use crate::hashmap::IndexSet;
use crate::nir::{
    CallArg, FunctionRef, NirBinaryOp, NirBlock, NirExpr, NirExprKind, NirLocal,
    NirMatchArm, NirPattern, NirStmt, NirStmtKind, NirStructField, NirStructPatternField,
    NirUnaryOp,
};
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

/// An expression node: the arena counterpart of [`NirExpr`].
#[derive(Debug, Clone)]
pub struct ExprNode {
    pub kind: ExprKind,
    pub type_id: TypeId,
    pub span: Span,
}

/// A statement node: the arena counterpart of [`NirStmt`].
#[derive(Debug, Clone)]
pub struct StmtNode {
    pub kind: StmtKind,
    pub span: Span,
}

/// A block node: the arena counterpart of [`NirBlock`].
#[derive(Debug, Clone)]
pub struct BlockNode {
    pub stmts: Vec<StmtId>,
    pub span: Span,
}

/// A pattern node: the arena counterpart of [`NirPattern`].
#[derive(Debug, Clone)]
pub struct PatNode {
    pub kind: PatKind,
    pub span: Span,
}

/// A call argument with its parameter mutability flag (arena form of
/// [`CallArg`]).
#[derive(Debug, Clone)]
pub struct ArenaCallArg {
    pub expr: ExprId,
    pub is_mut: bool,
}

/// A struct-literal field (arena form of [`NirStructField`]).
#[derive(Debug, Clone)]
pub struct ArenaStructField {
    pub name: String,
    pub value: ExprId,
    pub field_index: u32,
}

/// A match arm (arena form of [`NirMatchArm`]).
#[derive(Debug, Clone)]
pub struct ArmData {
    pub pattern: PatId,
    pub guard: Option<ExprId>,
    pub body: ExprId,
    pub span: Span,
}

/// A struct destructuring field (arena form of [`NirStructPatternField`]).
#[derive(Debug, Clone)]
pub struct ArenaStructPatternField {
    pub field_name: String,
    pub field_index: u32,
    pub pattern: PatId,
}

/// Arena counterpart of [`NirExprKind`]: identical leaf data, children by id.
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

/// Arena counterpart of [`NirStmtKind`].
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

/// Arena counterpart of [`NirPattern`]. Leaf payloads (`NirLiteralPattern`,
/// range bounds) are reused as-is; nested patterns and the `ConstantValue`
/// expression become ids.
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
    /// Build a `Body` from a standalone block (e.g. a global initializer or a
    /// test fixture); function-level fact sets are left empty.
    pub fn from_block(block: &NirBlock) -> Self {
        let mut b = Lower::default();
        let root = b.block(block);
        Self {
            exprs: b.exprs,
            stmts: b.stmts,
            blocks: b.blocks,
            pats: b.pats,
            root,
            locals: Vec::new(),
            address_taken_locals: IndexSet::default(),
            stores_aliased_locals: IndexSet::default(),
        }
    }

    /// Reconstruct the owned-tree block from the arena.
    pub fn to_block(&self) -> NirBlock {
        self.block_to_tree(self.root)
    }
}

/// Tree -> arena lowering. Pushes children before parents so ids are dense and
/// well-formed.
#[derive(Default)]
struct Lower {
    exprs: PrimaryMap<ExprId, ExprNode>,
    stmts: PrimaryMap<StmtId, StmtNode>,
    blocks: PrimaryMap<BlockId, BlockNode>,
    pats: PrimaryMap<PatId, PatNode>,
}

impl Lower {
    fn block(&mut self, block: &NirBlock) -> BlockId {
        let stmts = block.stmts.iter().map(|s| self.stmt(s)).collect();
        self.blocks.push(BlockNode {
            stmts,
            span: block.span,
        })
    }

    fn opt_block(&mut self, block: &Option<NirBlock>) -> Option<BlockId> {
        block.as_ref().map(|b| self.block(b))
    }

    fn arg(&mut self, arg: &CallArg) -> ArenaCallArg {
        ArenaCallArg {
            expr: self.expr(&arg.expr),
            is_mut: arg.is_mut,
        }
    }

    fn expr(&mut self, expr: &NirExpr) -> ExprId {
        let kind = self.expr_kind(&expr.kind);
        self.exprs.push(ExprNode {
            kind,
            type_id: expr.type_id,
            span: expr.span,
        })
    }

    fn opt_expr(&mut self, expr: &Option<Box<NirExpr>>) -> Option<ExprId> {
        expr.as_ref().map(|e| self.expr(e))
    }

    fn expr_kind(&mut self, kind: &NirExprKind) -> ExprKind {
        match kind {
            NirExprKind::IntLiteral { value, repr } => ExprKind::IntLiteral {
                value: *value,
                repr: repr.clone(),
            },
            NirExprKind::FloatLiteral { value, repr } => ExprKind::FloatLiteral {
                value: *value,
                repr: repr.clone(),
            },
            NirExprKind::BoolLiteral(v) => ExprKind::BoolLiteral(*v),
            NirExprKind::CharLiteral(v) => ExprKind::CharLiteral(*v),
            NirExprKind::StringLiteral(v) => ExprKind::StringLiteral(v.clone()),
            NirExprKind::BytesLiteral(v) => ExprKind::BytesLiteral(v.clone()),
            NirExprKind::Null => ExprKind::Null,
            NirExprKind::Unit => ExprKind::Unit,
            NirExprKind::Local { index, name } => ExprKind::Local {
                index: *index,
                name: name.clone(),
            },
            NirExprKind::GlobalVarGet {
                module_source,
                name,
            } => ExprKind::GlobalVarGet {
                module_source: module_source.clone(),
                name: name.clone(),
            },
            NirExprKind::GlobalVarSet {
                module_source,
                name,
                value,
            } => ExprKind::GlobalVarSet {
                module_source: module_source.clone(),
                name: name.clone(),
                value: self.expr(value),
            },
            NirExprKind::Binary { left, op, right } => ExprKind::Binary {
                left: self.expr(left),
                op: *op,
                right: self.expr(right),
            },
            NirExprKind::Unary { op, expr } => ExprKind::Unary {
                op: *op,
                expr: self.expr(expr),
            },
            NirExprKind::Assign { target, value } => ExprKind::Assign {
                target: self.expr(target),
                value: self.expr(value),
            },
            NirExprKind::Cast { expr, target_type } => ExprKind::Cast {
                expr: self.expr(expr),
                target_type: *target_type,
            },
            NirExprKind::Call {
                func,
                type_args,
                args,
            } => ExprKind::Call {
                func: func.clone(),
                type_args: type_args.clone(),
                args: args.iter().map(|a| self.arg(a)).collect(),
            },
            NirExprKind::CmRawCall { local_name, args } => ExprKind::CmRawCall {
                local_name: local_name.clone(),
                args: args.iter().map(|a| self.expr(a)).collect(),
            },
            NirExprKind::MethodCall {
                receiver,
                func,
                type_args,
                args,
                ..
            } => ExprKind::MethodCall {
                receiver: self.expr(receiver),
                func: func.clone(),
                type_args: type_args.clone(),
                args: args.iter().map(|a| self.arg(a)).collect(),
            },
            NirExprKind::FieldAccess {
                expr,
                field_index,
                field_name,
            } => ExprKind::FieldAccess {
                expr: self.expr(expr),
                field_index: *field_index,
                field_name: field_name.clone(),
            },
            NirExprKind::Index { expr, index } => ExprKind::Index {
                expr: self.expr(expr),
                index: self.expr(index),
            },
            NirExprKind::Block(block) => ExprKind::Block(self.block(block)),
            NirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => ExprKind::If {
                condition: self.expr(condition),
                then_branch: self.block(then_branch),
                else_branch: self.opt_block(else_branch),
            },
            NirExprKind::Match { expr, arms } => ExprKind::Match {
                expr: self.expr(expr),
                arms: arms.iter().map(|a| self.arm(a)).collect(),
            },
            NirExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields,
            } => ExprKind::StructLiteral {
                struct_type: *struct_type,
                struct_name: struct_name.clone(),
                fields: fields
                    .iter()
                    .map(|f| ArenaStructField {
                        name: f.name.clone(),
                        value: self.expr(&f.value),
                        field_index: f.field_index,
                    })
                    .collect(),
            },
            NirExprKind::TupleLiteral { elements } => ExprKind::TupleLiteral {
                elements: elements.iter().map(|e| self.expr(e)).collect(),
            },
            NirExprKind::ArrayLiteral { elements } => ExprKind::ArrayLiteral {
                elements: elements.iter().map(|e| self.expr(e)).collect(),
            },
            NirExprKind::IndirectCall { callee, args } => ExprKind::IndirectCall {
                callee: self.expr(callee),
                args: args.iter().map(|a| self.expr(a)).collect(),
            },
            NirExprKind::ClosureToCanonical {
                functor,
                functor_id,
                target_fn_type,
                closure_module,
            } => ExprKind::ClosureToCanonical {
                functor: self.expr(functor),
                functor_id: *functor_id,
                target_fn_type: *target_fn_type,
                closure_module: closure_module.clone(),
            },
            NirExprKind::VariantConstruct {
                variant_type,
                case_index,
                case_name,
                payload,
            } => ExprKind::VariantConstruct {
                variant_type: *variant_type,
                case_index: *case_index,
                case_name: case_name.clone(),
                payload: self.opt_expr(payload),
            },
            NirExprKind::EnumConstruct {
                enum_type,
                case_index,
                case_name,
            } => ExprKind::EnumConstruct {
                enum_type: *enum_type,
                case_index: *case_index,
                case_name: case_name.clone(),
            },
            NirExprKind::LabeledBlock {
                label,
                block,
                result_type,
            } => ExprKind::LabeledBlock {
                label: label.clone(),
                block: self.block(block),
                result_type: *result_type,
            },
            NirExprKind::VariantTag { expr } => ExprKind::VariantTag {
                expr: self.expr(expr),
            },
            NirExprKind::VariantTest {
                expr,
                case_index,
                case_name,
            } => ExprKind::VariantTest {
                expr: self.expr(expr),
                case_index: *case_index,
                case_name: case_name.clone(),
            },
            NirExprKind::VariantPayload {
                expr,
                case_index,
                payload_type,
            } => ExprKind::VariantPayload {
                expr: self.expr(expr),
                case_index: *case_index,
                payload_type: *payload_type,
            },
            NirExprKind::Switch {
                scrutinee,
                min_value,
                arms,
                default,
            } => ExprKind::Switch {
                scrutinee: self.expr(scrutinee),
                min_value: *min_value,
                arms: arms.iter().map(|a| self.block(a)).collect(),
                default: self.block(default),
            },
        }
    }

    fn arm(&mut self, arm: &NirMatchArm) -> ArmData {
        ArmData {
            pattern: self.pat(&arm.pattern),
            guard: arm.guard.as_ref().map(|g| self.expr(g)),
            body: self.expr(&arm.body),
            span: arm.span,
        }
    }

    fn stmt(&mut self, stmt: &NirStmt) -> StmtId {
        let kind = self.stmt_kind(&stmt.kind);
        self.stmts.push(StmtNode {
            kind,
            span: stmt.span,
        })
    }

    fn stmt_kind(&mut self, kind: &NirStmtKind) -> StmtKind {
        match kind {
            NirStmtKind::Let {
                name,
                local_index,
                is_mut,
                is_reactive,
                type_id,
                value,
                skip_value_copy,
            } => StmtKind::Let {
                name: name.clone(),
                local_index: *local_index,
                is_mut: *is_mut,
                is_reactive: *is_reactive,
                type_id: *type_id,
                value: self.expr(value),
                skip_value_copy: *skip_value_copy,
            },
            NirStmtKind::Expr(e) => StmtKind::Expr(self.expr(e)),
            NirStmtKind::Return { value } => StmtKind::Return {
                value: value.as_ref().map(|v| self.expr(v)),
            },
            NirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => StmtKind::If {
                condition: self.expr(condition),
                then_block: self.block(then_block),
                else_block: self.opt_block(else_block),
            },
            NirStmtKind::Loop { body } => StmtKind::Loop {
                body: self.block(body),
            },
            NirStmtKind::Break { label, value } => StmtKind::Break {
                label: label.clone(),
                value: value.as_ref().map(|v| self.expr(v)),
            },
            NirStmtKind::Continue => StmtKind::Continue,
            NirStmtKind::LabeledBlock { label, block } => StmtKind::LabeledBlock {
                label: label.clone(),
                block: self.block(block),
            },
            NirStmtKind::LetDestructure {
                pattern,
                is_mut,
                value,
            } => StmtKind::LetDestructure {
                pattern: self.pat(pattern),
                is_mut: *is_mut,
                value: self.expr(value),
            },
        }
    }

    fn pat(&mut self, pat: &NirPattern) -> PatId {
        // Patterns carry no span of their own in the tree; arena pattern nodes
        // reuse the surrounding default span so the round-trip is lossless on
        // the data that exists.
        let kind = self.pat_kind(pat);
        self.pats.push(PatNode {
            kind,
            span: Span::default(),
        })
    }

    fn pat_kind(&mut self, pat: &NirPattern) -> PatKind {
        match pat {
            NirPattern::Wildcard => PatKind::Wildcard,
            NirPattern::Binding {
                name,
                local_index,
                type_id,
            } => PatKind::Binding {
                name: name.clone(),
                local_index: *local_index,
                type_id: *type_id,
            },
            NirPattern::Literal(lit) => PatKind::Literal(lit.clone()),
            NirPattern::Tuple(pats, has_rest) => {
                PatKind::Tuple(pats.iter().map(|p| self.pat(p)).collect(), *has_rest)
            }
            NirPattern::Variant {
                enum_type,
                variant_name,
                bindings,
                payload_type,
            } => PatKind::Variant {
                enum_type: *enum_type,
                variant_name: variant_name.clone(),
                bindings: bindings.iter().map(|p| self.pat(p)).collect(),
                payload_type: *payload_type,
            },
            NirPattern::Enum {
                enum_type,
                case_name,
                case_index,
            } => PatKind::Enum {
                enum_type: *enum_type,
                case_name: case_name.clone(),
                case_index: *case_index,
            },
            NirPattern::Struct {
                struct_type,
                fields,
                has_rest,
            } => PatKind::Struct {
                struct_type: *struct_type,
                fields: fields
                    .iter()
                    .map(|f| ArenaStructPatternField {
                        field_name: f.field_name.clone(),
                        field_index: f.field_index,
                        pattern: self.pat(&f.pattern),
                    })
                    .collect(),
                has_rest: *has_rest,
            },
            NirPattern::Or(alts) => PatKind::Or(alts.iter().map(|p| self.pat(p)).collect()),
            NirPattern::ConstantValue { expr } => PatKind::ConstantValue {
                expr: self.expr(expr),
            },
            NirPattern::Range {
                start,
                end,
                inclusive,
                is_unsigned,
            } => PatKind::Range {
                start: *start,
                end: *end,
                inclusive: *inclusive,
                is_unsigned: *is_unsigned,
            },
        }
    }
}

/// Arena -> tree lowering.
impl Body {
    fn block_to_tree(&self, id: BlockId) -> NirBlock {
        let node = &self.blocks[id];
        NirBlock {
            stmts: node.stmts.iter().map(|s| self.stmt_to_tree(*s)).collect(),
            span: node.span,
        }
    }

    fn opt_block_to_tree(&self, id: Option<BlockId>) -> Option<NirBlock> {
        id.map(|b| self.block_to_tree(b))
    }

    fn expr_to_tree(&self, id: ExprId) -> NirExpr {
        let node = &self.exprs[id];
        NirExpr {
            kind: self.expr_kind_to_tree(&node.kind),
            type_id: node.type_id,
            span: node.span,
        }
    }

    fn boxed(&self, id: ExprId) -> Box<NirExpr> {
        Box::new(self.expr_to_tree(id))
    }

    fn arg_to_tree(&self, arg: &ArenaCallArg) -> CallArg {
        CallArg {
            expr: self.expr_to_tree(arg.expr),
            is_mut: arg.is_mut,
        }
    }

    fn expr_kind_to_tree(&self, kind: &ExprKind) -> NirExprKind {
        match kind {
            ExprKind::IntLiteral { value, repr } => NirExprKind::IntLiteral {
                value: *value,
                repr: repr.clone(),
            },
            ExprKind::FloatLiteral { value, repr } => NirExprKind::FloatLiteral {
                value: *value,
                repr: repr.clone(),
            },
            ExprKind::BoolLiteral(v) => NirExprKind::BoolLiteral(*v),
            ExprKind::CharLiteral(v) => NirExprKind::CharLiteral(*v),
            ExprKind::StringLiteral(v) => NirExprKind::StringLiteral(v.clone()),
            ExprKind::BytesLiteral(v) => NirExprKind::BytesLiteral(v.clone()),
            ExprKind::Null => NirExprKind::Null,
            ExprKind::Unit => NirExprKind::Unit,
            ExprKind::Local { index, name } => NirExprKind::Local {
                index: *index,
                name: name.clone(),
            },
            ExprKind::GlobalVarGet {
                module_source,
                name,
            } => NirExprKind::GlobalVarGet {
                module_source: module_source.clone(),
                name: name.clone(),
            },
            ExprKind::GlobalVarSet {
                module_source,
                name,
                value,
            } => NirExprKind::GlobalVarSet {
                module_source: module_source.clone(),
                name: name.clone(),
                value: self.boxed(*value),
            },
            ExprKind::Binary { left, op, right } => NirExprKind::Binary {
                left: self.boxed(*left),
                op: *op,
                right: self.boxed(*right),
            },
            ExprKind::Unary { op, expr } => NirExprKind::Unary {
                op: *op,
                expr: self.boxed(*expr),
            },
            ExprKind::Assign { target, value } => NirExprKind::Assign {
                target: self.boxed(*target),
                value: self.boxed(*value),
            },
            ExprKind::Cast { expr, target_type } => NirExprKind::Cast {
                expr: self.boxed(*expr),
                target_type: *target_type,
            },
            ExprKind::Call {
                func,
                type_args,
                args,
            } => NirExprKind::Call {
                func: func.clone(),
                type_args: type_args.clone(),
                args: args.iter().map(|a| self.arg_to_tree(a)).collect(),
            },
            ExprKind::CmRawCall { local_name, args } => NirExprKind::CmRawCall {
                local_name: local_name.clone(),
                args: args.iter().map(|a| self.expr_to_tree(*a)).collect(),
            },
            ExprKind::MethodCall {
                receiver,
                func,
                type_args,
                args,
            } => NirExprKind::method_call(
                self.boxed(*receiver),
                func.clone(),
                type_args.clone(),
                args.iter().map(|a| self.arg_to_tree(a)).collect(),
            ),
            ExprKind::FieldAccess {
                expr,
                field_index,
                field_name,
            } => NirExprKind::FieldAccess {
                expr: self.boxed(*expr),
                field_index: *field_index,
                field_name: field_name.clone(),
            },
            ExprKind::Index { expr, index } => NirExprKind::Index {
                expr: self.boxed(*expr),
                index: self.boxed(*index),
            },
            ExprKind::Block(block) => NirExprKind::Block(self.block_to_tree(*block)),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => NirExprKind::If {
                condition: self.boxed(*condition),
                then_branch: self.block_to_tree(*then_branch),
                else_branch: self.opt_block_to_tree(*else_branch),
            },
            ExprKind::Match { expr, arms } => NirExprKind::Match {
                expr: self.boxed(*expr),
                arms: arms.iter().map(|a| self.arm_to_tree(a)).collect(),
            },
            ExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields,
            } => NirExprKind::StructLiteral {
                struct_type: *struct_type,
                struct_name: struct_name.clone(),
                fields: fields
                    .iter()
                    .map(|f| NirStructField {
                        name: f.name.clone(),
                        value: self.expr_to_tree(f.value),
                        field_index: f.field_index,
                    })
                    .collect(),
            },
            ExprKind::TupleLiteral { elements } => NirExprKind::TupleLiteral {
                elements: elements.iter().map(|e| self.expr_to_tree(*e)).collect(),
            },
            ExprKind::ArrayLiteral { elements } => NirExprKind::ArrayLiteral {
                elements: elements.iter().map(|e| self.expr_to_tree(*e)).collect(),
            },
            ExprKind::IndirectCall { callee, args } => NirExprKind::IndirectCall {
                callee: self.boxed(*callee),
                args: args.iter().map(|a| self.expr_to_tree(*a)).collect(),
            },
            ExprKind::ClosureToCanonical {
                functor,
                functor_id,
                target_fn_type,
                closure_module,
            } => NirExprKind::ClosureToCanonical {
                functor: self.boxed(*functor),
                functor_id: *functor_id,
                target_fn_type: *target_fn_type,
                closure_module: closure_module.clone(),
            },
            ExprKind::VariantConstruct {
                variant_type,
                case_index,
                case_name,
                payload,
            } => NirExprKind::VariantConstruct {
                variant_type: *variant_type,
                case_index: *case_index,
                case_name: case_name.clone(),
                payload: payload.map(|p| self.boxed(p)),
            },
            ExprKind::EnumConstruct {
                enum_type,
                case_index,
                case_name,
            } => NirExprKind::EnumConstruct {
                enum_type: *enum_type,
                case_index: *case_index,
                case_name: case_name.clone(),
            },
            ExprKind::LabeledBlock {
                label,
                block,
                result_type,
            } => NirExprKind::LabeledBlock {
                label: label.clone(),
                block: self.block_to_tree(*block),
                result_type: *result_type,
            },
            ExprKind::VariantTag { expr } => NirExprKind::VariantTag {
                expr: self.boxed(*expr),
            },
            ExprKind::VariantTest {
                expr,
                case_index,
                case_name,
            } => NirExprKind::VariantTest {
                expr: self.boxed(*expr),
                case_index: *case_index,
                case_name: case_name.clone(),
            },
            ExprKind::VariantPayload {
                expr,
                case_index,
                payload_type,
            } => NirExprKind::VariantPayload {
                expr: self.boxed(*expr),
                case_index: *case_index,
                payload_type: *payload_type,
            },
            ExprKind::Switch {
                scrutinee,
                min_value,
                arms,
                default,
            } => NirExprKind::Switch {
                scrutinee: self.boxed(*scrutinee),
                min_value: *min_value,
                arms: arms.iter().map(|a| self.block_to_tree(*a)).collect(),
                default: self.block_to_tree(*default),
            },
        }
    }

    fn arm_to_tree(&self, arm: &ArmData) -> NirMatchArm {
        NirMatchArm {
            pattern: self.pat_to_tree(arm.pattern),
            guard: arm.guard.map(|g| self.expr_to_tree(g)),
            body: self.expr_to_tree(arm.body),
            span: arm.span,
        }
    }

    fn stmt_to_tree(&self, id: StmtId) -> NirStmt {
        let node = &self.stmts[id];
        NirStmt {
            kind: self.stmt_kind_to_tree(&node.kind),
            span: node.span,
        }
    }

    fn stmt_kind_to_tree(&self, kind: &StmtKind) -> NirStmtKind {
        match kind {
            StmtKind::Let {
                name,
                local_index,
                is_mut,
                is_reactive,
                type_id,
                value,
                skip_value_copy,
            } => NirStmtKind::Let {
                name: name.clone(),
                local_index: *local_index,
                is_mut: *is_mut,
                is_reactive: *is_reactive,
                type_id: *type_id,
                value: self.expr_to_tree(*value),
                skip_value_copy: *skip_value_copy,
            },
            StmtKind::Expr(e) => NirStmtKind::Expr(self.expr_to_tree(*e)),
            StmtKind::Return { value } => NirStmtKind::Return {
                value: value.map(|v| self.expr_to_tree(v)),
            },
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => NirStmtKind::If {
                condition: self.expr_to_tree(*condition),
                then_block: self.block_to_tree(*then_block),
                else_block: self.opt_block_to_tree(*else_block),
            },
            StmtKind::Loop { body } => NirStmtKind::Loop {
                body: self.block_to_tree(*body),
            },
            StmtKind::Break { label, value } => NirStmtKind::Break {
                label: label.clone(),
                value: value.map(|v| self.expr_to_tree(v)),
            },
            StmtKind::Continue => NirStmtKind::Continue,
            StmtKind::LabeledBlock { label, block } => NirStmtKind::LabeledBlock {
                label: label.clone(),
                block: self.block_to_tree(*block),
            },
            StmtKind::LetDestructure {
                pattern,
                is_mut,
                value,
            } => NirStmtKind::LetDestructure {
                pattern: self.pat_to_tree(*pattern),
                is_mut: *is_mut,
                value: self.expr_to_tree(*value),
            },
        }
    }

    fn pat_to_tree(&self, id: PatId) -> NirPattern {
        match &self.pats[id].kind {
            PatKind::Wildcard => NirPattern::Wildcard,
            PatKind::Binding {
                name,
                local_index,
                type_id,
            } => NirPattern::Binding {
                name: name.clone(),
                local_index: *local_index,
                type_id: *type_id,
            },
            PatKind::Literal(lit) => NirPattern::Literal(lit.clone()),
            PatKind::Tuple(pats, has_rest) => NirPattern::Tuple(
                pats.iter().map(|p| self.pat_to_tree(*p)).collect(),
                *has_rest,
            ),
            PatKind::Variant {
                enum_type,
                variant_name,
                bindings,
                payload_type,
            } => NirPattern::Variant {
                enum_type: *enum_type,
                variant_name: variant_name.clone(),
                bindings: bindings.iter().map(|p| self.pat_to_tree(*p)).collect(),
                payload_type: *payload_type,
            },
            PatKind::Enum {
                enum_type,
                case_name,
                case_index,
            } => NirPattern::Enum {
                enum_type: *enum_type,
                case_name: case_name.clone(),
                case_index: *case_index,
            },
            PatKind::Struct {
                struct_type,
                fields,
                has_rest,
            } => NirPattern::Struct {
                struct_type: *struct_type,
                fields: fields
                    .iter()
                    .map(|f| NirStructPatternField {
                        field_name: f.field_name.clone(),
                        field_index: f.field_index,
                        pattern: self.pat_to_tree(f.pattern),
                    })
                    .collect(),
                has_rest: *has_rest,
            },
            PatKind::Or(alts) => {
                NirPattern::Or(alts.iter().map(|p| self.pat_to_tree(*p)).collect())
            }
            PatKind::ConstantValue { expr } => NirPattern::ConstantValue {
                expr: Box::new(self.expr_to_tree(*expr)),
            },
            PatKind::Range {
                start,
                end,
                inclusive,
                is_unsigned,
            } => NirPattern::Range {
                start: *start,
                end: *end,
                inclusive: *inclusive,
                is_unsigned: *is_unsigned,
            },
        }
    }
}
