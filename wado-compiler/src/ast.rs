// AST definitions for Wado

use crate::token::Span;

#[derive(Debug, Clone)]
pub struct Module {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone)]
pub enum Item {
    Use(UseDecl),
    Function(Function),
    Effect(EffectDecl),
    Struct(StructDecl),
    Record(RecordDecl),
    Enum(EnumDecl),
    Type(TypeAlias),
    Impl(ImplBlock),
}

#[derive(Debug, Clone)]
pub struct UseDecl {
    pub path: Vec<String>,
    pub items: Vec<String>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Function {
    pub name: String,
    pub is_pub: bool,
    pub params: Vec<Param>,
    pub return_type: Option<Type>,
    pub effects: Vec<String>,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub ty: Type,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Let(LetStmt),
    Expr(ExprStmt),
    Return(ReturnStmt),
    If(IfStmt),
    While(WhileStmt),
    For(ForStmt),
}

#[derive(Debug, Clone)]
pub struct LetStmt {
    pub name: String,
    pub is_mut: bool,
    pub is_reactive: bool,
    pub ty: Option<Type>,
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ExprStmt {
    pub expr: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ReturnStmt {
    pub value: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct IfStmt {
    pub condition: Expr,
    pub then_block: Block,
    pub else_block: Option<Block>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct WhileStmt {
    pub condition: Expr,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ForStmt {
    pub var: String,
    pub iter: Expr,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Ident(IdentExpr),
    Literal(LiteralExpr),
    Binary(Box<BinaryExpr>),
    Unary(Box<UnaryExpr>),
    Call(Box<CallExpr>),
    MethodCall(Box<MethodCallExpr>),
    FieldAccess(Box<FieldAccessExpr>),
    Index(Box<IndexExpr>),
    Block(Box<Block>),
    If(Box<IfExpr>),
    Match(Box<MatchExpr>),
    Closure(Box<ClosureExpr>),
}

#[derive(Debug, Clone)]
pub struct IdentExpr {
    pub name: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct LiteralExpr {
    pub value: Literal,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Literal {
    Int(i64),
    Float(f64),
    String(String),
    Bool(bool),
}

#[derive(Debug, Clone)]
pub struct BinaryExpr {
    pub left: Expr,
    pub op: BinaryOp,
    pub right: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
}

#[derive(Debug, Clone)]
pub struct UnaryExpr {
    pub op: UnaryOp,
    pub expr: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, Copy)]
pub enum UnaryOp {
    Neg,
    Not,
    Ref,
    Deref,
}

#[derive(Debug, Clone)]
pub struct CallExpr {
    pub callee: Expr,
    pub args: Vec<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct MethodCallExpr {
    pub receiver: Expr,
    pub method: String,
    pub args: Vec<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FieldAccessExpr {
    pub expr: Expr,
    pub field: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct IndexExpr {
    pub expr: Expr,
    pub index: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct IfExpr {
    pub condition: Expr,
    pub then_block: Block,
    pub else_block: Option<Block>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct MatchExpr {
    pub expr: Expr,
    pub arms: Vec<MatchArm>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Ident(String),
    Literal(Literal),
    Wildcard,
    // TODO: Add more patterns
}

#[derive(Debug, Clone)]
pub struct ClosureExpr {
    pub params: Vec<ClosureParam>,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ClosureParam {
    pub name: String,
    pub ty: Option<Type>,
}

#[derive(Debug, Clone)]
pub enum Type {
    Named(NamedType),
    Generic(GenericType),
    Function(Box<FunctionType>),
    Tuple(Vec<Type>),
    Reference(Box<Type>),
}

#[derive(Debug, Clone)]
pub struct NamedType {
    pub name: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct GenericType {
    pub name: String,
    pub args: Vec<Type>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FunctionType {
    pub params: Vec<Type>,
    pub return_type: Type,
    pub effects: Vec<String>,
}

// Placeholder types for future implementation
#[derive(Debug, Clone)]
pub struct EffectDecl {
    pub name: String,
    pub is_pub: bool,
    pub methods: Vec<EffectMethod>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct EffectMethod {
    pub name: String,
    pub params: Vec<Param>,
    pub return_type: Option<Type>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct StructDecl {
    pub name: String,
    pub is_pub: bool,
    pub fields: Vec<StructField>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct StructField {
    pub name: String,
    pub ty: Type,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct RecordDecl {
    pub name: String,
    pub is_pub: bool,
    pub fields: Vec<StructField>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct EnumDecl {
    pub name: String,
    pub is_pub: bool,
    pub variants: Vec<EnumVariant>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct EnumVariant {
    pub name: String,
    pub fields: Option<Vec<Type>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TypeAlias {
    pub name: String,
    pub is_pub: bool,
    pub ty: Type,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ImplBlock {
    pub ty: Type,
    pub methods: Vec<Function>,
    pub span: Span,
}
