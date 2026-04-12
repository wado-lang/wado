// AST definitions for Wado

use crate::hashmap::IndexSet;
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
    /// Paths referenced by `#include_str` and `#include_bytes` literals, collected during parsing.
    include_paths: IndexSet<String>,
}

/// Inner attribute like `#![no_prelude]`, `#![wasm_module("mem")]`, or
/// `#![generated(source = "path", by = "tool")]`.
///
/// Arguments use the same `AttrArg` representation as outer attributes so that
/// key-value metadata (e.g. `source = "file.wit"`) survives unparse/reformat.
#[derive(Debug, Clone)]
pub struct InnerAttribute {
    pub name: String,
    pub args: Vec<AttrArg>,
    pub span: Span,
}

impl InnerAttribute {
    /// Find the first key-value argument with the given key.
    ///
    /// For `#![generated(by = "gale")]`, `attr.kv_value("by")` returns `Some("gale")`.
    pub fn kv_value(&self, key: &str) -> Option<&str> {
        self.args.iter().find_map(|arg| {
            if let AttrArg::KeyValue(k, v) = arg {
                if k == key { Some(v.as_str()) } else { None }
            } else {
                None
            }
        })
    }

    /// Return all values from key-value arguments with the given key (preserving order).
    ///
    /// Supports repeated keys such as
    /// `#![generated(source = "a.wit", source = "b.wit")]`.
    pub fn kv_values<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.args.iter().filter_map(move |arg| {
            if let AttrArg::KeyValue(k, v) = arg {
                if k == key { Some(v.as_str()) } else { None }
            } else {
                None
            }
        })
    }
}

impl Module {
    /// Creates a new module with the given items and no data section.
    pub fn new(items: Vec<Item>) -> Self {
        Self {
            items,
            inner_attributes: Vec::new(),
            shebang: None,
            data_section: None,
            include_paths: IndexSet::default(),
        }
    }

    /// Creates a new module with the given items, shebang, and data section.
    pub fn with_metadata(
        items: Vec<Item>,
        inner_attributes: Vec<InnerAttribute>,
        shebang: Option<String>,
        data_section: Option<String>,
        include_paths: IndexSet<String>,
    ) -> Self {
        Self {
            items,
            inner_attributes,
            shebang,
            data_section,
            include_paths,
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

    /// Returns true if the module has the `#![TODO]` attribute.
    /// All tests in a TODO module must fail; passing tests become failures.
    pub fn has_todo(&self) -> bool {
        self.inner_attributes.iter().any(|a| a.name == "TODO")
    }

    /// Returns true if the module has the `#![generated]` attribute.
    /// Indicates machine-generated code (e.g. wado-from-idl, gale).
    pub fn has_generated(&self) -> bool {
        self.inner_attributes.iter().any(|a| a.name == "generated")
    }

    /// Returns the `wasm_module` name if `#![wasm_module("name")]` is present.
    pub fn wasm_module(&self) -> Option<&str> {
        self.inner_attributes
            .iter()
            .find(|a| a.name == "wasm_module")
            .and_then(|a| a.args.first())
            .map(AttrArg::as_str)
    }

    /// Returns the value of a key-value argument on any `#![generated(...)]`
    /// inner attribute.
    ///
    /// Scans all `#![generated(...)]` attributes and returns the first matching
    /// `key = "value"` pair. For example, given
    /// `#![generated(by = "wado-from-idl", source = "a.wit")]`,
    /// `module.generated_meta("by")` returns `Some("wado-from-idl")`.
    pub fn generated_meta(&self, key: &str) -> Option<&str> {
        self.inner_attributes
            .iter()
            .filter(|a| a.name == "generated")
            .find_map(|a| a.kv_value(key))
    }

    /// Returns all values for a key-value argument across every
    /// `#![generated(...)]` inner attribute, preserving source order.
    pub fn generated_meta_all<'a>(&'a self, key: &'a str) -> Vec<&'a str> {
        self.inner_attributes
            .iter()
            .filter(|a| a.name == "generated")
            .flat_map(|a| a.kv_values(key))
            .collect()
    }

    /// Returns the shebang line, if present.
    pub fn shebang(&self) -> Option<&str> {
        self.shebang.as_deref()
    }

    /// Returns the content of the __DATA__ section, if present.
    pub fn data_section(&self) -> Option<&str> {
        self.data_section.as_deref()
    }

    pub fn include_paths(&self) -> &IndexSet<String> {
        &self.include_paths
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
    Newtype(Newtype),
    TupleTypeDecl(TupleTypeDecl),
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

/// A single argument in an attribute.
///
/// Distinguishes between string literals, bare identifiers, and key=value pairs so that
/// `unparse` can reconstruct the original syntax faithfully.
///
/// Examples:
/// - `#[cm("wasi:cli/stdout")]`          → `[Str("wasi:cli/stdout")]`
/// - `#[inline(always)]`                   → `[Ident("always")]`
/// - `#[serde(rename = "type")]`           → `[KeyValue("rename", "type")]`
/// - `#[canonical("wasi", "stream-new")]`  → `[Str("wasi"), Str("stream-new")]`
/// - `#[timeout_ms(120000)]`               → `[Number("120000")]`
#[derive(Debug, Clone)]
pub enum AttrArg {
    /// A quoted string literal, e.g. `"value"`.
    Str(String),
    /// A bare identifier, e.g. `always` or `default`.
    Ident(String),
    /// A key = "value" pair, e.g. `rename = "type"`.
    KeyValue(String, String),
    /// A numeric literal, e.g. `120000`.
    Number(String),
}

impl AttrArg {
    /// Returns the string value: for `Str`/`Ident` the contained string; for `KeyValue` the value side.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Str(s) | Self::Ident(s) | Self::Number(s) => s,
            Self::KeyValue(_, v) => v,
        }
    }
}

/// Attribute like #[cm("...")]
#[derive(Debug, Clone)]
pub struct Attribute {
    pub name: String,
    /// Arguments passed to the attribute.
    ///
    /// - `#[canonical("wasi", "stream-new")]` → `[Str("wasi"), Str("stream-new")]`
    /// - `#[serde(rename = "type")]`           → `[KeyValue("rename", "type")]`
    /// - `#[serde(default)]`                   → `[Ident("default")]`
    pub args: Vec<AttrArg>,
    pub cm_import: Option<CmImport>,
    pub span: Span,
}

impl Attribute {
    /// Find the value of a key-value argument by key name.
    ///
    /// For `#[serde(rename = "type")]`, `attr.kv_value("rename")` returns `Some("type")`.
    pub fn kv_value(&self, key: &str) -> Option<&str> {
        self.args.iter().find_map(|arg| {
            if let AttrArg::KeyValue(k, v) = arg {
                if k == key { Some(v.as_str()) } else { None }
            } else {
                None
            }
        })
    }

    /// Return true if any arg matches the given name as an identifier, string, or key in a key-value pair.
    ///
    /// For `#[serde(default)]`, `attr.has_arg("default")` returns `true`.
    pub fn has_arg(&self, name: &str) -> bool {
        self.args.iter().any(|arg| match arg {
            AttrArg::Str(s) | AttrArg::Ident(s) | AttrArg::Number(s) => s == name,
            AttrArg::KeyValue(k, _) => k == name,
        })
    }
}

/// Parsed Component Model import path
/// e.g., "wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream"
#[derive(Debug, Clone)]
pub struct CmImport {
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

impl CmImport {
    /// Parse a CM import path string
    /// Format: "namespace:package/interface@version#function"
    /// Examples:
    ///   "wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream"
    ///   "wasi:cli/terminal-input@0.3.0-rc-2025-09-16"
    pub fn parse(s: &str) -> Option<CmImport> {
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

        Some(CmImport {
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
/// Supports simple imports, effect function imports, wildcard imports, and namespace imports:
/// - Simple: `name` or `name as alias`
/// - Effect functions: `Effect::{func1, func2}`
/// - Wildcard: `_` (load module without binding names)
/// - Namespace: `use name from "..."` (import entire module as namespace)
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
    /// Namespace import: `use name from "..."` (import entire module as namespace)
    Namespace { name: String },
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
    /// Parameters declared in `stores[param1, param2]` — the function may store these references.
    pub stores: Vec<String>,
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
    pub is_mut: bool,
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
    Match(Box<MatchExpr>),
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
    /// Initializer expression, or `None` for uninitialized declarations (`let x: i32;`).
    pub value: Option<Expr>,
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

/// A single element in a let-chain condition
#[derive(Debug, Clone)]
pub enum ConditionElement {
    /// `let PAT = EXPR` — pattern match element
    Let {
        pattern: Pattern,
        expr: Expr,
        span: Span,
    },
    /// `BOOL_EXPR` — boolean expression element
    Expr(Expr),
}

/// Condition in control flow statements: either a regular expression or a let chain.
/// Used in `if`, `while`, and `for` statements.
#[derive(Debug, Clone)]
pub enum Condition {
    /// Regular boolean expression: `if x > 0 { ... }` or `while x > 0 { ... }`
    Expr(Expr),
    /// Let chain: `if let PAT = EXPR && BOOL && let PAT2 = EXPR2 { ... }`
    /// Also handles simple `if let PAT = EXPR { ... }` (single Let element, no guards).
    LetChain {
        elements: Vec<ConditionElement>,
        span: Span,
    },
}

#[derive(Debug, Clone)]
pub struct IfStmt {
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
    /// Postfix `?` operator for error propagation on `Result<T, E>` and `Option<T>`.
    TryOp(Box<TryOpExpr>),
    /// Spread expression inside a tuple literal: `[..expr]`
    /// At compile time, splices the tuple's elements into the enclosing tuple.
    Spread(Box<Expr>, Span),
    /// Range expression: `a..<b` (exclusive) or `a..=b` (inclusive)
    Range(Box<RangeExpr>),
}

/// Labeled block expression: `label: { ... }` that produces a value
/// The value is the last expression in the block (if any).
#[derive(Debug, Clone)]
pub struct LabeledBlockExpr {
    pub label: String,
    pub block: Block,
    pub span: Span,
}

/// Postfix `?` operator: `expr?`
#[derive(Debug, Clone)]
pub struct TryOpExpr {
    pub expr: Expr,
    pub span: Span,
}

/// Range kind: exclusive (..<) or inclusive (..=)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RangeKind {
    Exclusive, // ..<
    Inclusive, // ..=
}

/// Range expression: `start..<end` or `start..=end`
#[derive(Debug, Clone)]
pub struct RangeExpr {
    pub start: Expr,
    pub end: Expr,
    pub kind: RangeKind,
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
            Expr::TryOp(e) => e.span,
            Expr::Spread(_, span) => *span,
            Expr::Range(e) => e.span,
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
            Expr::TryOp(mut e) => {
                e.span = new_span;
                Expr::TryOp(e)
            }
            Expr::Spread(inner, _) => Expr::Spread(inner, new_span),
            Expr::Range(mut e) => {
                e.span = new_span;
                Expr::Range(e)
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
    Add,    // +=
    Sub,    // -=
    Mul,    // *=
    Div,    // /=
    Mod,    // %=
    BitAnd, // &=
    BitOr,  // |=
    BitXor, // ^=
    Shl,    // <<=
    Shr,    // >>=
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
    /// Compile-time file include as string: `#include_str("path")`
    IncludeStr(String),
    /// Compile-time file include as bytes: `#include_bytes("path")`
    IncludeBytes(String),
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
    /// Explicit type arguments for generic methods: `Box::<i32>::wrap_other::<String>(x)`
    pub type_args: Vec<Type>,
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
    Tuple(Vec<Pattern>, /* has_rest */ bool),
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
    /// Or pattern: `Red | Blue` or `Some(x) | Other(x)`
    Or(Vec<Pattern>),
    /// Range pattern: `0..<10` or `'a'..='z'`
    Range {
        start: Box<Pattern>,
        end: Box<Pattern>,
        kind: RangeKind,
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
    pub is_mut: bool,
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
    /// Type pack spread inside a tuple: `..T` in `[i32, ..T, bool]`
    TypePackSpread(String, Span),
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
    /// Positional indices of parameters the function may store (e.g., `stores[0]`).
    pub stores: Vec<StoresEntry>,
}

/// A stores entry: either a parameter name (in function declarations) or a
/// positional index (in function type expressions).
#[derive(Debug, Clone)]
pub enum StoresEntry {
    Name(String),
    Index(u32),
}

impl std::fmt::Display for StoresEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoresEntry::Name(name) => write!(f, "{name}"),
            StoresEntry::Index(idx) => write!(f, "{idx}"),
        }
    }
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
/// Effect parameter declaration: `<effect E>` — represents a set of effects
#[derive(Debug, Clone)]
pub struct GenericParam {
    pub name: String,
    /// Whether this is an effect parameter (`effect E`)
    pub is_effect: bool,
    /// Whether this is a type pack parameter (`..T`)
    pub is_pack: bool,
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
    /// Attributes like `#[cm("...")]`
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct StructField {
    pub name: String,
    pub is_pub: bool,
    pub ty: Type,
    /// Attributes like `#[cm("...")]` for CM name override
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
    /// Attributes like #[cm("...")]
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

/// A case in an enum declaration.
/// Unlike `VariantCase`, enum cases have no payload.
#[derive(Debug, Clone)]
pub struct EnumCase {
    pub name: String,
    /// Attributes like `#[cm("wit-kebab-name")]` for CM name override
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
    /// Attributes like `#[cm("wit-kebab-name")]` for CM name override
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
    /// Attributes like `#[cm("...")]`
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
    /// Attributes like `#[cm("wit-kebab-name")]` for CM name override
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Newtype {
    pub name: String,
    pub is_pub: bool,
    pub type_params: Vec<GenericParam>,
    pub ty: Type,
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

/// Tuple type family declaration: `pub type [..T];`
///
/// Declares the current module as the owner of the tuple type family.
/// This is a type-system anchor that generates no code.
#[derive(Debug, Clone)]
pub struct TupleTypeDecl {
    pub is_pub: bool,
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

/// Associated type declaration in a trait: `type Output;` or `type Output: Trait1 + Trait2;`
#[derive(Debug, Clone)]
pub struct AssociatedTypeDecl {
    pub name: String,
    /// Trait bounds on this associated type (e.g., `SerializeSeq` in `type SeqSerializer: SerializeSeq;`
    /// or `Iterator<Item = Self::Item>` in `type Iter: Iterator<Item = Self::Item>;`)
    pub bounds: Vec<TraitBound>,
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
    pub attrs: Vec<Attribute>,
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
