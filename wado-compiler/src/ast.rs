// AST definitions for Wado

use crate::token::Span;

#[derive(Debug, Clone)]
pub struct Module {
    pub items: Vec<Item>,
    /// Content of the __DATA__ section, if present in the source file.
    /// This is available after parsing for tooling (test harnesses, IDEs).
    data_section: Option<String>,
}

impl Module {
    /// Creates a new module with the given items and no data section.
    pub fn new(items: Vec<Item>) -> Self {
        Self {
            items,
            data_section: None,
        }
    }

    /// Creates a new module with the given items and data section.
    pub fn with_data_section(items: Vec<Item>, data_section: Option<String>) -> Self {
        Self {
            items,
            data_section,
        }
    }

    /// Returns the content of the __DATA__ section, if present.
    pub fn data_section(&self) -> Option<&str> {
        self.data_section.as_deref()
    }
}

#[derive(Debug, Clone)]
pub enum Item {
    Use(UseDecl),
    Function(Function),
    Effect(EffectDecl),
    Struct(StructDecl),
    Enum(EnumDecl),
    Type(TypeAlias),
    Impl(ImplBlock),
    Resource(ResourceDecl),
    World(WorldDecl),
}

/// Attribute like #[wasi("...")]
#[derive(Debug, Clone)]
pub struct Attribute {
    pub name: String,
    pub args: Option<String>,
    pub wasi_import: Option<WasiImport>,
    pub span: Span,
}

/// Parsed WASI import path
/// e.g., "wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream"
#[derive(Debug, Clone)]
pub struct WasiImport {
    /// Namespace (e.g., "wasi")
    pub namespace: String,
    /// Package (e.g., "cli")
    pub package: String,
    /// Interface (e.g., "stdout")
    pub interface: String,
    /// Version (e.g., "0.3.0-rc-2025-09-16")
    pub version: Option<String>,
    /// Function name (e.g., "write-via-stream")
    pub function: Option<String>,
}

impl WasiImport {
    /// Parse a WASI path string
    /// Format: "namespace:package/interface@version#function"
    /// Examples:
    ///   "wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream"
    ///   "wasi:cli/terminal-input@0.3.0-rc-2025-09-16"
    pub fn parse(s: &str) -> Option<WasiImport> {
        // Split by '#' first to extract function name
        let (path, function) = if let Some(pos) = s.rfind('#') {
            (&s[..pos], Some(s[pos + 1..].to_string()))
        } else {
            (s, None)
        };

        // Split by '@' to extract version
        let (path, version) = if let Some(pos) = path.rfind('@') {
            (&path[..pos], Some(path[pos + 1..].to_string()))
        } else {
            (path, None)
        };

        // Split by ':' to extract namespace
        let (namespace, rest) = path.split_once(':')?;

        // Split by '/' to extract package and interface
        let (package, interface) = rest.split_once('/')?;

        Some(WasiImport {
            namespace: namespace.to_string(),
            package: package.to_string(),
            interface: interface.to_string(),
            version,
            function,
        })
    }

    /// Get the full interface path (e.g., "wasi:cli/stdout@0.3.0-rc-2025-09-16")
    pub fn interface_path(&self) -> String {
        let mut path = format!("{}:{}/{}", self.namespace, self.package, self.interface);
        if let Some(ref ver) = self.version {
            path.push('@');
            path.push_str(ver);
        }
        path
    }
}

/// Resource declaration like `resource Foo;`
#[derive(Debug, Clone)]
pub struct ResourceDecl {
    pub name: String,
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

/// World declaration
/// ```wado
/// world CliCommand {
///     import Stdout {
///         write_via_stream,
///     }
///     export async fn run() -> Result<(), ()>;
/// }
/// ```
#[derive(Debug, Clone)]
pub struct WorldDecl {
    pub name: String,
    pub imports: Vec<WorldImport>,
    pub exports: Vec<WorldExport>,
    pub span: Span,
}

/// A world import group
/// ```wado
/// import EffectName {
///     function_name_1,
///     function_name_2,
/// }
/// ```
#[derive(Debug, Clone)]
pub struct WorldImport {
    pub effect_name: String,
    pub functions: Vec<String>,
    pub span: Span,
}

/// A world export declaration
/// ```wado
/// export async fn run() -> Result<(), ()>;
/// export fn get_version() -> string;
/// ```
#[derive(Debug, Clone)]
pub struct WorldExport {
    pub name: String,
    pub is_async: bool,
    pub params: Vec<Param>,
    pub return_type: Option<Type>,
    pub span: Span,
}

/// Use declaration item with optional renaming
/// Supports both simple imports and effect function imports:
/// - Simple: `name` or `name as alias`
/// - Effect functions: `Effect::{func1, func2}`
#[derive(Debug, Clone)]
pub enum UseItem {
    /// Simple import: `name` or `name as alias`
    Simple { name: String, alias: Option<String> },
    /// Effect with functions: `Effect::{func1, func2}`
    EffectFunctions {
        effect_name: String,
        functions: Vec<UseItemSimple>,
    },
}

/// Simple use item (used within effect function imports)
#[derive(Debug, Clone)]
pub struct UseItemSimple {
    pub name: String,
    pub alias: Option<String>,
}

/// Import attributes for `with { ... }` clause
#[derive(Debug, Clone, Default)]
pub struct ImportAttributes {
    pub version: Option<String>,
    pub integrity: Option<String>,
    pub type_hint: Option<String>,
}

/// Use declaration with ESM-like syntax:
/// `use {items} from "source"`
/// `use {items} from "source" with { version: "1.0" }`
/// `pub use {items} from "source"` (re-export)
#[derive(Debug, Clone)]
pub struct UseDecl {
    /// Whether this is a public re-export
    pub is_pub: bool,
    /// Import source (e.g., "core:cli", "wasi:filesystem", "./utils.wado")
    pub source: String,
    /// Items being imported
    pub items: Vec<UseItem>,
    /// Optional import attributes
    pub attributes: Option<ImportAttributes>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Function {
    pub name: String,
    pub is_pub: bool,
    pub params: Vec<Param>,
    pub return_type: Option<Type>,
    pub effects: Vec<String>,
    /// Function body. None indicates a compiler built-in (bodyless declaration like `pub fn foo();`)
    pub body: Option<Block>,
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
    Assert(AssertStmt),
}

/// Assert statement: `assert expr;` or `assert expr, "message";`
/// If the expression is false, prints a power-assert style error message and calls unreachable
#[derive(Debug, Clone)]
pub struct AssertStmt {
    pub condition: Expr,
    /// Optional message expression (typically a String literal or template string)
    pub message: Option<Expr>,
    pub span: Span,
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

/// C-style for loop: `for (init; condition; update) { body }`
#[derive(Debug, Clone)]
pub struct ForStmt {
    /// Initialization statement (e.g., `let i = 0`)
    pub init: Option<Box<Stmt>>,
    /// Loop condition (e.g., `i < 10`)
    pub condition: Option<Expr>,
    /// Update expression (e.g., `i = i + 1`)
    pub update: Option<Expr>,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Ident(IdentExpr),
    Literal(LiteralExpr),
    Binary(Box<BinaryExpr>),
    Unary(Box<UnaryExpr>),
    Assign(Box<AssignExpr>),
    Call(Box<CallExpr>),
    MethodCall(Box<MethodCallExpr>),
    FieldAccess(Box<FieldAccessExpr>),
    Index(Box<IndexExpr>),
    Block(Box<Block>),
    If(Box<IfExpr>),
    Match(Box<MatchExpr>),
    Closure(Box<ClosureExpr>),
    TemplateString(Box<TemplateStringExpr>),
    Cast(Box<CastExpr>),
}

impl Expr {
    /// Get the source span for this expression
    pub fn span(&self) -> Span {
        match self {
            Expr::Ident(e) => e.span,
            Expr::Literal(e) => e.span,
            Expr::Binary(e) => e.span,
            Expr::Unary(e) => e.span,
            Expr::Assign(e) => e.span,
            Expr::Call(e) => e.span,
            Expr::MethodCall(e) => e.span,
            Expr::FieldAccess(e) => e.span,
            Expr::Index(e) => e.span,
            Expr::Block(e) => e.span,
            Expr::If(e) => e.span,
            Expr::Match(e) => e.span,
            Expr::Closure(e) => e.span,
            Expr::TemplateString(e) => e.span,
            Expr::Cast(e) => e.span,
        }
    }
}

/// Type cast expression: `expr as Type`
#[derive(Debug, Clone)]
pub struct CastExpr {
    pub expr: Expr,
    pub target_type: Type,
    pub span: Span,
}

/// Assignment expression: `x = value` or `x.field = value`
#[derive(Debug, Clone)]
pub struct AssignExpr {
    pub target: Expr,
    pub value: Expr,
    pub span: Span,
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
    Char(char),
    Bool(bool),
    Null,
    Unit,
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
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
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
    BitNot,
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
    Tuple(Vec<Pattern>),
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

/// Template string expression: `Hello, {name}!`
#[derive(Debug, Clone)]
pub struct TemplateStringExpr {
    pub parts: Vec<TemplatePart>,
    pub span: Span,
}

/// A part of a template string - either a literal string or an interpolation
#[derive(Debug, Clone)]
pub enum TemplatePart {
    /// A literal string part
    String(String),
    /// An interpolated expression with optional format specifier
    Interpolation {
        expr: Box<Expr>,
        format: Option<FormatSpec>,
    },
}

/// Format specifier for template string interpolation
/// Examples: ".2f", "0.3f", "10", "d"
#[derive(Debug, Clone)]
pub struct FormatSpec {
    pub spec: String,
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
    pub is_async: bool,
    pub attrs: Vec<Attribute>,
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
