// AST definitions for Wado

use crate::token::Span;

#[derive(Debug, Clone)]
pub struct Module {
    pub items: Vec<Item>,
    /// Inner attributes like `#![no_prelude]`
    pub inner_attributes: Vec<InnerAttribute>,
    /// Shebang line, if present (e.g., "#!/usr/bin/env wado").
    shebang: Option<String>,
    /// Content of the __DATA__ section, if present in the source file.
    /// This is available after parsing for tooling (test harnesses, IDEs).
    data_section: Option<String>,
}

/// Inner attribute like `#![no_prelude]`
#[derive(Debug, Clone)]
pub struct InnerAttribute {
    pub name: String,
    pub span: Span,
}

impl Module {
    /// Creates a new module with the given items and no data section.
    pub fn new(items: Vec<Item>) -> Self {
        Self {
            items,
            inner_attributes: Vec::new(),
            shebang: None,
            data_section: None,
        }
    }

    /// Creates a new module with the given items, shebang, and data section.
    pub fn with_metadata(
        items: Vec<Item>,
        inner_attributes: Vec<InnerAttribute>,
        shebang: Option<String>,
        data_section: Option<String>,
    ) -> Self {
        Self {
            items,
            inner_attributes,
            shebang,
            data_section,
        }
    }

    /// Returns the inner attributes.
    pub fn inner_attributes(&self) -> &[InnerAttribute] {
        &self.inner_attributes
    }

    /// Returns true if the module has the `#![no_prelude]` attribute.
    pub fn has_no_prelude(&self) -> bool {
        self.inner_attributes.iter().any(|a| a.name == "no_prelude")
    }

    /// Returns the shebang line, if present.
    pub fn shebang(&self) -> Option<&str> {
        self.shebang.as_deref()
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
    Variant(VariantDecl),
    Flags(FlagsDecl),
    Type(Newtype),
    Impl(ImplBlock),
    Trait(TraitDecl),
    Resource(ResourceDecl),
    World(WorldDecl),
    Test(TestDecl),
    Global(GlobalDecl),
}

/// Test declaration: `test "name" { ... }` or `test { ... }`
#[derive(Debug, Clone)]
pub struct TestDecl {
    /// Attributes applied to this test (e.g., `#[expect_trap]`).
    pub attributes: Vec<Attribute>,
    /// Optional test name (string literal). If None, identified by <file:line>.
    pub name: Option<String>,
    pub body: Block,
    pub span: Span,
}

/// Global variable declaration: `global name: Type = expr;`
/// or `pub global mut name: Type = expr;`
#[derive(Debug, Clone)]
pub struct GlobalDecl {
    pub name: String,
    pub ty: Type,
    pub initializer: Expr,
    pub mutable: bool,
    pub is_pub: bool,
    pub attributes: Vec<Attribute>,
    pub span: Span,
}

/// Attribute like #[wasi("...")]
#[derive(Debug, Clone)]
pub struct Attribute {
    pub name: String,
    /// Arguments passed to the attribute, e.g., #[canonical("wasi", "stream-new")] -> ["wasi", "stream-new"]
    pub args: Vec<String>,
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

/// Resource declaration like `resource Foo;` or `resource Foo<T> { fn method(...); }`
#[derive(Debug, Clone)]
pub struct ResourceDecl {
    pub name: String,
    pub is_pub: bool,
    /// Generic type parameters: `resource Future<T> { ... }`
    pub type_params: Vec<GenericParam>,
    pub attrs: Vec<Attribute>,
    /// Methods declared within the resource block (reuses `EffectMethod` structure)
    pub methods: Vec<EffectMethod>,
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
    pub is_pub: bool,
    pub attrs: Vec<Attribute>,
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
/// Supports simple imports, effect function imports, and wildcard imports:
/// - Simple: `name` or `name as alias`
/// - Effect functions: `Effect::{func1, func2}`
/// - Wildcard: `_` (load module without binding names)
#[derive(Debug, Clone)]
pub enum UseItem {
    /// Simple import: `name` or `name as alias`
    Simple { name: String, alias: Option<String> },
    /// Effect with functions: `Effect::{func1, func2}`
    EffectFunctions {
        effect_name: String,
        functions: Vec<UseItemSimple>,
    },
    /// Wildcard import: `use _ from "..."` (load module for side effects only)
    Wildcard,
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
    /// Whether this function is exported at the Component Model boundary (world export)
    pub is_export: bool,
    /// Whether this is an async function (`export async fn`).
    /// Async functions use `task return` instead of `return` to deliver results
    /// without terminating the function.
    pub is_async: bool,
    /// Generic type parameters: `fn swap<T>(a: T, b: T) -> T`
    pub type_params: Vec<GenericParam>,
    pub attrs: Vec<Attribute>,
    pub params: Vec<Param>,
    pub return_type: Option<Type>,
    pub effects: Vec<String>,
    /// Function body. None indicates a compiler built-in (bodyless declaration like `pub fn foo();`)
    pub body: Option<Block>,
    pub span: Span,
}

/// Self parameter kind: &self or &mut self
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfKind {
    None,
    Ref,
    MutRef,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub ty: Type,
    pub self_kind: SelfKind,
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
    /// `task return expr;` — delivers the async task result without terminating the function.
    /// Only valid inside `export async fn` bodies.
    TaskReturn(TaskReturnStmt),
    If(IfStmt),
    While(WhileStmt),
    For(ForStmt),
    ForOf(ForOfStmt),
    Loop(LoopStmt),
    Break(BreakStmt),
    Continue(ContinueStmt),
    Assert(AssertStmt),
    LabeledBlock(LabeledBlockStmt),
}

/// Labeled block statement: `LABEL: { ... }`
/// Creates a new scope with local bindings. The label is required to reduce syntactic ambiguity.
#[derive(Debug, Clone)]
pub struct LabeledBlockStmt {
    pub label: String,
    pub block: Block,
    pub span: Span,
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
    pub pattern: Pattern,
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

/// `task return expr;` — delivers the async task result without terminating the function.
/// Only valid inside `export async fn` bodies.
#[derive(Debug, Clone)]
pub struct TaskReturnStmt {
    pub value: Expr,
    pub span: Span,
}

/// Condition in control flow statements: either a regular expression or a pattern match
/// Used in `if`, `while`, and `for` statements.
#[derive(Debug, Clone)]
pub enum Condition {
    /// Regular boolean expression: `if x > 0 { ... }` or `while x > 0 { ... }`
    Expr(Expr),
    /// Rust-style pattern match: `if let Some(x) = expr { ... }` or `while let Some(x) = expr { ... }`
    Pattern {
        pattern: Pattern,
        expr: Expr,
        span: Span,
    },
}

#[derive(Debug, Clone)]
pub struct IfStmt {
    /// Optional init binding: `if let x = expr; condition { ... }`
    pub init: Option<Box<LetStmt>>,
    pub condition: Condition,
    pub then_block: Block,
    pub else_block: Option<Block>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct WhileStmt {
    pub condition: Condition,
    pub body: Block,
    pub span: Span,
}

/// C-style for loop: `for (init; condition; update) { body }`
/// Also supports pattern conditions: `for init; let Some(x) = iter.next(); update { body }`
#[derive(Debug, Clone)]
pub struct ForStmt {
    /// Initialization statement (e.g., `let i = 0`)
    pub init: Option<Box<Stmt>>,
    /// Loop condition (e.g., `i < 10` or `let Some(x) = iter.next()`)
    pub condition: Option<Condition>,
    /// Update expression (e.g., `i = i + 1`)
    pub update: Option<Expr>,
    pub body: Block,
    pub span: Span,
}

/// For-of loop: `for let item of array { body }`
/// Iterates over elements of an Array<T>
#[derive(Debug, Clone)]
pub struct ForOfStmt {
    /// Pattern to bind each element (Ident for simple, Struct/Tuple for destructuring)
    pub binding: Pattern,
    /// Whether the binding is mutable
    pub is_mut: bool,
    /// The array expression to iterate over
    pub iterable: Expr,
    pub body: Block,
    pub span: Span,
}

/// Infinite loop: `loop { body }`
#[derive(Debug, Clone)]
pub struct LoopStmt {
    pub body: Block,
    pub span: Span,
}

/// Break statement: `break;`, `break label;`, or `break label: expr;`
#[derive(Debug, Clone)]
pub struct BreakStmt {
    /// Optional label to break to (for labeled blocks)
    pub label: Option<String>,
    /// Optional value to return from the labeled block
    pub value: Option<Box<Expr>>,
    pub span: Span,
}

/// Continue statement: `continue;`
#[derive(Debug, Clone)]
pub struct ContinueStmt {
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Ident(IdentExpr),
    Literal(LiteralExpr),
    Binary(Box<BinaryExpr>),
    Unary(Box<UnaryExpr>),
    Assign(Box<AssignExpr>),
    CompoundAssign(Box<CompoundAssignExpr>),
    ComparisonChain(Box<ComparisonChainExpr>),
    Call(Box<CallExpr>),
    MethodCall(Box<MethodCallExpr>),
    StaticMethodCall(Box<StaticMethodCallExpr>),
    FieldAccess(Box<FieldAccessExpr>),
    Index(Box<IndexExpr>),
    Block(Box<Block>),
    If(Box<IfExpr>),
    Match(Box<MatchExpr>),
    Matches(Box<MatchesExpr>),
    Closure(Box<ClosureExpr>),
    TemplateString(Box<TemplateStringExpr>),
    Cast(Box<CastExpr>),
    StructLiteral(Box<StructLiteralExpr>),
    TupleLiteral(Box<TupleLiteralExpr>),
    /// Labeled block expression: `label: { ... }` where the last expression is the value
    LabeledBlock(Box<LabeledBlockExpr>),
}

/// Labeled block expression: `label: { ... }` that produces a value
/// The value is the last expression in the block (if any).
#[derive(Debug, Clone)]
pub struct LabeledBlockExpr {
    pub label: String,
    pub block: Block,
    pub span: Span,
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
            Expr::CompoundAssign(e) => e.span,
            Expr::ComparisonChain(e) => e.span,
            Expr::Call(e) => e.span,
            Expr::MethodCall(e) => e.span,
            Expr::StaticMethodCall(e) => e.span,
            Expr::FieldAccess(e) => e.span,
            Expr::Index(e) => e.span,
            Expr::Block(e) => e.span,
            Expr::If(e) => e.span,
            Expr::Match(e) => e.span,
            Expr::Matches(e) => e.span,
            Expr::Closure(e) => e.span,
            Expr::TemplateString(e) => e.span,
            Expr::Cast(e) => e.span,
            Expr::StructLiteral(e) => e.span,
            Expr::TupleLiteral(e) => e.span,
            Expr::LabeledBlock(e) => e.span,
        }
    }

    /// Create a copy of this expression with an updated span.
    /// Used for parenthesized expressions to include the parens in the span.
    pub fn with_span(self, new_span: Span) -> Expr {
        match self {
            Expr::Ident(mut e) => {
                e.span = new_span;
                Expr::Ident(e)
            }
            Expr::Literal(mut e) => {
                e.span = new_span;
                Expr::Literal(e)
            }
            Expr::Binary(mut e) => {
                e.span = new_span;
                Expr::Binary(e)
            }
            Expr::Unary(mut e) => {
                e.span = new_span;
                Expr::Unary(e)
            }
            Expr::Assign(mut e) => {
                e.span = new_span;
                Expr::Assign(e)
            }
            Expr::CompoundAssign(mut e) => {
                e.span = new_span;
                Expr::CompoundAssign(e)
            }
            Expr::ComparisonChain(mut e) => {
                e.span = new_span;
                Expr::ComparisonChain(e)
            }
            Expr::Call(mut e) => {
                e.span = new_span;
                Expr::Call(e)
            }
            Expr::MethodCall(mut e) => {
                e.span = new_span;
                Expr::MethodCall(e)
            }
            Expr::StaticMethodCall(mut e) => {
                e.span = new_span;
                Expr::StaticMethodCall(e)
            }
            Expr::FieldAccess(mut e) => {
                e.span = new_span;
                Expr::FieldAccess(e)
            }
            Expr::Index(mut e) => {
                e.span = new_span;
                Expr::Index(e)
            }
            Expr::Block(mut e) => {
                e.span = new_span;
                Expr::Block(e)
            }
            Expr::If(mut e) => {
                e.span = new_span;
                Expr::If(e)
            }
            Expr::Match(mut e) => {
                e.span = new_span;
                Expr::Match(e)
            }
            Expr::Matches(mut e) => {
                e.span = new_span;
                Expr::Matches(e)
            }
            Expr::Closure(mut e) => {
                e.span = new_span;
                Expr::Closure(e)
            }
            Expr::TemplateString(mut e) => {
                e.span = new_span;
                Expr::TemplateString(e)
            }
            Expr::Cast(mut e) => {
                e.span = new_span;
                Expr::Cast(e)
            }
            Expr::StructLiteral(mut e) => {
                e.span = new_span;
                Expr::StructLiteral(e)
            }
            Expr::TupleLiteral(mut e) => {
                e.span = new_span;
                Expr::TupleLiteral(e)
            }
            Expr::LabeledBlock(mut e) => {
                e.span = new_span;
                Expr::LabeledBlock(e)
            }
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

/// Struct literal expression: `Point { x: 10, y: 20 }` or implicit `{ x: 10, y: 20 }`
#[derive(Debug, Clone)]
pub struct StructLiteralExpr {
    /// The struct type name. None for implicit struct literals like `{ x: 1, y: 2 }`
    /// which require type context (e.g., `let p: Point = { x: 1, y: 2 }`).
    pub name: Option<String>,
    pub fields: Vec<StructLiteralField>,
    /// Whether the original source had a trailing comma (for formatting purposes).
    /// Multiline formatting is used when this is true.
    pub has_trailing_comma: bool,
    pub span: Span,
}

/// A field in a struct literal: `x: 10` or `x` (shorthand)
#[derive(Debug, Clone)]
pub struct StructLiteralField {
    pub name: String,
    pub value: Expr,
    /// Whether this field uses shorthand syntax `{ x }` instead of `{ x: x }`
    pub is_shorthand: bool,
    pub span: Span,
}

/// Tuple literal expression: `[1, 2, 3]` or `[1, "hello", true]`
/// This uses bracket syntax following TypeScript conventions.
/// Can be coerced to Array<T> when all elements have the same type.
#[derive(Debug, Clone)]
pub struct TupleLiteralExpr {
    pub elements: Vec<Expr>,
    pub span: Span,
}

/// Assignment expression: `x = value` or `x.field = value`
#[derive(Debug, Clone)]
pub struct AssignExpr {
    pub target: Expr,
    pub value: Expr,
    pub span: Span,
}

/// Compound assignment operators
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CompoundAssignOp {
    Add, // +=
    Sub, // -=
    Mul, // *=
    Div, // /=
    Mod, // %=
}

/// Compound assignment expression: `x += value`
#[derive(Debug, Clone)]
pub struct CompoundAssignExpr {
    pub target: Expr,
    pub op: CompoundAssignOp,
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
    /// Numeric literal: raw source text (e.g. `"42"`, `"0xFF"`, `"3.14"`). Type resolved later.
    Number(String),
    /// String literal: raw source text between quotes (escape sequences not interpreted).
    String(String),
    /// Char literal: raw source text between quotes (escape sequences not interpreted).
    Char(String),
    Bool(bool),
    Null,
    Unit,
    /// Compile-time location literal: `#file`
    LocationFile,
    /// Compile-time location literal: `#line`
    LocationLine,
    /// Compile-time location literal: `#function`
    LocationFunction,
    /// Compile-time data section literal: `#data`
    DataSection,
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

/// A comparison in a chain (e.g., the `< b` part of `a < b < c`)
#[derive(Debug, Clone)]
pub struct ChainedComparison {
    pub op: BinaryOp,
    pub right: Expr,
    pub op_span: Span,
}

/// Comparison chain expression: `a < b < c` or `0 <= x <= 100`
#[derive(Debug, Clone)]
pub struct ComparisonChainExpr {
    pub first: Expr,
    pub comparisons: Vec<ChainedComparison>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct UnaryExpr {
    pub op: UnaryOp,
    pub expr: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
    BitNot,
    Ref,
    MutRef,
    Deref,
}

#[derive(Debug, Clone)]
pub struct CallExpr {
    pub callee: Expr,
    /// Explicit type arguments for generic functions: `foo::<i32>(x)`
    pub type_args: Vec<Type>,
    pub args: Vec<Expr>,
    /// Whether the original source had a trailing comma (for formatting purposes).
    pub has_trailing_comma: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct MethodCallExpr {
    pub receiver: Expr,
    pub method: String,
    /// Explicit type arguments for generic methods: `obj.foo::<i32>(x)`
    pub type_args: Vec<Type>,
    pub args: Vec<Expr>,
    /// Whether the original source had a trailing comma (for formatting purposes).
    pub has_trailing_comma: bool,
    pub span: Span,
}

/// Static method call expression: `Array::<i32>::with_capacity(100)` or `Point::origin()`
#[derive(Debug, Clone)]
pub struct StaticMethodCallExpr {
    /// The target type (e.g., `Array<i32>` or `Point`)
    pub target_type: Type,
    /// The method name (e.g., `with_capacity` or `origin`)
    pub method: String,
    /// Arguments to the method
    pub args: Vec<Expr>,
    /// Whether the original source had a trailing comma (for formatting purposes).
    pub has_trailing_comma: bool,
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
    /// Optional init binding: `if let x = expr; condition { ... }`
    pub init: Option<Box<LetStmt>>,
    pub condition: Condition,
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
    /// Optional guard expression (the condition after `&&`)
    pub guard: Option<Expr>,
    pub body: Expr,
    pub span: Span,
}

/// Matches expression: `expr matches { pattern && guard }`
/// Returns true if the pattern matches and the optional guard is true.
#[derive(Debug, Clone)]
pub struct MatchesExpr {
    pub expr: Expr,
    pub pattern: Pattern,
    /// Optional guard expression (the condition after `&&`)
    pub guard: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Ident(String),
    MutIdent(String),
    Literal(Literal),
    Wildcard,
    Tuple(Vec<Pattern>),
    /// Variant pattern: `Some(x)` or `None`
    Variant {
        variant_name: String,
        bindings: Vec<Pattern>,
        span: Span,
    },
    /// Struct destructuring pattern: `{ x, y }` or `Point { x, y }`
    Struct {
        type_name: Option<String>,
        fields: Vec<StructPatternField>,
        has_rest: bool,
        span: Span,
    },
}

#[derive(Debug, Clone)]
pub struct StructPatternField {
    pub field_name: String,
    pub pattern: Pattern,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ClosureExpr {
    pub params: Vec<ClosureParam>,
    pub body: Expr,
    /// Pre-desugar source text, set by the desugar phase before transforming the body.
    pub source_text: Option<String>,
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
    /// Namespaced generic type like `builtin::array<T>`
    NamespacedGeneric(NamespacedGenericType),
    Function(Box<FunctionType>),
    Tuple(Vec<Type>),
    Reference(Box<Type>),
    MutReference(Box<Type>),
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

/// Namespaced generic type like `builtin::array<T>`
#[derive(Debug, Clone)]
pub struct NamespacedGenericType {
    /// Namespace (e.g., "builtin")
    pub namespace: String,
    /// Type name (e.g., "array")
    pub name: String,
    /// Generic arguments
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
    pub attrs: Vec<Attribute>,
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

/// An associated type binding inside a trait bound, e.g., `Output = T` in `Builder<Output = T>`.
#[derive(Debug, Clone)]
pub struct AssocTypeBound {
    pub name: String,
    pub ty: Type,
    pub span: Span,
}

/// A single trait bound on a generic parameter.
/// Simple: `Ord`; with associated type bindings: `Builder<Output = T>`.
#[derive(Debug, Clone)]
pub struct TraitBound {
    pub name: String,
    pub assoc_types: Vec<AssocTypeBound>,
    pub span: Span,
}

/// Generic type parameter declaration: `<T>`, `<T, U>`, `<T: Ord>`, `<T: Builder<Output = T>>`
#[derive(Debug, Clone)]
pub struct GenericParam {
    pub name: String,
    /// Trait bounds (e.g., `Ord`, `Builder<Output = T>`)
    pub bounds: Vec<TraitBound>,
    /// Default type (e.g., `T = []` or `Effects = []`)
    pub default: Option<Type>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct StructDecl {
    pub name: String,
    pub is_pub: bool,
    /// Generic type parameters: `struct Pair<T, U> { ... }`
    pub type_params: Vec<GenericParam>,
    pub fields: Vec<StructField>,
    /// Attributes like `#[wasi("...")]`
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct StructField {
    pub name: String,
    pub is_pub: bool,
    pub ty: Type,
    /// Attributes like `#[wasi("...")]` for CM name override
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct EnumDecl {
    pub name: String,
    pub is_pub: bool,
    /// Generic type parameters: `enum Option<T> { Some(T), None }`
    pub type_params: Vec<GenericParam>,
    pub cases: Vec<EnumCase>,
    /// Attributes like #[wasi("...")]
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

/// A case in an enum declaration.
/// Unlike `VariantCase`, enum cases have no payload.
#[derive(Debug, Clone)]
pub struct EnumCase {
    pub name: String,
    /// Attributes like `#[wasi("wit-kebab-name")]` for CM name override
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

/// Flags declaration (bitflags type from WASI)
/// ```wado
/// flags DescriptorFlags {
///     Read,
///     Write,
///     FileIntegritySync,
/// }
/// ```
#[derive(Debug, Clone)]
pub struct FlagsDecl {
    pub name: String,
    pub is_pub: bool,
    pub attributes: Option<Vec<Attribute>>,
    pub flags: Vec<FlagsVariant>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FlagsVariant {
    pub name: String,
    /// Attributes like `#[wasi("wit-kebab-name")]` for CM name override
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

/// Variant declaration (tagged union with payloads)
/// ```wado
/// variant Option<T> {
///     Some(T),
///     None,
/// }
/// ```
#[derive(Debug, Clone)]
pub struct VariantDecl {
    pub name: String,
    pub is_pub: bool,
    /// Generic type parameters: `variant Option<T> { Some(T), None }`
    pub type_params: Vec<GenericParam>,
    pub cases: Vec<VariantCase>,
    /// Attributes like `#[wasi("...")]`
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

/// A case in a variant declaration: `Some(T)` or `None`
///
/// Each variant case has exactly one payload type:
/// - Unit variants: `None` → payload is `None` (parser level), becomes `()` in TIR
/// - Scalar payloads: `Some(T)` → payload is `Some(T)`
/// - Tuple payloads: `Rectangle([f64, f64])` → payload is `Some([f64, f64])`
/// - Struct payloads: `Named({ w: f64 })` → payload is `Some({ w: f64 })`
#[derive(Debug, Clone)]
pub struct VariantCase {
    pub name: String,
    /// Payload type for this case. None for unit variants like `None`.
    /// At TIR level, unit variants are normalized to have `()` payload.
    pub payload: Option<Type>,
    /// Attributes like `#[wasi("wit-kebab-name")]` for CM name override
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Newtype {
    pub name: String,
    pub is_pub: bool,
    pub ty: Type,
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

/// Associated type declaration in a trait: `type Output;` or `type Output: Trait1 + Trait2;`
#[derive(Debug, Clone)]
pub struct AssociatedTypeDecl {
    pub name: String,
    /// Trait bounds on this associated type (e.g., `SerializeSeq` in `type SeqSerializer: SerializeSeq;`)
    pub bounds: Vec<String>,
    pub span: Span,
}

/// Associated type binding in an impl block: `type Output = T;`
#[derive(Debug, Clone)]
pub struct AssociatedTypeBinding {
    pub name: String,
    pub ty: Type,
    pub span: Span,
}

/// Associated constant in an impl block: `pub const PI: f64 = 3.14159;`
/// These are compile-time constants that are inlined at every use site.
#[derive(Debug, Clone)]
pub struct AssociatedConst {
    pub name: String,
    pub is_pub: bool,
    pub ty: Type,
    pub value: Expr,
    pub span: Span,
}

/// Trait declaration: `trait Foo { type Output; fn method(&self) -> Self::Output; }`
#[derive(Debug, Clone)]
pub struct TraitDecl {
    pub name: String,
    pub is_pub: bool,
    pub type_params: Vec<GenericParam>,
    /// Associated type declarations: `type Output;`
    pub associated_types: Vec<AssociatedTypeDecl>,
    /// Trait methods. Body is None for required methods.
    pub methods: Vec<Function>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ImplBlock {
    /// Generic type parameters: `impl<T> Box<T> { ... }`
    pub type_params: Vec<GenericParam>,
    /// The trait being implemented, if any: `impl Trait for Type`
    /// None for inherent impl blocks: `impl Type`
    pub trait_type: Option<Type>,
    pub ty: Type,
    /// Associated type bindings: `type Output = T;`
    pub associated_types: Vec<AssociatedTypeBinding>,
    /// Associated constants: `pub const PI: f64 = 3.14159;`
    pub constants: Vec<AssociatedConst>,
    pub methods: Vec<Function>,
    /// `impl Trait for Type;` — synthesis request (compiler generates the body)
    pub is_synthesize_request: bool,
    pub span: Span,
}
