// The parser implementation of Wado with recursive descent parser.
// This module must be synchronized with syntax.rs (canonical syntax definition).

use crate::ast::{
    AssertStmt, AssignExpr, AssociatedTypeBinding, AssociatedTypeDecl, Attribute, BinaryExpr,
    BinaryOp, Block, BreakStmt, CallExpr, CastExpr, ChainedComparison, ClosureExpr, ClosureParam,
    ComparisonChainExpr, CompoundAssignExpr, CompoundAssignOp, Condition, ContinueStmt, EffectDecl,
    EffectMethod, EnumCase, EnumDecl, Expr, ExprStmt, FieldAccessExpr, FlagsDecl, FlagsVariant,
    FloatLiteral, ForOfStmt, ForStmt, FormatSpec, Function, FunctionType, GenericType, GlobalDecl,
    IdentExpr, IfExpr, IfStmt, ImplBlock, ImportAttributes, IndexExpr, InnerAttribute, IntLiteral,
    Item, LabeledBlockStmt, LetStmt, Literal, LiteralExpr, LoopStmt, MethodCallExpr, Module,
    NamedType, NamespacedGenericType, Param, Pattern, ResourceDecl, ReturnStmt, SelfKind,
    StaticMethodCallExpr, Stmt, StructDecl, StructField, StructLiteralExpr, StructLiteralField,
    TestDecl, TraitDecl, TupleLiteralExpr, Type, TypeAlias, UnaryExpr, UnaryOp, UseDecl, UseItem,
    UseItemSimple, VariantCase, VariantDecl, WasiImport, WhileStmt, WorldDecl, WorldExport,
    WorldImport,
};
use crate::token::{Span, Token, TokenKind};

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// Tracks when we've split a `GtGt` into two Gt tokens for nested generics.
    /// When true, the next `expect_gt` call should succeed without consuming a token.
    pending_gt: bool,
    /// Shebang line, passed from the lexer.
    shebang: Option<String>,
    /// Content of the __DATA__ section, passed from the lexer.
    data_section: Option<String>,
}

#[derive(Debug)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
}

type ParseResult<T> = Result<T, ParseError>;

/// Groups of comparison operators for chain validation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComparisonChainGroup {
    /// < and <=
    Ascending,
    /// > and >=
    Descending,
    /// ==
    Equality,
    /// != (cannot be chained)
    NotEqual,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            pos: 0,
            pending_gt: false,
            shebang: None,
            data_section: None,
        }
    }

    /// Creates a new parser with the given tokens, shebang, and data section.
    pub fn with_metadata(
        tokens: Vec<Token>,
        shebang: Option<String>,
        data_section: Option<String>,
    ) -> Self {
        Self {
            tokens,
            pos: 0,
            pending_gt: false,
            shebang,
            data_section,
        }
    }

    /// Check if a name looks like a primitive type name (e.g., i32, u64, f32, i128, u128).
    /// This allows struct literals with these names: `u128 { low: 0, high: 0 }`
    fn looks_like_primitive_type(name: &str) -> bool {
        if name.len() < 2 {
            return false;
        }
        let mut chars = name.chars();
        match chars.next() {
            Some('i' | 'u' | 'f') => {
                let rest: String = chars.collect();
                !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
            }
            _ => false,
        }
    }

    pub fn parse(&mut self) -> ParseResult<Module> {
        // Parse inner attributes at the start of the module
        let inner_attributes = self.parse_inner_attributes()?;

        let mut items = Vec::new();

        while !self.is_at_end() {
            items.push(self.parse_item()?);
        }

        Ok(Module::with_metadata(
            items,
            inner_attributes,
            self.shebang.take(),
            self.data_section.take(),
        ))
    }

    // Token handling

    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn peek_kind(&self) -> &TokenKind {
        &self.tokens[self.pos].kind
    }

    /// Peek at the nth token ahead (0 = current, 1 = next, etc.)
    fn peek_nth(&self, n: usize) -> &Token {
        let idx = self.pos + n;
        if idx < self.tokens.len() {
            &self.tokens[idx]
        } else {
            // Return the last token (should be Eof)
            &self.tokens[self.tokens.len() - 1]
        }
    }

    fn is_at_end(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::Eof)
    }

    fn advance(&mut self) -> &Token {
        if !self.is_at_end() {
            self.pos += 1;
        }
        &self.tokens[self.pos - 1]
    }

    fn check(&self, kind: &TokenKind) -> bool {
        std::mem::discriminant(self.peek_kind()) == std::mem::discriminant(kind)
    }

    fn expect(&mut self, kind: &TokenKind) -> ParseResult<&Token> {
        if self.check(kind) {
            Ok(self.advance())
        } else {
            Err(ParseError {
                message: format!("expected {:?}, found {:?}", kind, self.peek_kind()),
                span: self.peek().span,
            })
        }
    }

    /// Expect a > token for closing generic type arguments.
    /// Handles the case where >> is lexed as `GtGt` instead of two separate Gt tokens.
    /// This is necessary for nested generics like Array<Tuple<String, String>>.
    fn expect_gt(&mut self) -> ParseResult<()> {
        // First check if we have a pending > from a previous GtGt split
        if self.pending_gt {
            self.pending_gt = false;
            return Ok(());
        }

        // Check for a regular Gt token
        if self.check(&TokenKind::Gt) {
            self.advance();
            return Ok(());
        }

        // Check for GtGt (>>) - split it into two Gt tokens
        if self.check(&TokenKind::GtGt) {
            self.advance();
            self.pending_gt = true; // Remember we have one more > to consume
            return Ok(());
        }

        Err(ParseError {
            message: format!("expected Gt, found {:?}", self.peek_kind()),
            span: self.peek().span,
        })
    }

    /// Create an error at a specific span
    fn error_at_span(&self, span: Span, message: &str) -> ParseError {
        ParseError {
            message: message.to_string(),
            span,
        }
    }

    fn consume_ident(&mut self) -> ParseResult<String> {
        match self.peek_kind().clone() {
            TokenKind::Ident(name) => {
                self.advance();
                Ok(name)
            }
            _ => Err(ParseError {
                message: format!("expected identifier, found {:?}", self.peek_kind()),
                span: self.peek().span,
            }),
        }
    }

    /// Consume an identifier or a keyword for use as a field name.
    /// Keywords are allowed as field names since they're unambiguous (receiver.field).
    fn consume_field_name(&mut self) -> ParseResult<String> {
        // First try identifier
        if let TokenKind::Ident(name) = self.peek_kind().clone() {
            self.advance();
            return Ok(name);
        }

        // Try keyword
        if let Some(keyword) = self.peek_kind().as_keyword_str() {
            self.advance();
            return Ok(keyword.to_string());
        }

        Err(ParseError {
            message: format!("expected field name, found {:?}", self.peek_kind()),
            span: self.peek().span,
        })
    }

    // Parsing

    fn parse_item(&mut self) -> ParseResult<Item> {
        // Parse any leading attributes
        let attrs = self.parse_attributes()?;

        let is_pub = if self.check(&TokenKind::Pub) {
            self.advance();
            true
        } else {
            false
        };

        // Check for contextual keyword "test" (identifier followed by string or block)
        if let TokenKind::Ident(name) = self.peek_kind()
            && name == "test"
        {
            return self.parse_test_decl().map(Item::Test);
        }

        match self.peek_kind() {
            TokenKind::Use => self.parse_use_decl(is_pub).map(Item::Use),
            TokenKind::Fn => self.parse_function(is_pub, attrs).map(Item::Function),
            TokenKind::Effect => self.parse_effect_decl(is_pub, attrs).map(Item::Effect),
            TokenKind::Struct => self.parse_struct_decl(is_pub).map(Item::Struct),
            TokenKind::Enum => self.parse_enum_decl(is_pub, attrs).map(Item::Enum),
            TokenKind::Variant => self.parse_variant_decl(is_pub).map(Item::Variant),
            TokenKind::Flags => self.parse_flags_decl(is_pub, attrs).map(Item::Flags),
            TokenKind::Type => self.parse_type_alias(is_pub).map(Item::Type),
            TokenKind::Impl => self.parse_impl_block().map(Item::Impl),
            TokenKind::Trait => self.parse_trait_decl(is_pub).map(Item::Trait),
            TokenKind::Resource => self.parse_resource_decl(attrs).map(Item::Resource),
            TokenKind::World => self.parse_world_decl().map(Item::World),
            TokenKind::Global => self.parse_global_decl(is_pub, attrs).map(Item::Global),
            _ => Err(ParseError {
                message: format!("expected item, found {:?}", self.peek_kind()),
                span: self.peek().span,
            }),
        }
    }

    /// Parse test declaration: `test "name" { ... }` or `test { ... }`
    fn parse_test_decl(&mut self) -> ParseResult<TestDecl> {
        let start_span = self.peek().span;
        // Consume the "test" identifier (contextual keyword)
        self.advance();

        // Optional test name (string literal)
        let name = if let TokenKind::StringLit(s) = self.peek_kind().clone() {
            self.advance();
            Some(s)
        } else {
            None
        };

        // Parse body block
        let body = self.parse_block()?;
        let end_span = body.span;

        Ok(TestDecl {
            name,
            body,
            span: start_span.merge(&end_span),
        })
    }

    /// Parse global variable declaration: `[pub] global [mut] name: Type = expr;`
    fn parse_global_decl(
        &mut self,
        is_pub: bool,
        attributes: Vec<Attribute>,
    ) -> ParseResult<GlobalDecl> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Global)?;

        // Optional mut
        let mutable = if self.check(&TokenKind::Mut) {
            self.advance();
            true
        } else {
            false
        };

        // Variable name
        let name = self.consume_ident()?;

        // Type annotation (required)
        self.expect(&TokenKind::Colon)?;
        let ty = self.parse_type()?;

        // Initializer (required)
        self.expect(&TokenKind::Eq)?;
        let initializer = self.parse_expr()?;

        let end_span = self.peek().span;
        self.expect(&TokenKind::Semicolon)?;

        Ok(GlobalDecl {
            name,
            ty,
            initializer,
            mutable,
            is_pub,
            attributes,
            span: start_span.merge(&end_span),
        })
    }

    fn parse_attributes(&mut self) -> ParseResult<Vec<Attribute>> {
        let mut attrs = Vec::new();

        while self.check(&TokenKind::Hash) {
            // Check if this is an inner attribute (#![...]) - if so, stop
            if self.peek_nth(1).kind == TokenKind::Not {
                break;
            }
            attrs.push(self.parse_attribute()?);
        }

        Ok(attrs)
    }

    /// Parse inner attributes at the start of a module: `#![name]`
    fn parse_inner_attributes(&mut self) -> ParseResult<Vec<InnerAttribute>> {
        let mut attrs = Vec::new();

        while self.check(&TokenKind::Hash) && self.peek_nth(1).kind == TokenKind::Not {
            attrs.push(self.parse_inner_attribute()?);
        }

        Ok(attrs)
    }

    /// Parse a single inner attribute: `#![name]`
    fn parse_inner_attribute(&mut self) -> ParseResult<InnerAttribute> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Hash)?;
        self.expect(&TokenKind::Not)?;
        self.expect(&TokenKind::LBracket)?;

        let name = self.consume_ident()?;

        self.expect(&TokenKind::RBracket)?;

        Ok(InnerAttribute {
            name,
            span: start_span,
        })
    }

    fn parse_attribute(&mut self) -> ParseResult<Attribute> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Hash)?;
        self.expect(&TokenKind::LBracket)?;

        let name = self.consume_ident()?;

        let args = if self.check(&TokenKind::LParen) {
            self.advance();
            // Parse comma-separated string literal arguments
            let mut args = Vec::new();
            while let TokenKind::StringLit(s) = self.peek_kind().clone() {
                self.advance();
                args.push(s);
                // Check for comma to continue parsing more arguments
                if self.check(&TokenKind::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
            self.expect(&TokenKind::RParen)?;
            args
        } else {
            Vec::new()
        };

        self.expect(&TokenKind::RBracket)?;

        // Parse WASI import path if this is a wasi attribute
        let wasi_import = if name == "wasi" {
            args.first().and_then(|s| WasiImport::parse(s))
        } else {
            None
        };

        Ok(Attribute {
            name,
            args,
            wasi_import,
            span: start_span,
        })
    }

    fn parse_resource_decl(&mut self, attrs: Vec<Attribute>) -> ParseResult<ResourceDecl> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Resource)?;
        let name = self.consume_ident()?;

        // Either `resource Name;` (opaque) or `resource Name { ... }` (with methods)
        let (methods, end_span) = if self.check(&TokenKind::LBrace) {
            self.advance(); // consume '{'

            let mut methods = Vec::new();
            while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
                // Reuse effect method parser for resource methods
                methods.push(self.parse_effect_method()?);
            }

            let end = self.expect(&TokenKind::RBrace)?.span;
            (methods, end)
        } else {
            let end = self.expect(&TokenKind::Semicolon)?.span;
            (Vec::new(), end)
        };

        Ok(ResourceDecl {
            name,
            attrs,
            methods,
            span: start_span.merge(&end_span),
        })
    }

    /// Parse use declaration with ESM-like syntax:
    /// `use {items} from "source";`
    /// `use {items} from "source" with { version: "1.0" };`
    ///
    /// Items can be:
    /// - Simple: `name` or `name as alias`
    /// - Effect functions: `Effect::{func1, func2}` or `Effect::{func1 as alias}`
    fn parse_use_decl(&mut self, is_pub: bool) -> ParseResult<UseDecl> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Use)?;

        // Parse items: `{...}`
        self.expect(&TokenKind::LBrace)?;
        let items = self.parse_use_items()?;
        self.expect(&TokenKind::RBrace)?;

        // Expect `from`
        self.expect(&TokenKind::From)?;

        // Parse source string
        let source = self.consume_string()?;

        // Parse optional `with { ... }` attributes
        let attributes = if self.check(&TokenKind::With) {
            self.advance();
            Some(self.parse_import_attributes()?)
        } else {
            None
        };

        self.expect(&TokenKind::Semicolon)?;

        Ok(UseDecl {
            is_pub,
            source,
            items,
            attributes,
            span: start_span,
        })
    }

    /// Parse use items inside `{...}`
    fn parse_use_items(&mut self) -> ParseResult<Vec<UseItem>> {
        let mut items = vec![];

        if self.check(&TokenKind::RBrace) {
            return Ok(items);
        }

        loop {
            let name = self.consume_ident()?;

            // Check if this is an effect with functions: `Effect::{...}`
            if self.check(&TokenKind::ColonColon) {
                self.advance(); // consume `::`
                self.expect(&TokenKind::LBrace)?;

                // Parse function list inside Effect::{...}
                let functions = self.parse_use_item_simple_list()?;
                self.expect(&TokenKind::RBrace)?;

                items.push(UseItem::EffectFunctions {
                    effect_name: name,
                    functions,
                });
            } else {
                // Simple import, possibly with alias
                let alias = if self.check(&TokenKind::As) {
                    self.advance();
                    Some(self.consume_ident()?)
                } else {
                    None
                };
                items.push(UseItem::Simple { name, alias });
            }

            if !self.check(&TokenKind::Comma) {
                break;
            }
            self.advance(); // consume comma
            if self.check(&TokenKind::RBrace) {
                break; // trailing comma allowed
            }
        }

        Ok(items)
    }

    /// Parse simple use items (name or name as alias) for use inside `Effect::`{...}
    fn parse_use_item_simple_list(&mut self) -> ParseResult<Vec<UseItemSimple>> {
        let mut items = vec![];

        if self.check(&TokenKind::RBrace) {
            return Ok(items);
        }

        loop {
            let name = self.consume_ident()?;
            let alias = if self.check(&TokenKind::As) {
                self.advance();
                Some(self.consume_ident()?)
            } else {
                None
            };
            items.push(UseItemSimple { name, alias });

            if !self.check(&TokenKind::Comma) {
                break;
            }
            self.advance(); // consume comma
            if self.check(&TokenKind::RBrace) {
                break; // trailing comma allowed
            }
        }

        Ok(items)
    }

    /// Parse import attributes: `{ version: "1.0", integrity: "sha384-..." }`
    fn parse_import_attributes(&mut self) -> ParseResult<ImportAttributes> {
        self.expect(&TokenKind::LBrace)?;

        let mut attrs = ImportAttributes::default();

        if !self.check(&TokenKind::RBrace) {
            loop {
                let key = self.consume_ident()?;
                self.expect(&TokenKind::Colon)?;
                let value = self.consume_string()?;

                match key.as_str() {
                    "version" => attrs.version = Some(value),
                    "integrity" => attrs.integrity = Some(value),
                    "type" => attrs.type_hint = Some(value),
                    _ => {
                        return Err(ParseError {
                            message: format!("unknown import attribute: {key}"),
                            span: self.peek().span,
                        });
                    }
                }

                if !self.check(&TokenKind::Comma) {
                    break;
                }
                self.advance();
                if self.check(&TokenKind::RBrace) {
                    break;
                }
            }
        }

        self.expect(&TokenKind::RBrace)?;
        Ok(attrs)
    }

    /// Consume a string literal and return its value
    fn consume_string(&mut self) -> ParseResult<String> {
        match &self.peek().kind {
            TokenKind::StringLit(s) => {
                let s = s.clone();
                self.advance();
                Ok(s)
            }
            _ => Err(ParseError {
                message: "expected string literal".to_string(),
                span: self.peek().span,
            }),
        }
    }

    fn parse_function(&mut self, is_pub: bool, attrs: Vec<Attribute>) -> ParseResult<Function> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Fn)?;

        let name = self.consume_ident()?;

        // Parse generic parameters like <T, U> or <T: Ord>
        let type_params = self.parse_generic_params()?;

        self.expect(&TokenKind::LParen)?;
        let params = self.parse_param_list()?;
        self.expect(&TokenKind::RParen)?;

        let return_type = if self.check(&TokenKind::Arrow) {
            self.advance();
            Some(self.parse_type()?)
        } else {
            None
        };

        let effects = if self.check(&TokenKind::With) {
            self.advance();
            self.parse_effect_list()?
        } else {
            Vec::new()
        };

        // Check for bodyless function declaration (compiler built-in)
        // e.g., `pub fn stream_new() -> i64;`
        let body = if self.check(&TokenKind::Semicolon) {
            self.advance();
            None
        } else {
            Some(self.parse_block()?)
        };

        let span = body
            .as_ref()
            .map_or(start_span, |b| start_span.merge(&b.span));

        Ok(Function {
            name,
            is_pub,
            type_params,
            attrs,
            params,
            return_type,
            effects,
            body,
            span,
        })
    }

    fn parse_param_list(&mut self) -> ParseResult<Vec<Param>> {
        let mut params = Vec::new();

        if !self.check(&TokenKind::RParen) {
            params.push(self.parse_param()?);

            while self.check(&TokenKind::Comma) {
                self.advance();
                if self.check(&TokenKind::RParen) {
                    break;
                }
                params.push(self.parse_param()?);
            }
        }

        Ok(params)
    }

    fn parse_param(&mut self) -> ParseResult<Param> {
        let start_span = self.peek().span;

        // Handle &self and &mut self for methods
        if self.check(&TokenKind::Ampersand) {
            self.advance();

            // Check for &mut self
            let is_mut = if self.check(&TokenKind::Mut) {
                self.advance();
                true
            } else {
                false
            };

            // Expect "self" identifier
            if let TokenKind::Ident(name) = self.peek_kind()
                && name == "self"
            {
                self.advance();
                let self_type = Type::Named(NamedType {
                    name: "Self".to_string(),
                    span: start_span,
                });
                let ty = if is_mut {
                    Type::MutReference(Box::new(self_type))
                } else {
                    Type::Reference(Box::new(self_type))
                };
                return Ok(Param {
                    name: "self".to_string(),
                    ty,
                    self_kind: if is_mut {
                        SelfKind::MutRef
                    } else {
                        SelfKind::Ref
                    },
                    span: start_span,
                });
            }

            return Err(ParseError {
                message: "expected 'self' after '&' in method parameter".to_string(),
                span: self.peek().span,
            });
        }

        let name = self.consume_ident()?;
        self.expect(&TokenKind::Colon)?;
        let ty = self.parse_type()?;

        Ok(Param {
            name,
            ty,
            self_kind: SelfKind::None,
            span: start_span,
        })
    }

    fn parse_effect_list(&mut self) -> ParseResult<Vec<String>> {
        let mut effects = vec![self.consume_ident()?];

        while self.check(&TokenKind::Comma) {
            self.advance();
            effects.push(self.consume_ident()?);
        }

        Ok(effects)
    }

    fn parse_block(&mut self) -> ParseResult<Block> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::LBrace)?;

        let mut stmts = Vec::new();

        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            // Try to parse a statement, allowing optional trailing semicolon for final expression
            let stmt = self.parse_stmt_in_block()?;
            stmts.push(stmt);
        }

        let end_span = self.expect(&TokenKind::RBrace)?.span;

        Ok(Block {
            stmts,
            span: start_span.merge(&end_span),
        })
    }

    /// Parse a statement inside a block, allowing optional semicolon for the final expression
    fn parse_stmt_in_block(&mut self) -> ParseResult<Stmt> {
        // Check for labeled block: `LABEL: { ... }`
        if let TokenKind::Ident(_) = self.peek_kind()
            && matches!(self.peek_nth(1).kind, TokenKind::Colon)
            && matches!(self.peek_nth(2).kind, TokenKind::LBrace)
        {
            return self.parse_labeled_block_stmt();
        }

        match self.peek_kind() {
            TokenKind::Let | TokenKind::Reactive => self.parse_let_stmt(),
            TokenKind::Return => self.parse_return_stmt(),
            TokenKind::If => self.parse_if_stmt(),
            TokenKind::While => self.parse_while_stmt(),
            TokenKind::For => self.parse_for_stmt(),
            TokenKind::Loop => self.parse_loop_stmt(),
            TokenKind::Break => self.parse_break_stmt(),
            TokenKind::Continue => self.parse_continue_stmt(),
            TokenKind::Assert => self.parse_assert_stmt(),
            _ => self.parse_expr_stmt_in_block(),
        }
    }

    /// Parse an expression statement in a block, with optional trailing semicolon
    fn parse_expr_stmt_in_block(&mut self) -> ParseResult<Stmt> {
        let start_span = self.peek().span;
        let expr = self.parse_expr()?;

        // Semicolon is optional if followed by `}` (end of block)
        if self.check(&TokenKind::Semicolon) {
            self.advance();
        } else if !self.check(&TokenKind::RBrace) {
            return Err(ParseError {
                message: format!("expected `;` or `}}`, found {:?}", self.peek_kind()),
                span: self.peek().span,
            });
        }

        Ok(Stmt::Expr(ExprStmt {
            expr,
            span: start_span,
        }))
    }

    fn parse_labeled_block_stmt(&mut self) -> ParseResult<Stmt> {
        let start_span = self.peek().span;

        // Parse the label (identifier)
        let label = match self.advance().kind.clone() {
            TokenKind::Ident(name) => name,
            _ => unreachable!("parse_labeled_block_stmt called without identifier"),
        };

        // Consume the colon
        self.expect(&TokenKind::Colon)?;

        // Parse the block
        let block = self.parse_block()?;
        let end_span = block.span;

        Ok(Stmt::LabeledBlock(LabeledBlockStmt {
            label,
            block,
            span: start_span.merge(&end_span),
        }))
    }

    fn parse_assert_stmt(&mut self) -> ParseResult<Stmt> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Assert)?;

        let condition = self.parse_expr()?;

        // Check for optional message after comma
        let message = if self.check(&TokenKind::Comma) {
            self.advance(); // consume comma
            Some(self.parse_expr()?)
        } else {
            None
        };

        self.expect(&TokenKind::Semicolon)?;

        Ok(Stmt::Assert(AssertStmt {
            condition,
            message,
            span: start_span,
        }))
    }

    fn parse_let_stmt(&mut self) -> ParseResult<Stmt> {
        let stmt = self.parse_let_stmt_inner()?;
        self.expect(&TokenKind::Semicolon)?;
        Ok(stmt)
    }

    /// Parse let statement without consuming trailing semicolon
    /// Used in for loop init: `for (let mut i = 0; ...)`
    fn parse_let_stmt_inner(&mut self) -> ParseResult<Stmt> {
        let start_span = self.peek().span;

        let is_reactive = if self.check(&TokenKind::Reactive) {
            self.advance();
            true
        } else {
            false
        };

        self.expect(&TokenKind::Let)?;

        let is_mut = if self.check(&TokenKind::Mut) {
            self.advance();
            true
        } else {
            false
        };

        let pattern = self.parse_let_pattern()?;

        let ty = if self.check(&TokenKind::Colon) {
            self.advance();
            Some(self.parse_type()?)
        } else {
            None
        };

        self.expect(&TokenKind::Eq)?;
        let value = self.parse_expr()?;

        Ok(Stmt::Let(LetStmt {
            pattern,
            is_mut,
            is_reactive,
            ty,
            value,
            span: start_span,
        }))
    }

    fn parse_return_stmt(&mut self) -> ParseResult<Stmt> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Return)?;

        let value = if self.check(&TokenKind::Semicolon) {
            None
        } else {
            Some(self.parse_expr()?)
        };

        self.expect(&TokenKind::Semicolon)?;

        Ok(Stmt::Return(ReturnStmt {
            value,
            span: start_span,
        }))
    }

    fn parse_if_stmt(&mut self) -> ParseResult<Stmt> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::If)?;

        let mut init = None;
        let condition;

        // Check for if-let: either Rust-style pattern matching or Go-style init
        if self.check(&TokenKind::Let) {
            self.advance(); // consume 'let'

            // Check if this is Rust-style pattern matching (uppercase identifier = pattern start)
            if self.is_variant_pattern_start() {
                // Rust-style: `if let Some(x) = expr { ... }` or `if let None = expr { ... }`
                let pattern_span = self.peek().span;
                let pattern = self.parse_pattern()?;
                self.expect(&TokenKind::Eq)?;
                let expr = self.parse_expr()?;
                let span = pattern_span.merge(&expr.span());
                condition = Condition::Pattern {
                    pattern,
                    expr,
                    span,
                };
            } else {
                // Go-style: `if let x = expr; condition { ... }`
                // We already consumed 'let', need to parse variable declaration
                let let_stmt = self.parse_let_stmt_after_let()?;
                self.expect(&TokenKind::Semicolon)?;
                init = Some(Box::new(let_stmt));
                condition = Condition::Expr(self.parse_expr()?);
            }
        } else {
            // No 'let', just a regular condition expression
            condition = Condition::Expr(self.parse_expr()?);
        }

        let then_block = self.parse_block()?;

        let else_block = if self.check(&TokenKind::Else) {
            self.advance();
            if self.check(&TokenKind::If) {
                // `else if` - parse as nested if statement wrapped in a block
                let if_stmt = self.parse_if_stmt()?;
                let span = match &if_stmt {
                    Stmt::If(s) => s.span,
                    _ => unreachable!("parse_if_stmt must return Stmt::If"),
                };
                Some(Block {
                    stmts: vec![if_stmt],
                    span,
                })
            } else {
                Some(self.parse_block()?)
            }
        } else {
            None
        };

        let end_span = else_block.as_ref().map_or(&then_block.span, |b| &b.span);
        let span = start_span.merge(end_span);

        Ok(Stmt::If(IfStmt {
            init,
            condition,
            then_block,
            else_block,
            span,
        }))
    }

    /// Check if the next tokens look like a variant pattern start.
    /// Detects patterns by presence of parentheses after identifier: `Some(`.
    /// This detects patterns structurally, not by naming convention.
    /// Note: Unit variants like `None` are detected by uppercase first letter convention.
    fn is_variant_pattern_start(&self) -> bool {
        if let TokenKind::Ident(name) = self.peek_kind() {
            let next = self.peek_nth(1);
            // Check if identifier is followed by `(` (variant with payload like `Some(x)`)
            if matches!(next.kind, TokenKind::LParen) {
                return true;
            }
            // For unit variants like `None`, check if identifier starts with uppercase
            // and is followed by `=` (to distinguish from Go-style `let x = value`)
            if matches!(next.kind, TokenKind::Eq)
                && let Some(first_char) = name.chars().next()
            {
                return first_char.is_uppercase();
            }
            false
        } else {
            false
        }
    }

    /// Parse the rest of a let statement after the 'let' keyword has been consumed
    fn parse_let_stmt_after_let(&mut self) -> ParseResult<LetStmt> {
        let start_span = self.peek().span;

        let is_mut = if self.check(&TokenKind::Mut) {
            self.advance();
            true
        } else {
            false
        };

        let pattern = self.parse_let_pattern()?;

        let ty = if self.check(&TokenKind::Colon) {
            self.advance();
            Some(self.parse_type()?)
        } else {
            None
        };

        self.expect(&TokenKind::Eq)?;
        let value = self.parse_expr()?;

        Ok(LetStmt {
            pattern,
            is_mut,
            is_reactive: false,
            ty,
            value,
            span: start_span,
        })
    }

    fn parse_while_stmt(&mut self) -> ParseResult<Stmt> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::While)?;

        // Check for while-let: `while let Some(x) = expr { ... }`
        let condition = if self.check(&TokenKind::Let) {
            self.advance(); // consume 'let'

            if self.is_variant_pattern_start() {
                // Rust-style: `while let Some(x) = expr { ... }`
                let pattern_span = self.peek().span;
                let pattern = self.parse_pattern()?;
                self.expect(&TokenKind::Eq)?;
                let expr = self.parse_expr()?;
                let span = pattern_span.merge(&expr.span());
                Condition::Pattern {
                    pattern,
                    expr,
                    span,
                }
            } else {
                return Err(ParseError {
                    message: "expected pattern after 'while let'".to_string(),
                    span: self.peek().span,
                });
            }
        } else {
            Condition::Expr(self.parse_expr()?)
        };

        let body = self.parse_block()?;
        let span = start_span.merge(&body.span);

        Ok(Stmt::While(WhileStmt {
            condition,
            body,
            span,
        }))
    }

    /// Parse for loop: either C-style or for-of
    /// - C-style: `for (init; condition; update) { body }` or `for init; condition; update { body }`
    /// - For-of: `for let item of array { body }`
    fn parse_for_stmt(&mut self) -> ParseResult<Stmt> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::For)?;

        // Check for for-of syntax: `for let [mut] item of array { ... }`
        if self.check(&TokenKind::Let) {
            // Save position for potential backtrack
            let saved_pos = self.pos;

            self.advance(); // consume 'let'

            // Check for optional 'mut'
            let is_mut = self.check(&TokenKind::Mut);
            if is_mut {
                self.advance();
            }

            // Check if identifier followed by 'of'
            if let TokenKind::Ident(binding) = &self.peek().kind {
                let binding = binding.clone();
                self.advance(); // consume identifier

                // Check if next token is 'of'
                if matches!(self.peek().kind, TokenKind::Of) {
                    // This is a for-of loop
                    self.advance(); // consume 'of'
                    let iterable = self.parse_expr()?;
                    let body = self.parse_block()?;
                    let span = start_span.merge(&body.span);

                    return Ok(Stmt::ForOf(ForOfStmt {
                        binding,
                        is_mut,
                        iterable,
                        body,
                        span,
                    }));
                }
            }

            // Not a for-of loop, backtrack and parse as C-style for
            self.pos = saved_pos;
        }

        // Parentheses are optional for C-style for
        let has_parens = self.check(&TokenKind::LParen);
        if has_parens {
            self.advance();
        }

        // Parse init (optional): `let i = 0` or expression statement
        let init = if self.check(&TokenKind::Semicolon) {
            None
        } else if self.check(&TokenKind::Let) || self.check(&TokenKind::Reactive) {
            Some(Box::new(self.parse_let_stmt_inner()?))
        } else {
            let expr = self.parse_expr()?;
            Some(Box::new(Stmt::Expr(ExprStmt {
                expr,
                span: start_span,
            })))
        };
        self.expect(&TokenKind::Semicolon)?;

        // Parse condition (optional): `i < 10` or `let Some(x) = expr`
        let condition = if self.check(&TokenKind::Semicolon) {
            None
        } else if self.check(&TokenKind::Let) {
            // Pattern condition: `let Some(x) = iter.next()`
            self.advance(); // consume 'let'
            if self.is_variant_pattern_start() {
                let pattern_span = self.peek().span;
                let pattern = self.parse_pattern()?;
                self.expect(&TokenKind::Eq)?;
                let expr = self.parse_expr()?;
                let span = pattern_span.merge(&expr.span());
                Some(Condition::Pattern {
                    pattern,
                    expr,
                    span,
                })
            } else {
                return Err(ParseError {
                    message: "expected pattern after 'let' in for condition".to_string(),
                    span: self.peek().span,
                });
            }
        } else {
            Some(Condition::Expr(self.parse_expr()?))
        };
        self.expect(&TokenKind::Semicolon)?;

        // Parse update (optional): `i = i + 1`
        // Without parens, update ends at `{`; with parens, it ends at `)`
        let update = if has_parens {
            if self.check(&TokenKind::RParen) {
                None
            } else {
                Some(self.parse_expr()?)
            }
        } else if self.check(&TokenKind::LBrace) {
            None
        } else {
            Some(self.parse_expr()?)
        };

        if has_parens {
            self.expect(&TokenKind::RParen)?;
        }

        let body = self.parse_block()?;
        let span = start_span.merge(&body.span);

        Ok(Stmt::For(ForStmt {
            init,
            condition,
            update,
            body,
            span,
        }))
    }

    /// Parse infinite loop: `loop { body }`
    fn parse_loop_stmt(&mut self) -> ParseResult<Stmt> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Loop)?;

        let body = self.parse_block()?;
        let span = start_span.merge(&body.span);

        Ok(Stmt::Loop(LoopStmt { body, span }))
    }

    /// Parse break statement: `break;`, `break label;`, or `break label: expr;`
    fn parse_break_stmt(&mut self) -> ParseResult<Stmt> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Break)?;

        // Check for optional label
        let (label, value) = if let TokenKind::Ident(name) = self.peek_kind().clone() {
            self.advance();
            // Check for colon followed by expression (break with value)
            if self.check(&TokenKind::Colon) {
                self.advance(); // consume ':'
                let expr = self.parse_expr()?;
                (Some(name), Some(Box::new(expr)))
            } else {
                // Just a label, no value
                (Some(name), None)
            }
        } else {
            // No label, no value
            (None, None)
        };

        self.expect(&TokenKind::Semicolon)?;

        Ok(Stmt::Break(BreakStmt {
            label,
            value,
            span: start_span,
        }))
    }

    /// Parse continue statement: `continue;`
    fn parse_continue_stmt(&mut self) -> ParseResult<Stmt> {
        let span = self.peek().span;
        self.expect(&TokenKind::Continue)?;
        self.expect(&TokenKind::Semicolon)?;

        Ok(Stmt::Continue(ContinueStmt { span }))
    }

    fn parse_pattern(&mut self) -> ParseResult<Pattern> {
        if self.check(&TokenKind::LParen) {
            // Tuple pattern with parentheses: (a, b, c)
            self.advance();
            let mut patterns = Vec::new();
            if !self.check(&TokenKind::RParen) {
                patterns.push(self.parse_pattern()?);
                while self.check(&TokenKind::Comma) {
                    self.advance();
                    if self.check(&TokenKind::RParen) {
                        break;
                    }
                    patterns.push(self.parse_pattern()?);
                }
            }
            self.expect(&TokenKind::RParen)?;
            Ok(Pattern::Tuple(patterns))
        } else if self.check(&TokenKind::LBracket) {
            // Tuple pattern with brackets: [a, b, c]
            self.advance();
            let mut patterns = Vec::new();
            if !self.check(&TokenKind::RBracket) {
                patterns.push(self.parse_pattern()?);
                while self.check(&TokenKind::Comma) {
                    self.advance();
                    if self.check(&TokenKind::RBracket) {
                        break;
                    }
                    patterns.push(self.parse_pattern()?);
                }
            }
            self.expect(&TokenKind::RBracket)?;
            Ok(Pattern::Tuple(patterns))
        } else if let TokenKind::Ident(name) = self.peek_kind().clone() {
            let start_span = self.peek().span;
            self.advance();
            if name == "_" {
                Ok(Pattern::Wildcard)
            } else if name.chars().next().is_some_and(char::is_uppercase) {
                // Uppercase identifier - could be variant pattern like Some(x) or None
                if self.check(&TokenKind::LParen) {
                    // Variant with bindings: Some(x)
                    self.advance(); // consume (
                    let mut bindings = Vec::new();
                    if !self.check(&TokenKind::RParen) {
                        bindings.push(self.parse_pattern()?);
                        while self.check(&TokenKind::Comma) {
                            self.advance();
                            if self.check(&TokenKind::RParen) {
                                break;
                            }
                            bindings.push(self.parse_pattern()?);
                        }
                    }
                    let end_span = self.peek().span;
                    self.expect(&TokenKind::RParen)?;
                    Ok(Pattern::Variant {
                        variant_name: name,
                        bindings,
                        span: start_span.merge(&end_span),
                    })
                } else {
                    // Variant without bindings: None
                    Ok(Pattern::Variant {
                        variant_name: name,
                        bindings: vec![],
                        span: start_span,
                    })
                }
            } else {
                Ok(Pattern::Ident(name))
            }
        } else {
            Err(ParseError {
                message: format!("expected pattern, found {:?}", self.peek_kind()),
                span: self.peek().span,
            })
        }
    }

    /// Parse a pattern for let statements
    /// Supports: identifier, wildcard `_`, and tuple pattern `[a, b, c]`
    fn parse_let_pattern(&mut self) -> ParseResult<Pattern> {
        if self.check(&TokenKind::LBracket) {
            // Tuple pattern: [a, b, c]
            self.advance();
            let mut patterns = Vec::new();
            if !self.check(&TokenKind::RBracket) {
                patterns.push(self.parse_let_pattern()?);
                while self.check(&TokenKind::Comma) {
                    self.advance();
                    if self.check(&TokenKind::RBracket) {
                        break;
                    }
                    patterns.push(self.parse_let_pattern()?);
                }
            }
            self.expect(&TokenKind::RBracket)?;
            Ok(Pattern::Tuple(patterns))
        } else if let TokenKind::Ident(name) = self.peek_kind().clone() {
            self.advance();
            if name == "_" {
                Ok(Pattern::Wildcard)
            } else {
                Ok(Pattern::Ident(name))
            }
        } else {
            Err(ParseError {
                message: format!(
                    "expected identifier or tuple pattern, found {:?}",
                    self.peek_kind()
                ),
                span: self.peek().span,
            })
        }
    }

    // Expression parsing with precedence climbing

    fn parse_expr(&mut self) -> ParseResult<Expr> {
        self.parse_assignment_expr()
    }

    /// Parse assignment expression: `target = value` or `target op= value`
    /// Assignment has lowest precedence and is right-associative
    /// Compound assignments (+=, -=, *=, /=, %=) are desugared to `target = target op value`
    fn parse_assignment_expr(&mut self) -> ParseResult<Expr> {
        let start_span = self.peek().span;
        let expr = self.parse_or_expr()?;

        // Check for simple assignment
        if self.check(&TokenKind::Eq) {
            self.advance();
            let value = self.parse_assignment_expr()?; // Right-associative
            return Ok(Expr::Assign(Box::new(AssignExpr {
                target: expr,
                value,
                span: start_span,
            })));
        }

        // Check for compound assignment operators
        let compound_op = match self.peek().kind {
            TokenKind::PlusEq => Some(CompoundAssignOp::Add),
            TokenKind::MinusEq => Some(CompoundAssignOp::Sub),
            TokenKind::StarEq => Some(CompoundAssignOp::Mul),
            TokenKind::SlashEq => Some(CompoundAssignOp::Div),
            TokenKind::PercentEq => Some(CompoundAssignOp::Mod),
            _ => None,
        };

        if let Some(op) = compound_op {
            self.advance();
            let value = self.parse_assignment_expr()?; // Right-associative
            let value_span = value.span();

            return Ok(Expr::CompoundAssign(Box::new(CompoundAssignExpr {
                target: expr,
                op,
                value,
                span: start_span.merge(&value_span),
            })));
        }

        Ok(expr)
    }

    fn parse_or_expr(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_and_expr()?;

        while self.check(&TokenKind::Or) {
            let left_span = left.span();
            self.advance();
            let right = self.parse_and_expr()?;
            let merged_span = left_span.merge(&right.span());
            left = Expr::Binary(Box::new(BinaryExpr {
                left,
                op: BinaryOp::Or,
                right,
                span: merged_span,
            }));
        }

        Ok(left)
    }

    fn parse_and_expr(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_comparison_expr()?;

        while self.check(&TokenKind::And) {
            let left_span = left.span();
            self.advance();
            let right = self.parse_comparison_expr()?;
            let merged_span = left_span.merge(&right.span());
            left = Expr::Binary(Box::new(BinaryExpr {
                left,
                op: BinaryOp::And,
                right,
                span: merged_span,
            }));
        }

        Ok(left)
    }

    /// Parse comparison expressions with chaining support.
    /// All comparison operators (==, !=, <, <=, >, >=) are at the same precedence level.
    /// Chaining like `a < b < c` is preserved as `ComparisonChainExpr`.
    ///
    /// Chaining rules:
    /// - `<`/`<=` can only chain with `<`/`<=` (ascending)
    /// - `>`/`>=` can only chain with `>`/`>=` (descending)
    /// - `==` can only chain with `==`
    /// - `!=` cannot be chained at all
    fn parse_comparison_expr(&mut self) -> ParseResult<Expr> {
        let first = self.parse_bitor_expr()?;

        // Check if we have a comparison operator
        let first_op = match self.peek_kind() {
            TokenKind::EqEq => Some(BinaryOp::Eq),
            TokenKind::NotEq => Some(BinaryOp::NotEq),
            TokenKind::Lt => Some(BinaryOp::Lt),
            TokenKind::LtEq => Some(BinaryOp::LtEq),
            TokenKind::Gt => Some(BinaryOp::Gt),
            TokenKind::GtEq => Some(BinaryOp::GtEq),
            _ => None,
        };

        if first_op.is_none() {
            return Ok(first);
        }

        // Parse first comparison
        let first_op = first_op.unwrap();
        let first_op_span = self.peek().span;
        let first_span = first.span();
        self.advance();
        let second = self.parse_bitor_expr()?;

        // Collect comparisons
        let mut comparisons = vec![ChainedComparison {
            op: first_op,
            right: second.clone(),
            op_span: first_op_span,
        }];

        // Determine chain group from first operator
        let chain_group = Self::comparison_chain_group(first_op);

        // Check for chained comparisons (e.g., a < b < c)
        let mut current = second;
        loop {
            let next_op = match self.peek_kind() {
                TokenKind::EqEq => Some(BinaryOp::Eq),
                TokenKind::NotEq => Some(BinaryOp::NotEq),
                TokenKind::Lt => Some(BinaryOp::Lt),
                TokenKind::LtEq => Some(BinaryOp::LtEq),
                TokenKind::Gt => Some(BinaryOp::Gt),
                TokenKind::GtEq => Some(BinaryOp::GtEq),
                _ => break,
            };

            let next_op = next_op.unwrap();
            let next_op_span = self.peek().span;

            // Validate chain
            // Rule: != cannot be chained
            if first_op == BinaryOp::NotEq {
                return Err(self.error_at_span(
                    next_op_span,
                    "!= operator cannot be chained; use explicit && instead: `a != b && b != c`",
                ));
            }
            if next_op == BinaryOp::NotEq {
                return Err(self.error_at_span(
                    next_op_span,
                    "!= operator cannot be chained; use explicit && instead: `a != b && b != c`",
                ));
            }

            // Rule: operators must be in the same group
            let next_group = Self::comparison_chain_group(next_op);
            if chain_group != next_group {
                let msg = match (chain_group, next_group) {
                    (ComparisonChainGroup::Ascending, ComparisonChainGroup::Descending)
                    | (ComparisonChainGroup::Descending, ComparisonChainGroup::Ascending) => {
                        "cannot mix ascending (<, <=) and descending (>, >=) comparisons in a chain"
                    }
                    (ComparisonChainGroup::Equality, _) | (_, ComparisonChainGroup::Equality) => {
                        "cannot mix == with inequality operators in a comparison chain"
                    }
                    _ => "invalid comparison chain: operators must be in the same direction",
                };
                return Err(self.error_at_span(next_op_span, msg));
            }

            self.advance();
            let right = self.parse_bitor_expr()?;

            comparisons.push(ChainedComparison {
                op: next_op,
                right: right.clone(),
                op_span: next_op_span,
            });

            current = right;
        }

        // If only one comparison, return a simple binary expression
        if comparisons.len() == 1 {
            let cmp = comparisons.pop().unwrap();
            let merged_span = first_span.merge(&cmp.right.span());
            return Ok(Expr::Binary(Box::new(BinaryExpr {
                left: first,
                op: cmp.op,
                right: cmp.right,
                span: merged_span,
            })));
        }

        // Multiple comparisons: return a ComparisonChainExpr
        let full_span = first_span.merge(&current.span());
        Ok(Expr::ComparisonChain(Box::new(ComparisonChainExpr {
            first,
            comparisons,
            span: full_span,
        })))
    }

    /// Determine the chain group for a comparison operator
    fn comparison_chain_group(op: BinaryOp) -> ComparisonChainGroup {
        match op {
            BinaryOp::Lt | BinaryOp::LtEq => ComparisonChainGroup::Ascending,
            BinaryOp::Gt | BinaryOp::GtEq => ComparisonChainGroup::Descending,
            BinaryOp::Eq => ComparisonChainGroup::Equality,
            BinaryOp::NotEq => ComparisonChainGroup::NotEqual,
            _ => unreachable!("not a comparison operator"),
        }
    }

    fn parse_bitor_expr(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_bitxor_expr()?;

        while self.check(&TokenKind::Pipe) {
            let left_span = left.span();
            self.advance();
            let right = self.parse_bitxor_expr()?;
            let merged_span = left_span.merge(&right.span());
            left = Expr::Binary(Box::new(BinaryExpr {
                left,
                op: BinaryOp::BitOr,
                right,
                span: merged_span,
            }));
        }

        Ok(left)
    }

    fn parse_bitxor_expr(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_bitand_expr()?;

        while self.check(&TokenKind::Caret) {
            let left_span = left.span();
            self.advance();
            let right = self.parse_bitand_expr()?;
            let merged_span = left_span.merge(&right.span());
            left = Expr::Binary(Box::new(BinaryExpr {
                left,
                op: BinaryOp::BitXor,
                right,
                span: merged_span,
            }));
        }

        Ok(left)
    }

    fn parse_bitand_expr(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_shift_expr()?;

        while self.check(&TokenKind::Ampersand) {
            let left_span = left.span();
            self.advance();
            let right = self.parse_shift_expr()?;
            let merged_span = left_span.merge(&right.span());
            left = Expr::Binary(Box::new(BinaryExpr {
                left,
                op: BinaryOp::BitAnd,
                right,
                span: merged_span,
            }));
        }

        Ok(left)
    }

    fn parse_shift_expr(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_additive_expr()?;

        loop {
            let op = match self.peek_kind() {
                TokenKind::LtLt => BinaryOp::Shl,
                TokenKind::GtGt => BinaryOp::Shr,
                _ => break,
            };
            let left_span = left.span();
            self.advance();
            let right = self.parse_additive_expr()?;
            let merged_span = left_span.merge(&right.span());
            left = Expr::Binary(Box::new(BinaryExpr {
                left,
                op,
                right,
                span: merged_span,
            }));
        }

        Ok(left)
    }

    fn parse_additive_expr(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_multiplicative_expr()?;

        loop {
            let op = match self.peek_kind() {
                TokenKind::Plus => BinaryOp::Add,
                TokenKind::Minus => BinaryOp::Sub,
                _ => break,
            };
            let left_span = left.span();
            self.advance();
            let right = self.parse_multiplicative_expr()?;
            let merged_span = left_span.merge(&right.span());
            left = Expr::Binary(Box::new(BinaryExpr {
                left,
                op,
                right,
                span: merged_span,
            }));
        }

        Ok(left)
    }

    fn parse_multiplicative_expr(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_unary_expr()?;

        loop {
            let op = match self.peek_kind() {
                TokenKind::Star => BinaryOp::Mul,
                TokenKind::Slash => BinaryOp::Div,
                TokenKind::Percent => BinaryOp::Mod,
                _ => break,
            };
            let left_span = left.span();
            self.advance();
            let right = self.parse_unary_expr()?;
            let merged_span = left_span.merge(&right.span());
            left = Expr::Binary(Box::new(BinaryExpr {
                left,
                op,
                right,
                span: merged_span,
            }));
        }

        Ok(left)
    }

    fn parse_unary_expr(&mut self) -> ParseResult<Expr> {
        // Handle &mut as a special case (two-token operator)
        if *self.peek_kind() == TokenKind::Ampersand {
            let start_span = self.peek().span;
            self.advance();
            let op = if *self.peek_kind() == TokenKind::Mut {
                self.advance();
                UnaryOp::MutRef
            } else {
                UnaryOp::Ref
            };
            let expr = self.parse_unary_expr()?;
            // Span covers from operator to end of inner expression
            let span = start_span.merge(&expr.span());
            return Ok(Expr::Unary(Box::new(UnaryExpr { op, expr, span })));
        }

        let op = match self.peek_kind() {
            TokenKind::Minus => Some(UnaryOp::Neg),
            TokenKind::Not => Some(UnaryOp::Not),
            TokenKind::Tilde => Some(UnaryOp::BitNot),
            TokenKind::Star => Some(UnaryOp::Deref),
            _ => None,
        };

        if let Some(op) = op {
            let start_span = self.peek().span;
            self.advance();
            let expr = self.parse_unary_expr()?;
            // Span covers from operator to end of inner expression
            let span = start_span.merge(&expr.span());
            return Ok(Expr::Unary(Box::new(UnaryExpr { op, expr, span })));
        }

        self.parse_postfix_expr()
    }

    fn parse_postfix_expr(&mut self) -> ParseResult<Expr> {
        let mut expr = self.parse_primary_expr()?;

        loop {
            match self.peek_kind() {
                TokenKind::LParen => {
                    let callee_span = expr.span();
                    self.advance();
                    let (args, has_trailing_comma) = self.parse_arg_list()?;
                    let rparen_span = self.peek().span;
                    self.expect(&TokenKind::RParen)?;
                    let merged_span = callee_span.merge(&rparen_span);
                    expr = Expr::Call(Box::new(CallExpr {
                        callee: expr,
                        type_args: vec![],
                        args,
                        has_trailing_comma,
                        span: merged_span,
                    }));
                }
                // Turbofish syntax for explicit type arguments: foo::<T>(x)
                TokenKind::ColonColon => {
                    // Check if this is turbofish (:: followed by <)
                    // Peek at the token after ::
                    let checkpoint = self.pos;
                    self.advance(); // consume ::
                    if self.check(&TokenKind::Lt) {
                        // This is turbofish syntax: foo::<T>(x)
                        let callee_span = expr.span();
                        let type_args = self.parse_call_type_args()?;
                        self.expect(&TokenKind::LParen)?;
                        let (args, has_trailing_comma) = self.parse_arg_list()?;
                        let rparen_span = self.peek().span;
                        self.expect(&TokenKind::RParen)?;
                        let merged_span = callee_span.merge(&rparen_span);
                        expr = Expr::Call(Box::new(CallExpr {
                            callee: expr,
                            type_args,
                            args,
                            has_trailing_comma,
                            span: merged_span,
                        }));
                    } else {
                        // Not turbofish, backtrack
                        self.pos = checkpoint;
                        break;
                    }
                }
                TokenKind::Dot => {
                    let receiver_span = expr.span();
                    self.advance();
                    let field_span = self.peek().span;

                    // Support identifier, integer literal, and float literal for field access
                    // Integer literals are used for tuple field access: t.0, t.1, etc.
                    // Float literals like "0.0" after a dot are split into two field accesses.
                    let (field, second_field) = if let TokenKind::IntLit(s) = &self.peek().kind {
                        let field_name = s.clone();
                        self.advance();
                        (field_name, None)
                    } else if let TokenKind::FloatLit(s) = &self.peek().kind {
                        // Handle cases like `t.0.0` where the lexer tokenizes "0.0" as a float
                        // Split the float literal into two field indices
                        let parts: Vec<&str> = s.split('.').collect();
                        if parts.len() == 2
                            && parts[0].chars().all(|c| c.is_ascii_digit())
                            && parts[1].chars().all(|c| c.is_ascii_digit())
                        {
                            let first = parts[0].to_string();
                            let second = parts[1].to_string();
                            self.advance();
                            (first, Some(second))
                        } else {
                            // Not a valid tuple field sequence
                            return Err(ParseError {
                                message: format!("expected field name, found FloatLit({s:?})"),
                                span: field_span,
                            });
                        }
                    } else {
                        // Allow keywords as field names (unambiguous after dot)
                        (self.consume_field_name()?, None)
                    };

                    // Check for method call with turbofish: obj.method::<T>(x)
                    if self.check(&TokenKind::ColonColon) {
                        self.advance(); // consume ::
                        if self.check(&TokenKind::Lt) {
                            let type_args = self.parse_call_type_args()?;
                            self.expect(&TokenKind::LParen)?;
                            let (args, has_trailing_comma) = self.parse_arg_list()?;
                            let rparen_span = self.peek().span;
                            self.expect(&TokenKind::RParen)?;
                            let merged_span = receiver_span.merge(&rparen_span);
                            expr = Expr::MethodCall(Box::new(MethodCallExpr {
                                receiver: expr,
                                method: field,
                                type_args,
                                args,
                                has_trailing_comma,
                                span: merged_span,
                            }));
                        } else {
                            // :: not followed by <, this is an error
                            return Err(ParseError {
                                message: "expected '<' after '::'".to_string(),
                                span: self.peek().span,
                            });
                        }
                    } else if self.check(&TokenKind::LParen) {
                        self.advance();
                        let (args, has_trailing_comma) = self.parse_arg_list()?;
                        let rparen_span = self.peek().span;
                        self.expect(&TokenKind::RParen)?;
                        let merged_span = receiver_span.merge(&rparen_span);
                        expr = Expr::MethodCall(Box::new(MethodCallExpr {
                            receiver: expr,
                            method: field,
                            type_args: vec![],
                            args,
                            has_trailing_comma,
                            span: merged_span,
                        }));
                    } else {
                        let merged_span = receiver_span.merge(&field_span);
                        expr = Expr::FieldAccess(Box::new(FieldAccessExpr {
                            expr,
                            field,
                            span: merged_span,
                        }));

                        // If we parsed a float literal as two fields (e.g., "0.0" -> "0", "0"),
                        // add the second field access
                        if let Some(second) = second_field {
                            let second_span = expr.span();
                            expr = Expr::FieldAccess(Box::new(FieldAccessExpr {
                                expr,
                                field: second,
                                span: second_span,
                            }));
                        }
                    }
                }
                TokenKind::LBracket => {
                    let expr_span = expr.span();
                    self.advance();
                    let index = self.parse_expr()?;
                    let rbracket_span = self.peek().span;
                    self.expect(&TokenKind::RBracket)?;
                    let merged_span = expr_span.merge(&rbracket_span);
                    expr = Expr::Index(Box::new(IndexExpr {
                        expr,
                        index,
                        span: merged_span,
                    }));
                }
                TokenKind::As => {
                    let start_span = self.peek().span;
                    self.advance();
                    let target_type = self.parse_type()?;
                    expr = Expr::Cast(Box::new(CastExpr {
                        expr,
                        target_type,
                        span: start_span,
                    }));
                }
                _ => break,
            }
        }

        Ok(expr)
    }

    fn parse_primary_expr(&mut self) -> ParseResult<Expr> {
        let start_span = self.peek().span;

        match self.peek_kind().clone() {
            TokenKind::Ident(name) => {
                self.advance();
                // A name is considered a type name if:
                // 1. It starts with uppercase (UpperCamelCase convention), OR
                // 2. It looks like a primitive type (i32, u64, i128, u128, f32, etc.)
                let is_type_name = name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                    || Self::looks_like_primitive_type(&name);

                // Check for qualified name (Effect::function) or static method call
                if self.check(&TokenKind::ColonColon) {
                    // Peek ahead to check if this is turbofish (::< for type args)
                    let checkpoint = self.pos;
                    self.advance(); // consume ::

                    if self.check(&TokenKind::Lt) && is_type_name {
                        // This could be Type::<Args>::method() - static method on generic type
                        // Parse type arguments
                        self.advance(); // consume <
                        let mut type_args = vec![self.parse_type()?];
                        while self.check(&TokenKind::Comma) {
                            self.advance();
                            type_args.push(self.parse_type()?);
                        }
                        self.expect_gt()?;

                        // Now expect ::method(args)
                        if self.check(&TokenKind::ColonColon) {
                            self.advance(); // consume ::
                            let method = self.consume_ident()?;
                            self.expect(&TokenKind::LParen)?;
                            let (args, has_trailing_comma) = self.parse_arg_list()?;
                            let end_span = self.expect(&TokenKind::RParen)?.span;

                            Ok(Expr::StaticMethodCall(Box::new(StaticMethodCallExpr {
                                target_type: Type::Generic(GenericType {
                                    name,
                                    args: type_args,
                                    span: start_span,
                                }),
                                method,
                                args,
                                has_trailing_comma,
                                span: start_span.merge(&end_span),
                            })))
                        } else {
                            // Not followed by ::method, backtrack for turbofish
                            self.pos = checkpoint;
                            Ok(Expr::Ident(IdentExpr {
                                name,
                                span: start_span,
                            }))
                        }
                    } else if self.check(&TokenKind::Lt) {
                        // Lowercase name with ::<, this is turbofish, backtrack
                        self.pos = checkpoint;
                        Ok(Expr::Ident(IdentExpr {
                            name,
                            span: start_span,
                        }))
                    } else {
                        // This is a qualified name like Effect::function
                        let method = self.consume_ident()?;
                        let qualified_name = format!("{name}::{method}");
                        Ok(Expr::Ident(IdentExpr {
                            name: qualified_name,
                            span: start_span,
                        }))
                    }
                } else if self.check(&TokenKind::Colon)
                    && self.peek_nth(1).kind == TokenKind::LBrace
                {
                    // Labeled block expression: `label: { ... }`
                    self.advance(); // consume ':'
                    let block = self.parse_block()?;
                    let end_span = block.span;
                    Ok(Expr::LabeledBlock(Box::new(crate::ast::LabeledBlockExpr {
                        label: name,
                        block,
                        span: start_span.merge(&end_span),
                    })))
                } else if self.check(&TokenKind::LBrace) && is_type_name {
                    // Struct literal: `Point { x: 10, y: 20 }`
                    // Only parse as struct literal if name starts with uppercase
                    // (struct naming convention: UpperCamelCase)
                    self.parse_struct_literal(Some(name), start_span)
                } else {
                    Ok(Expr::Ident(IdentExpr {
                        name,
                        span: start_span,
                    }))
                }
            }
            TokenKind::IntLit(repr) => {
                self.advance();
                Ok(Expr::Literal(LiteralExpr {
                    value: Literal::Int(IntLiteral { repr: repr.clone() }),
                    span: start_span,
                }))
            }
            TokenKind::FloatLit(repr) => {
                self.advance();
                Ok(Expr::Literal(LiteralExpr {
                    value: Literal::Float(FloatLiteral { repr: repr.clone() }),
                    span: start_span,
                }))
            }
            TokenKind::StringLit(value) => {
                self.advance();
                Ok(Expr::Literal(LiteralExpr {
                    value: Literal::String(value),
                    span: start_span,
                }))
            }
            TokenKind::TemplateStringLit(value) => {
                self.advance();
                self.parse_template_string_parts(value, start_span)
            }
            TokenKind::True => {
                self.advance();
                Ok(Expr::Literal(LiteralExpr {
                    value: Literal::Bool(true),
                    span: start_span,
                }))
            }
            TokenKind::False => {
                self.advance();
                Ok(Expr::Literal(LiteralExpr {
                    value: Literal::Bool(false),
                    span: start_span,
                }))
            }
            TokenKind::Null => {
                self.advance();
                Ok(Expr::Literal(LiteralExpr {
                    value: Literal::Null,
                    span: start_span,
                }))
            }
            TokenKind::CharLit(value) => {
                self.advance();
                Ok(Expr::Literal(LiteralExpr {
                    value: Literal::Char(value),
                    span: start_span,
                }))
            }
            TokenKind::LParen => {
                self.advance();
                // Unit expression: ()
                if self.check(&TokenKind::RParen) {
                    self.advance();
                    return Ok(Expr::Literal(LiteralExpr {
                        value: Literal::Unit,
                        span: start_span,
                    }));
                }
                let expr = self.parse_expr()?;
                let end_span = self.expect(&TokenKind::RParen)?.span;
                // Update expression span to include the parentheses
                Ok(expr.with_span(start_span.merge(&end_span)))
            }
            TokenKind::LBracket => {
                self.advance();
                self.parse_tuple_literal(start_span)
            }
            TokenKind::Pipe => self.parse_closure(),
            TokenKind::LBrace => {
                // Implicit struct literal: `{ field: value, ... }`
                // Look ahead to check if this is a struct literal (ident followed by : or ,)
                // vs a future block expression
                self.advance(); // consume `{`

                // Check if this looks like a struct literal
                // Pattern: `{ ident :` or `{ ident ,` or `{ ident }`
                if let TokenKind::Ident(_) = self.peek_kind() {
                    // Peek at the token after the identifier
                    let after_ident = self.peek_nth(1);
                    if matches!(
                        after_ident.kind,
                        TokenKind::Colon | TokenKind::Comma | TokenKind::RBrace
                    ) {
                        // This is an implicit struct literal
                        return self.parse_struct_literal(None, start_span);
                    }
                }

                // Empty struct literal: `{}`
                if self.check(&TokenKind::RBrace) {
                    return self.parse_struct_literal(None, start_span);
                }

                // Not a valid implicit struct literal
                Err(ParseError {
                    message: "implicit struct literal requires field syntax: { field: value }"
                        .into(),
                    span: start_span,
                })
            }
            TokenKind::If => self.parse_if_expr(),
            TokenKind::Hash => {
                self.advance(); // consume '#'
                // Parse compile-time location literals: #file, #line, #function
                match self.peek_kind() {
                    TokenKind::Ident(name) => {
                        let name = name.clone();
                        let end_span = self.advance().span;
                        let literal = match name.as_str() {
                            "file" => Literal::LocationFile,
                            "line" => Literal::LocationLine,
                            "function" => Literal::LocationFunction,
                            _ => {
                                return Err(ParseError {
                                    message: format!(
                                        "unknown compile-time literal `#{name}`, expected `#file`, `#line`, or `#function`"
                                    ),
                                    span: start_span.merge(&end_span),
                                });
                            }
                        };
                        Ok(Expr::Literal(LiteralExpr {
                            value: literal,
                            span: start_span.merge(&end_span),
                        }))
                    }
                    _ => Err(ParseError {
                        message: "expected identifier after `#` for compile-time literal".into(),
                        span: start_span,
                    }),
                }
            }
            _ => Err(ParseError {
                message: format!("expected expression, found {:?}", self.peek_kind()),
                span: start_span,
            }),
        }
    }

    /// Parse if expression: `if condition { expr } else { expr }`
    fn parse_if_expr(&mut self) -> ParseResult<Expr> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::If)?;

        let mut init = None;
        let condition;

        // Check for if-let: either Rust-style pattern matching or Go-style init
        if self.check(&TokenKind::Let) {
            self.advance(); // consume 'let'

            // Check if this is Rust-style pattern matching (uppercase identifier = pattern start)
            if self.is_variant_pattern_start() {
                // Rust-style: `if let Some(x) = expr { ... }` or `if let None = expr { ... }`
                let pattern_span = self.peek().span;
                let pattern = self.parse_pattern()?;
                self.expect(&TokenKind::Eq)?;
                let expr = self.parse_expr()?;
                let span = pattern_span.merge(&expr.span());
                condition = Condition::Pattern {
                    pattern,
                    expr,
                    span,
                };
            } else {
                // Go-style: `if let x = expr; condition { ... }`
                // We already consumed 'let', need to parse variable declaration
                let let_stmt = self.parse_let_stmt_after_let()?;
                self.expect(&TokenKind::Semicolon)?;
                init = Some(Box::new(let_stmt));
                condition = Condition::Expr(self.parse_expr()?);
            }
        } else {
            // No 'let', just a regular condition expression
            condition = Condition::Expr(self.parse_expr()?);
        }

        let then_block = self.parse_block()?;

        let else_block = if self.check(&TokenKind::Else) {
            self.advance();
            if self.check(&TokenKind::If) {
                // `else if` - parse as nested if expression wrapped in a block
                let if_expr = self.parse_if_expr()?;
                let span = if_expr.span();
                Some(Block {
                    stmts: vec![Stmt::Expr(ExprStmt {
                        expr: if_expr,
                        span,
                    })],
                    span,
                })
            } else {
                Some(self.parse_block()?)
            }
        } else {
            None
        };

        let end_span = else_block.as_ref().map_or(&then_block.span, |b| &b.span);
        let span = start_span.merge(end_span);

        Ok(Expr::If(Box::new(IfExpr {
            init,
            condition,
            then_block,
            else_block,
            span,
        })))
    }

    /// Parse tuple literal: `[expr, expr, ...]` or `[]`
    fn parse_tuple_literal(&mut self, start_span: Span) -> ParseResult<Expr> {
        let mut elements = Vec::new();

        // Handle empty tuple: []
        if self.check(&TokenKind::RBracket) {
            self.advance();
            return Ok(Expr::TupleLiteral(Box::new(TupleLiteralExpr {
                elements,
                span: start_span,
            })));
        }

        // Parse first element
        elements.push(self.parse_expr()?);

        // Parse remaining elements
        while self.check(&TokenKind::Comma) {
            self.advance();
            // Handle trailing comma: [1, 2, 3,]
            if self.check(&TokenKind::RBracket) {
                break;
            }
            elements.push(self.parse_expr()?);
        }

        self.expect(&TokenKind::RBracket)?;

        Ok(Expr::TupleLiteral(Box::new(TupleLiteralExpr {
            elements,
            span: start_span,
        })))
    }

    /// Parse argument list. Returns (args, `has_trailing_comma`).
    fn parse_arg_list(&mut self) -> ParseResult<(Vec<Expr>, bool)> {
        let mut args = Vec::new();
        let mut has_trailing_comma = false;

        if !self.check(&TokenKind::RParen) {
            args.push(self.parse_expr()?);

            while self.check(&TokenKind::Comma) {
                self.advance();
                if self.check(&TokenKind::RParen) {
                    has_trailing_comma = true;
                    break;
                }
                args.push(self.parse_expr()?);
            }
        }

        Ok((args, has_trailing_comma))
    }

    fn parse_closure(&mut self) -> ParseResult<Expr> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Pipe)?;

        let mut params = Vec::new();
        if !self.check(&TokenKind::Pipe) {
            params.push(self.parse_closure_param()?);
            while self.check(&TokenKind::Comma) {
                self.advance();
                params.push(self.parse_closure_param()?);
            }
        }

        self.expect(&TokenKind::Pipe)?;

        // Check for block body: |params| { ... }
        let body = if self.check(&TokenKind::LBrace) {
            let block = self.parse_block()?;
            Expr::Block(Box::new(block))
        } else {
            self.parse_expr()?
        };

        Ok(Expr::Closure(Box::new(ClosureExpr {
            params,
            body,
            span: start_span,
        })))
    }

    fn parse_closure_param(&mut self) -> ParseResult<ClosureParam> {
        let name = self.consume_ident()?;
        let ty = if self.check(&TokenKind::Colon) {
            self.advance();
            Some(self.parse_type()?)
        } else {
            None
        };
        Ok(ClosureParam { name, ty })
    }

    fn parse_type(&mut self) -> ParseResult<Type> {
        let start_span = self.peek().span;

        // Never type: !
        if self.check(&TokenKind::Not) {
            self.advance();
            return Ok(Type::Named(NamedType {
                name: "!".to_string(),
                span: start_span,
            }));
        }

        // Function type: fn(T1, T2) -> R
        if self.check(&TokenKind::Fn) {
            self.advance();
            self.expect(&TokenKind::LParen)?;

            // Parse parameter types
            let mut params = Vec::new();
            if !self.check(&TokenKind::RParen) {
                params.push(self.parse_type()?);
                while self.check(&TokenKind::Comma) {
                    self.advance();
                    if self.check(&TokenKind::RParen) {
                        break;
                    }
                    params.push(self.parse_type()?);
                }
            }
            self.expect(&TokenKind::RParen)?;

            // Parse return type (optional)
            let return_type = if self.check(&TokenKind::Arrow) {
                self.advance();
                self.parse_type()?
            } else {
                Type::Named(NamedType {
                    name: "()".to_string(),
                    span: start_span,
                })
            };

            return Ok(Type::Function(Box::new(FunctionType {
                params,
                return_type,
                effects: Vec::new(),
            })));
        }

        // Reference type: &T or &mut T
        if self.check(&TokenKind::Ampersand) {
            self.advance();
            // Check for mutable reference
            let is_mut = if self.check(&TokenKind::Mut) {
                self.advance();
                true
            } else {
                false
            };
            let inner = self.parse_type()?;
            return Ok(if is_mut {
                Type::MutReference(Box::new(inner))
            } else {
                Type::Reference(Box::new(inner))
            });
        }

        // Unit type: ()
        if self.check(&TokenKind::LParen) {
            self.advance();
            if self.check(&TokenKind::RParen) {
                self.advance();
                // Unit type () - distinct from empty tuple []
                return Ok(Type::Named(NamedType {
                    name: "()".to_string(),
                    span: start_span,
                }));
            }
            // Parenthesized type for grouping (not tuple in this case)
            let inner = self.parse_type()?;
            self.expect(&TokenKind::RParen)?;
            return Ok(inner);
        }

        // Tuple type: [] or [T1, T2, ...]
        if self.check(&TokenKind::LBracket) {
            self.advance();
            if self.check(&TokenKind::RBracket) {
                self.advance();
                // Empty tuple type []
                return Ok(Type::Tuple(Vec::new()));
            }
            // Tuple with elements
            let mut types = vec![self.parse_type()?];
            while self.check(&TokenKind::Comma) {
                self.advance();
                if self.check(&TokenKind::RBracket) {
                    break;
                }
                types.push(self.parse_type()?);
            }
            self.expect(&TokenKind::RBracket)?;
            return Ok(Type::Tuple(types));
        }

        let name = self.consume_ident()?;

        // Check for namespaced type: namespace::type<T>
        if self.check(&TokenKind::ColonColon) {
            self.advance();
            let type_name = self.consume_ident()?;

            // Namespaced generic type: namespace::type<T>
            if self.check(&TokenKind::Lt) {
                self.advance();
                let mut args = vec![self.parse_type()?];
                // Continue parsing args only if we see a comma AND we don't have a pending >
                while !self.pending_gt && self.check(&TokenKind::Comma) {
                    self.advance();
                    args.push(self.parse_type()?);
                }
                self.expect_gt()?;

                return Ok(Type::NamespacedGeneric(NamespacedGenericType {
                    namespace: name,
                    name: type_name,
                    args,
                    span: start_span,
                }));
            } else {
                // Namespaced type without generics: namespace::type
                return Ok(Type::NamespacedGeneric(NamespacedGenericType {
                    namespace: name,
                    name: type_name,
                    args: Vec::new(),
                    span: start_span,
                }));
            }
        }

        if self.check(&TokenKind::Lt) {
            self.advance();
            let mut args = vec![self.parse_type()?];
            // Continue parsing args only if we see a comma AND we don't have a pending >
            // (pending_gt means we split >> and the outer > closes this type arg list)
            while !self.pending_gt && self.check(&TokenKind::Comma) {
                self.advance();
                args.push(self.parse_type()?);
            }
            // Handle >> being lexed as GtGt instead of two Gt tokens (for nested generics)
            self.expect_gt()?;

            Ok(Type::Generic(GenericType {
                name,
                args,
                span: start_span,
            }))
        } else {
            Ok(Type::Named(NamedType {
                name,
                span: start_span,
            }))
        }
    }

    // Placeholder implementations for other declarations

    fn parse_effect_decl(
        &mut self,
        is_pub: bool,
        attrs: Vec<Attribute>,
    ) -> ParseResult<EffectDecl> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Effect)?;
        let name = self.consume_ident()?;
        self.expect(&TokenKind::LBrace)?;

        let mut methods = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            methods.push(self.parse_effect_method()?);
        }

        let end_span = self.expect(&TokenKind::RBrace)?.span;

        Ok(EffectDecl {
            name,
            is_pub,
            attrs,
            methods,
            span: start_span.merge(&end_span),
        })
    }

    fn parse_effect_method(&mut self) -> ParseResult<EffectMethod> {
        // Parse any attributes on the method (e.g., #[wasi("...")])
        let attrs = self.parse_attributes()?;

        let start_span = self.peek().span;

        // Check for async keyword
        let is_async = if self.check(&TokenKind::Async) {
            self.advance();
            true
        } else {
            false
        };

        self.expect(&TokenKind::Fn)?;
        let name = self.consume_ident()?;

        // Skip generic parameters like <T, E>
        if self.check(&TokenKind::Lt) {
            self.advance();
            while !self.check(&TokenKind::Gt) && !self.is_at_end() {
                self.advance();
            }
            self.expect(&TokenKind::Gt)?;
        }

        self.expect(&TokenKind::LParen)?;
        let params = self.parse_param_list()?;
        self.expect(&TokenKind::RParen)?;

        let return_type = if self.check(&TokenKind::Arrow) {
            self.advance();
            Some(self.parse_type()?)
        } else {
            None
        };

        self.expect(&TokenKind::Semicolon)?;

        Ok(EffectMethod {
            name,
            is_async,
            attrs,
            params,
            return_type,
            span: start_span,
        })
    }

    /// Parse generic type parameters: `<T>`, `<T, U>`, `<T: Ord>`, `<T: Ord + Clone>`, `<T = Default>`
    fn parse_generic_params(&mut self) -> ParseResult<Vec<crate::ast::GenericParam>> {
        if !self.check(&TokenKind::Lt) {
            return Ok(Vec::new());
        }

        self.advance(); // consume '<'

        let mut params = Vec::new();

        while !self.check(&TokenKind::Gt) && !self.is_at_end() {
            let start_span = self.peek().span;
            let name = self.consume_ident()?;

            // Parse optional trait bounds: `T: Ord` or `T: Ord + Clone`
            let bounds = if self.check(&TokenKind::Colon) {
                self.advance();
                let mut bounds = vec![self.consume_ident()?];
                while self.check(&TokenKind::Plus) {
                    self.advance();
                    bounds.push(self.consume_ident()?);
                }
                bounds
            } else {
                Vec::new()
            };

            // Parse optional default type: `T = DefaultType`
            let default = if self.check(&TokenKind::Eq) {
                self.advance();
                Some(self.parse_type()?)
            } else {
                None
            };

            params.push(crate::ast::GenericParam {
                name,
                bounds,
                default,
                span: start_span,
            });

            if self.check(&TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }

        self.expect(&TokenKind::Gt)?;
        Ok(params)
    }

    /// Parse type arguments for turbofish syntax: `<T1, T2, ...>`
    /// Used in function calls like `identity::<i32>(x)`
    fn parse_call_type_args(&mut self) -> ParseResult<Vec<Type>> {
        // Must start with '<'
        self.expect(&TokenKind::Lt)?;

        let mut types = vec![self.parse_type()?];
        while self.check(&TokenKind::Comma) {
            self.advance();
            types.push(self.parse_type()?);
        }

        self.expect_gt()?;
        Ok(types)
    }

    fn parse_struct_decl(&mut self, is_pub: bool) -> ParseResult<StructDecl> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Struct)?;
        let name = self.consume_ident()?;

        // Parse generic parameters like <T, U>
        let type_params = self.parse_generic_params()?;

        self.expect(&TokenKind::LBrace)?;

        let fields = self.parse_struct_fields()?;

        let end_span = self.expect(&TokenKind::RBrace)?.span;

        Ok(StructDecl {
            name,
            is_pub,
            type_params,
            fields,
            span: start_span.merge(&end_span),
        })
    }

    fn parse_struct_fields(&mut self) -> ParseResult<Vec<StructField>> {
        let mut fields = Vec::new();

        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            let start_span = self.peek().span;
            // Allow keywords as field names (unambiguous in context)
            let name = self.consume_field_name()?;
            self.expect(&TokenKind::Colon)?;
            let ty = self.parse_type()?;

            fields.push(StructField {
                name,
                ty,
                span: start_span,
            });

            if !self.check(&TokenKind::RBrace) {
                self.expect(&TokenKind::Comma)?;
            }
        }

        Ok(fields)
    }

    fn parse_enum_decl(&mut self, is_pub: bool, attrs: Vec<Attribute>) -> ParseResult<EnumDecl> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Enum)?;
        let name = self.consume_ident()?;

        // Parse generic parameters like <T>
        let type_params = self.parse_generic_params()?;

        self.expect(&TokenKind::LBrace)?;

        let mut cases = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            cases.push(self.parse_enum_case()?);
            if !self.check(&TokenKind::RBrace) {
                self.expect(&TokenKind::Comma)?;
            }
        }

        let end_span = self.expect(&TokenKind::RBrace)?.span;

        Ok(EnumDecl {
            name,
            is_pub,
            type_params,
            cases,
            attrs,
            span: start_span.merge(&end_span),
        })
    }

    fn parse_enum_case(&mut self) -> ParseResult<EnumCase> {
        let start_span = self.peek().span;
        let name = self.consume_ident()?;

        // Enum cases have no payload (unlike variant cases)
        Ok(EnumCase {
            name,
            span: start_span,
        })
    }

    /// Parse a flags declaration
    /// ```wado
    /// flags DescriptorFlags {
    ///     Read,
    ///     Write,
    /// }
    /// ```
    fn parse_flags_decl(&mut self, is_pub: bool, attrs: Vec<Attribute>) -> ParseResult<FlagsDecl> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Flags)?;
        let name = self.consume_ident()?;

        self.expect(&TokenKind::LBrace)?;

        let mut flags = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            // Skip doc comments (lines starting with ///)
            // The lexer should handle these, but flags only have simple names
            let flag_span = self.peek().span;
            let flag_name = self.consume_ident()?;
            flags.push(FlagsVariant {
                name: flag_name,
                span: flag_span,
            });
            // Comma is optional for the last item
            if !self.check(&TokenKind::RBrace) {
                self.expect(&TokenKind::Comma)?;
            }
        }

        let end_span = self.expect(&TokenKind::RBrace)?.span;

        Ok(FlagsDecl {
            name,
            is_pub,
            attributes: if attrs.is_empty() { None } else { Some(attrs) },
            flags,
            span: start_span.merge(&end_span),
        })
    }

    /// Parse a variant declaration (tagged union with payloads)
    /// ```wado
    /// variant Option<T> {
    ///     Some(T),
    ///     None,
    /// }
    /// ```
    fn parse_variant_decl(&mut self, is_pub: bool) -> ParseResult<VariantDecl> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Variant)?;
        let name = self.consume_ident()?;

        // Parse generic parameters like <T>
        let type_params = self.parse_generic_params()?;

        self.expect(&TokenKind::LBrace)?;

        let mut cases = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            cases.push(self.parse_variant_case()?);
            if !self.check(&TokenKind::RBrace) {
                self.expect(&TokenKind::Comma)?;
            }
        }

        let end_span = self.expect(&TokenKind::RBrace)?.span;

        Ok(VariantDecl {
            name,
            is_pub,
            type_params,
            cases,
            span: start_span.merge(&end_span),
        })
    }

    fn parse_variant_case(&mut self) -> ParseResult<VariantCase> {
        let start_span = self.peek().span;
        let name = self.consume_ident()?;

        // Each variant case has exactly one payload type.
        // - Unit: `None` (no parentheses)
        // - Scalar: `Some(T)` (single type in parentheses)
        // - Tuple: `Rectangle([f64, f64])` (tuple type as single payload)
        // - Struct: `Named({ w: f64, h: f64 })` (struct type as single payload)
        let payload = if self.check(&TokenKind::LParen) {
            self.advance();
            let payload_type = self.parse_type()?;
            // Reject implicit tuple expansion: `Name(T, U)` is now invalid
            if self.check(&TokenKind::Comma) {
                return Err(ParseError {
                    message: "variant case payload must be a single type; use explicit tuple syntax like `Name([T, U])` for multiple values".to_string(),
                    span: self.peek().span,
                });
            }
            self.expect(&TokenKind::RParen)?;
            Some(payload_type)
        } else {
            None
        };

        Ok(VariantCase {
            name,
            payload,
            span: start_span,
        })
    }

    fn parse_type_alias(&mut self, is_pub: bool) -> ParseResult<TypeAlias> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Type)?;
        let name = self.consume_ident()?;
        self.expect(&TokenKind::Eq)?;
        let ty = self.parse_type()?;
        self.expect(&TokenKind::Semicolon)?;

        Ok(TypeAlias {
            name,
            is_pub,
            ty,
            span: start_span,
        })
    }

    fn parse_impl_block(&mut self) -> ParseResult<ImplBlock> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Impl)?;

        // Parse generic parameters like <T>
        let type_params = self.parse_generic_params()?;

        // Parse first type (could be trait name or target type)
        let first_type = self.parse_type()?;

        // Check if this is `impl Trait for Type` or just `impl Type`
        let (trait_type, ty) = if self.check(&TokenKind::For) {
            self.advance(); // consume 'for'
            let target_type = self.parse_type()?;
            (Some(first_type), target_type)
        } else {
            (None, first_type)
        };

        self.expect(&TokenKind::LBrace)?;

        let mut associated_types = Vec::new();
        let mut methods = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            let attrs = self.parse_attributes()?;

            // Check if this is an associated type binding: `type Name = Type;`
            if self.check(&TokenKind::Type) {
                let type_span = self.peek().span;
                self.advance();
                let assoc_name = self.consume_ident()?;
                self.expect(&TokenKind::Eq)?;
                let assoc_ty = self.parse_type()?;
                let end = self.expect(&TokenKind::Semicolon)?.span;
                associated_types.push(AssociatedTypeBinding {
                    name: assoc_name,
                    ty: assoc_ty,
                    span: type_span.merge(&end),
                });
            } else {
                let _ = attrs; // attrs handled in parse_function
                let is_pub = if self.check(&TokenKind::Pub) {
                    self.advance();
                    true
                } else {
                    false
                };
                methods.push(self.parse_function(is_pub, Vec::new())?);
            }
        }

        let end_span = self.expect(&TokenKind::RBrace)?.span;

        Ok(ImplBlock {
            type_params,
            trait_type,
            ty,
            associated_types,
            methods,
            span: start_span.merge(&end_span),
        })
    }

    /// Parse a trait declaration
    /// ```wado
    /// trait Display {
    ///     fn display(&self) -> String;
    /// }
    /// ```
    fn parse_trait_decl(&mut self, is_pub: bool) -> ParseResult<TraitDecl> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Trait)?;

        let name = self.consume_ident()?;

        // Parse generic parameters like <T>
        let type_params = self.parse_generic_params()?;

        self.expect(&TokenKind::LBrace)?;

        let mut associated_types = Vec::new();
        let mut methods = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            let attrs = self.parse_attributes()?;

            // Check if this is an associated type declaration: `type Name;`
            if self.check(&TokenKind::Type) {
                let type_span = self.peek().span;
                self.advance();
                let assoc_name = self.consume_ident()?;
                let end = self.expect(&TokenKind::Semicolon)?.span;
                associated_types.push(AssociatedTypeDecl {
                    name: assoc_name,
                    span: type_span.merge(&end),
                });
            } else {
                // Trait methods are not pub (visibility comes from trait itself)
                let _ = attrs; // attrs currently unused for trait methods
                methods.push(self.parse_function(false, Vec::new())?);
            }
        }

        let end_span = self.expect(&TokenKind::RBrace)?.span;

        Ok(TraitDecl {
            name,
            is_pub,
            type_params,
            associated_types,
            methods,
            span: start_span.merge(&end_span),
        })
    }

    /// Parse a world declaration
    /// ```wado
    /// world CliCommand {
    ///     import Stdout {
    ///         write_via_stream,
    ///     }
    ///     export async fn run() -> Result<(), ()>;
    /// }
    /// ```
    fn parse_world_decl(&mut self) -> ParseResult<WorldDecl> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::World)?;
        let name = self.consume_ident()?;
        self.expect(&TokenKind::LBrace)?;

        let mut imports = Vec::new();
        let mut exports = Vec::new();

        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            match self.peek_kind() {
                TokenKind::Import => {
                    imports.push(self.parse_world_import()?);
                }
                TokenKind::Export => {
                    exports.push(self.parse_world_export()?);
                }
                _ => {
                    return Err(ParseError {
                        message: format!(
                            "expected 'import' or 'export' in world declaration, found {:?}",
                            self.peek_kind()
                        ),
                        span: self.peek().span,
                    });
                }
            }
        }

        let end_span = self.expect(&TokenKind::RBrace)?.span;

        Ok(WorldDecl {
            name,
            imports,
            exports,
            span: start_span.merge(&end_span),
        })
    }

    /// Parse a world import group
    /// ```wado
    /// import Stdout {
    ///     write_via_stream,
    /// }
    /// ```
    fn parse_world_import(&mut self) -> ParseResult<WorldImport> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Import)?;
        let effect_name = self.consume_ident()?;
        self.expect(&TokenKind::LBrace)?;

        let mut functions = Vec::new();
        if !self.check(&TokenKind::RBrace) {
            functions.push(self.consume_ident()?);
            while self.check(&TokenKind::Comma) {
                self.advance();
                if self.check(&TokenKind::RBrace) {
                    break;
                }
                functions.push(self.consume_ident()?);
            }
        }

        self.expect(&TokenKind::RBrace)?;

        Ok(WorldImport {
            effect_name,
            functions,
            span: start_span,
        })
    }

    /// Parse a world export declaration
    /// ```wado
    /// export async fn run() -> Result<(), ()>;
    /// export fn get_version() -> string;
    /// ```
    fn parse_world_export(&mut self) -> ParseResult<WorldExport> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Export)?;

        // Check for async keyword
        let is_async = if self.check(&TokenKind::Async) {
            self.advance();
            true
        } else {
            false
        };

        self.expect(&TokenKind::Fn)?;
        let name = self.consume_ident()?;

        // Skip generic parameters like <T, E>
        if self.check(&TokenKind::Lt) {
            self.advance();
            while !self.check(&TokenKind::Gt) && !self.is_at_end() {
                self.advance();
            }
            self.expect(&TokenKind::Gt)?;
        }

        self.expect(&TokenKind::LParen)?;
        let params = self.parse_param_list()?;
        self.expect(&TokenKind::RParen)?;

        let return_type = if self.check(&TokenKind::Arrow) {
            self.advance();
            Some(self.parse_type()?)
        } else {
            None
        };

        self.expect(&TokenKind::Semicolon)?;

        Ok(WorldExport {
            name,
            is_async,
            params,
            return_type,
            span: start_span,
        })
    }

    /// Parse template string and extract parts (string literals and interpolations)
    /// Input: raw template string content (without backticks)
    /// Example: "Hello, {name}!" -> [String("Hello, "), Interpolation(name), String("!")]
    fn parse_template_string_parts(&mut self, content: String, span: Span) -> ParseResult<Expr> {
        use crate::ast::{TemplatePart, TemplateStringExpr};

        let mut parts = Vec::new();
        let mut chars = content.chars().peekable();
        let mut current_string = String::new();

        while let Some(ch) = chars.next() {
            if ch == '{' {
                // Check if it's an escaped brace or interpolation start
                if chars.peek() == Some(&'{') {
                    // Escaped {{ -> single {
                    chars.next();
                    current_string.push('{');
                } else {
                    // Save current string part if not empty
                    if !current_string.is_empty() {
                        parts.push(TemplatePart::String(current_string.clone()));
                        current_string.clear();
                    }

                    // Extract interpolation expression and optional format spec
                    let (expr_str, format_spec) = self.extract_interpolation(&mut chars, span)?;

                    // Parse the expression
                    let expr = self.parse_interpolation_expr(&expr_str, span)?;

                    parts.push(TemplatePart::Interpolation {
                        expr: Box::new(expr),
                        format: format_spec,
                    });
                }
            } else if ch == '}' {
                // Check if it's an escaped brace
                if chars.peek() == Some(&'}') {
                    // Escaped }} -> single }
                    chars.next();
                    current_string.push('}');
                } else {
                    // Unmatched closing brace - error
                    return Err(ParseError {
                        message: "unmatched '}' in template string".to_string(),
                        span,
                    });
                }
            } else {
                current_string.push(ch);
            }
        }

        // Add final string part if not empty
        if !current_string.is_empty() {
            parts.push(TemplatePart::String(current_string));
        }

        Ok(Expr::TemplateString(Box::new(TemplateStringExpr {
            parts,
            span,
        })))
    }

    /// Extract interpolation expression and format specifier from template string
    /// Returns (`expression_string`, `optional_format_spec`)
    fn extract_interpolation(
        &mut self,
        chars: &mut std::iter::Peekable<std::str::Chars>,
        span: Span,
    ) -> ParseResult<(String, Option<FormatSpec>)> {
        let mut expr_str = String::new();
        let mut brace_depth = 1;
        let mut in_string = false;
        let mut backtick_depth = 0; // Track nested template strings
        let mut escape_next = false;

        while let Some(ch) = chars.next() {
            if escape_next {
                expr_str.push(ch);
                escape_next = false;
                continue;
            }

            match ch {
                '\\' => {
                    expr_str.push(ch);
                    escape_next = true;
                }
                '"' if backtick_depth == 0 => {
                    in_string = !in_string;
                    expr_str.push(ch);
                }
                '`' if !in_string => {
                    expr_str.push(ch);
                    // Toggle backtick depth: odd = inside template, even = outside
                    if backtick_depth == 0 {
                        backtick_depth = 1;
                    } else {
                        backtick_depth = 0;
                    }
                }
                '{' if !in_string && backtick_depth == 0 => {
                    brace_depth += 1;
                    expr_str.push(ch);
                }
                '}' if !in_string && backtick_depth == 0 => {
                    brace_depth -= 1;
                    if brace_depth == 0 {
                        break;
                    }
                    expr_str.push(ch);
                }
                ':' if !in_string && backtick_depth == 0 && brace_depth == 1 => {
                    // Check if it's :: (scope resolution) or : (format spec)
                    if chars.peek() == Some(&':') {
                        // It's ::, part of the expression
                        expr_str.push(ch);
                        expr_str.push(chars.next().unwrap());
                    } else {
                        // It's a format specifier - extract it
                        let format_spec = self.extract_format_spec(chars, span)?;
                        // Consume the closing }
                        if chars.next() != Some('}') {
                            return Err(ParseError {
                                message: "expected '}' after format specifier".to_string(),
                                span,
                            });
                        }
                        return Ok((expr_str.trim().to_string(), Some(format_spec)));
                    }
                }
                _ => {
                    expr_str.push(ch);
                }
            }
        }

        if brace_depth != 0 {
            return Err(ParseError {
                message: "unclosed '{' in template string interpolation".to_string(),
                span,
            });
        }

        Ok((expr_str.trim().to_string(), None))
    }

    /// Extract format specifier (everything after : until })
    fn extract_format_spec(
        &mut self,
        chars: &mut std::iter::Peekable<std::str::Chars>,
        span: Span,
    ) -> ParseResult<FormatSpec> {
        let mut spec = String::new();

        while let Some(&ch) = chars.peek() {
            if ch == '}' {
                break;
            }
            spec.push(ch);
            chars.next();
        }

        if spec.is_empty() {
            return Err(ParseError {
                message: "empty format specifier in template string".to_string(),
                span,
            });
        }

        Ok(FormatSpec { spec })
    }

    /// Parse an interpolation expression string
    fn parse_interpolation_expr(&mut self, expr_str: &str, span: Span) -> ParseResult<Expr> {
        if expr_str.is_empty() {
            return Err(ParseError {
                message: "empty interpolation expression in template string".to_string(),
                span,
            });
        }

        // Create a new lexer and parser for the interpolation expression
        // Use the span's line number so that #line reports the correct location
        let mut lexer = crate::lexer::Lexer::with_line(expr_str, span.line);
        let tokens = lexer.tokenize().map_err(|e| ParseError {
            message: format!("error parsing template interpolation: {}", e.message),
            span,
        })?;

        let mut parser = Parser::new(tokens);
        parser.parse_expr()
    }

    /// Parse struct literal: `Point { x: 10, y: 20 }` or `Point { x, y }` (shorthand)
    /// Also handles implicit struct literals `{ x: 10, y: 20 }` where name is None.
    fn parse_struct_literal(
        &mut self,
        name: Option<String>,
        start_span: Span,
    ) -> ParseResult<Expr> {
        // For named struct literals, the `{` comes after the name
        // For implicit struct literals, the `{` is already consumed
        if name.is_some() {
            self.expect(&TokenKind::LBrace)?;
        }

        let mut fields = Vec::new();
        let mut has_trailing_comma = false;

        if !self.check(&TokenKind::RBrace) {
            loop {
                let field_span = self.peek().span;
                // Allow keywords as field names (unambiguous in context)
                let field_name = self.consume_field_name()?;

                let (value, is_shorthand) = if self.check(&TokenKind::Colon) {
                    self.advance();
                    (self.parse_expr()?, false)
                } else {
                    // Shorthand: `{ x }` is equivalent to `{ x: x }`
                    (
                        Expr::Ident(IdentExpr {
                            name: field_name.clone(),
                            span: field_span,
                        }),
                        true,
                    )
                };

                fields.push(StructLiteralField {
                    name: field_name,
                    value,
                    is_shorthand,
                    span: field_span,
                });

                if !self.check(&TokenKind::Comma) {
                    break;
                }
                self.advance(); // consume comma
                if self.check(&TokenKind::RBrace) {
                    has_trailing_comma = true;
                    break; // trailing comma allowed
                }
            }
        }

        let end_span = self.peek().span;
        self.expect(&TokenKind::RBrace)?;

        Ok(Expr::StructLiteral(Box::new(StructLiteralExpr {
            name,
            fields,
            has_trailing_comma,
            span: start_span.merge(&end_span),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;

    fn parse(source: &str) -> ParseResult<Module> {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().expect("lexer error");
        let (data_section, _comments, shebang) = lexer.into_parts();
        let mut parser = Parser::with_metadata(tokens, shebang, data_section);
        parser.parse()
    }

    #[test]
    fn test_use_decl() {
        let module = parse(r#"use {println, Stdout} from "core:cli";"#).unwrap();
        assert_eq!(module.items.len(), 1);

        if let Item::Use(use_decl) = &module.items[0] {
            assert_eq!(use_decl.source, "core:cli");
            assert_eq!(use_decl.items.len(), 2);
            assert!(
                matches!(&use_decl.items[0], UseItem::Simple { name, alias } if name == "println" && alias.is_none())
            );
            assert!(
                matches!(&use_decl.items[1], UseItem::Simple { name, alias } if name == "Stdout" && alias.is_none())
            );
            assert!(use_decl.attributes.is_none());
        } else {
            panic!("expected use declaration");
        }
    }

    #[test]
    fn test_use_decl_with_alias() {
        let module = parse(r#"use {println as print} from "core:cli";"#).unwrap();

        if let Item::Use(use_decl) = &module.items[0] {
            assert_eq!(use_decl.source, "core:cli");
            assert!(
                matches!(&use_decl.items[0], UseItem::Simple { name, alias } if name == "println" && alias.as_deref() == Some("print"))
            );
        } else {
            panic!("expected use declaration");
        }
    }

    #[test]
    fn test_use_decl_effect_functions() {
        let module = parse(r#"use {Stdout::{write_via_stream}} from "wasi:cli";"#).unwrap();

        if let Item::Use(use_decl) = &module.items[0] {
            assert_eq!(use_decl.source, "wasi:cli");
            assert_eq!(use_decl.items.len(), 1);
            if let UseItem::EffectFunctions {
                effect_name,
                functions,
            } = &use_decl.items[0]
            {
                assert_eq!(effect_name, "Stdout");
                assert_eq!(functions.len(), 1);
                assert_eq!(functions[0].name, "write_via_stream");
            } else {
                panic!("expected EffectFunctions");
            }
        } else {
            panic!("expected use declaration");
        }
    }

    #[test]
    fn test_use_decl_with_attributes() {
        let module = parse(r#"use {Stdout} from "wasi:cli" with { version: "0.3.0" };"#).unwrap();

        if let Item::Use(use_decl) = &module.items[0] {
            assert_eq!(use_decl.source, "wasi:cli");
            assert!(use_decl.attributes.is_some());
            let attrs = use_decl.attributes.as_ref().unwrap();
            assert_eq!(attrs.version, Some("0.3.0".to_string()));
        } else {
            panic!("expected use declaration");
        }
    }

    #[test]
    fn test_simple_function() {
        let module = parse("fn run() { }").unwrap();
        assert_eq!(module.items.len(), 1);

        if let Item::Function(func) = &module.items[0] {
            assert_eq!(func.name, "run");
            assert!(func.params.is_empty());
            assert!(func.return_type.is_none());
            assert!(func.effects.is_empty());
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_function_with_effects() {
        let module = parse("fn run() with Stdout { }").unwrap();

        if let Item::Function(func) = &module.items[0] {
            assert_eq!(func.effects, vec!["Stdout"]);
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_function_call() {
        let module = parse(r#"fn run() { println("hello"); }"#).unwrap();

        if let Item::Function(func) = &module.items[0] {
            let body = func.body.as_ref().expect("function should have body");
            assert_eq!(body.stmts.len(), 1);
            if let Stmt::Expr(expr_stmt) = &body.stmts[0]
                && let Expr::Call(call) = &expr_stmt.expr
                && let Expr::Ident(ident) = &call.callee
            {
                assert_eq!(ident.name, "println");
            }
        }
    }

    #[test]
    fn test_world_decl() {
        let source = r#"
            world CliCommand {
                import Stdout {
                    write_via_stream,
                }

                export async fn run() -> Result<(), ()>;
            }
        "#;

        let module = parse(source).unwrap();
        assert_eq!(module.items.len(), 1);

        if let Item::World(world) = &module.items[0] {
            assert_eq!(world.name, "CliCommand");
            assert_eq!(world.imports.len(), 1);
            assert_eq!(world.exports.len(), 1);

            // Check import
            let import = &world.imports[0];
            assert_eq!(import.effect_name, "Stdout");
            assert_eq!(import.functions, vec!["write_via_stream"]);

            // Check export
            let export = &world.exports[0];
            assert_eq!(export.name, "run");
            assert!(export.is_async);
            assert!(export.params.is_empty());
            assert!(export.return_type.is_some());
        } else {
            panic!("expected world declaration");
        }
    }

    #[test]
    fn test_world_multiple_imports_exports() {
        let source = r#"
            world TestWorld {
                import Stdout {
                    write_via_stream,
                }

                import Stderr {
                    write_via_stream,
                }

                import Environment {
                    get_arguments,
                    get_environment,
                }

                export async fn run() -> Result<(), ()>;
                export fn get_version() -> string;
            }
        "#;

        let module = parse(source).unwrap();

        if let Item::World(world) = &module.items[0] {
            assert_eq!(world.name, "TestWorld");
            assert_eq!(world.imports.len(), 3);
            assert_eq!(world.exports.len(), 2);

            // Check Environment import has multiple functions
            let env_import = &world.imports[2];
            assert_eq!(env_import.effect_name, "Environment");
            assert_eq!(env_import.functions.len(), 2);
            assert_eq!(
                env_import.functions,
                vec!["get_arguments", "get_environment"]
            );

            // Check sync export
            let sync_export = &world.exports[1];
            assert_eq!(sync_export.name, "get_version");
            assert!(!sync_export.is_async);
        } else {
            panic!("expected world declaration");
        }
    }

    #[test]
    fn test_effect_with_async_method() {
        let source = r#"
            effect Http {
                async fn get(url: String) -> Response;
                fn status() -> i32;
            }
        "#;

        let module = parse(source).unwrap();

        if let Item::Effect(effect) = &module.items[0] {
            assert_eq!(effect.name, "Http");
            assert_eq!(effect.methods.len(), 2);

            // First method is async
            assert!(effect.methods[0].is_async);
            assert_eq!(effect.methods[0].name, "get");

            // Second method is sync
            assert!(!effect.methods[1].is_async);
            assert_eq!(effect.methods[1].name, "status");
        } else {
            panic!("expected effect declaration");
        }
    }

    #[test]
    fn test_effect_with_wasi_attribute() {
        let source = r#"
            pub effect Stdout {
                #[wasi("wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream")]
                async fn write_via_stream(data: Stream<u8>) -> Result<(), ErrorCode>;
            }
        "#;

        let module = parse(source).unwrap();

        if let Item::Effect(effect) = &module.items[0] {
            assert_eq!(effect.name, "Stdout");
            assert!(effect.is_pub);
            assert_eq!(effect.methods.len(), 1);

            let method = &effect.methods[0];
            assert!(method.is_async);
            assert_eq!(method.name, "write_via_stream");
            assert_eq!(method.attrs.len(), 1);

            let attr = &method.attrs[0];
            assert_eq!(attr.name, "wasi");
            assert!(attr.wasi_import.is_some());

            let wasi = attr.wasi_import.as_ref().unwrap();
            assert_eq!(wasi.namespace, "wasi");
            assert_eq!(wasi.package, "cli");
            assert_eq!(wasi.interface, "stdout");
            assert_eq!(wasi.function.as_deref(), Some("write-via-stream"));
        } else {
            panic!("expected effect declaration");
        }
    }

    #[test]
    fn test_export_with_params() {
        let source = r#"
            world TestWorld {
                export fn process(input: String, count: i32) -> Result<String, Error>;
            }
        "#;

        let module = parse(source).unwrap();

        if let Item::World(world) = &module.items[0] {
            assert_eq!(world.exports.len(), 1);
            let export = &world.exports[0];
            assert_eq!(export.name, "process");
            assert!(!export.is_async);
            assert_eq!(export.params.len(), 2);
            assert_eq!(export.params[0].name, "input");
            assert_eq!(export.params[1].name, "count");
        } else {
            panic!("expected world declaration");
        }
    }

    #[test]
    fn test_bodyless_function_declaration() {
        // Bodyless functions are compiler built-ins
        let source = r#"
            pub fn stream_new() -> i64;
            pub fn stream_write(tx: i32, ptr: i32, len: i32) -> i32;
            fn internal_helper();
        "#;

        let module = parse(source).unwrap();
        assert_eq!(module.items.len(), 3);

        // First function: pub fn stream_new() -> i64;
        if let Item::Function(func) = &module.items[0] {
            assert_eq!(func.name, "stream_new");
            assert!(func.is_pub);
            assert!(func.params.is_empty());
            assert!(func.return_type.is_some());
            assert!(func.body.is_none(), "bodyless function should have no body");
        } else {
            panic!("expected function");
        }

        // Second function: pub fn stream_write(tx: i32, ptr: i32, len: i32) -> i32;
        if let Item::Function(func) = &module.items[1] {
            assert_eq!(func.name, "stream_write");
            assert!(func.is_pub);
            assert_eq!(func.params.len(), 3);
            assert!(func.return_type.is_some());
            assert!(func.body.is_none(), "bodyless function should have no body");
        } else {
            panic!("expected function");
        }

        // Third function: fn internal_helper();
        if let Item::Function(func) = &module.items[2] {
            assert_eq!(func.name, "internal_helper");
            assert!(!func.is_pub);
            assert!(func.params.is_empty());
            assert!(func.return_type.is_none());
            assert!(func.body.is_none(), "bodyless function should have no body");
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_function_with_body_still_works() {
        // Make sure regular functions with bodies still parse correctly
        let source = r#"
            pub fn hello() -> string {
                return "hello";
            }
        "#;

        let module = parse(source).unwrap();
        assert_eq!(module.items.len(), 1);

        if let Item::Function(func) = &module.items[0] {
            assert_eq!(func.name, "hello");
            assert!(func.body.is_some(), "function with body should have body");
            let body = func.body.as_ref().unwrap();
            assert_eq!(body.stmts.len(), 1);
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_c_style_for_loop() {
        let source = r#"
            fn test() {
                for (let mut i = 0; i < 10; i = i + 1) {
                    println("hello");
                }
            }
        "#;

        let module = parse(source).unwrap();
        assert_eq!(module.items.len(), 1);

        if let Item::Function(func) = &module.items[0] {
            let body = func.body.as_ref().expect("function should have body");
            assert_eq!(body.stmts.len(), 1);

            if let Stmt::For(for_stmt) = &body.stmts[0] {
                // Check init
                assert!(for_stmt.init.is_some());
                if let Stmt::Let(let_stmt) = for_stmt.init.as_ref().unwrap().as_ref() {
                    if let Pattern::Ident(name) = &let_stmt.pattern {
                        assert_eq!(name, "i");
                    } else {
                        panic!("expected ident pattern");
                    }
                    assert!(let_stmt.is_mut);
                }

                // Check condition
                assert!(for_stmt.condition.is_some());

                // Check update
                assert!(for_stmt.update.is_some());

                // Check body
                assert_eq!(for_stmt.body.stmts.len(), 1);
            } else {
                panic!("expected for statement");
            }
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_for_loop_empty_parts() {
        // Test for loop with empty parts: for (;;) { }
        let source = r#"
            fn test() {
                for (;;) {
                    println("infinite");
                }
            }
        "#;

        let module = parse(source).unwrap();

        if let Item::Function(func) = &module.items[0] {
            let body = func.body.as_ref().unwrap();
            if let Stmt::For(for_stmt) = &body.stmts[0] {
                assert!(for_stmt.init.is_none());
                assert!(for_stmt.condition.is_none());
                assert!(for_stmt.update.is_none());
            } else {
                panic!("expected for statement");
            }
        }
    }

    #[test]
    fn test_assert_simple() {
        let source = r#"
            fn test() {
                assert x > 0;
            }
        "#;

        let module = parse(source).unwrap();

        if let Item::Function(func) = &module.items[0] {
            let body = func.body.as_ref().unwrap();
            if let Stmt::Assert(assert_stmt) = &body.stmts[0] {
                // Check that condition is parsed
                assert!(matches!(assert_stmt.condition, Expr::Binary(_)));
                // Check that message is None
                assert!(assert_stmt.message.is_none());
            } else {
                panic!("expected assert statement");
            }
        }
    }

    #[test]
    fn test_assert_with_message() {
        let source = r#"
            fn test() {
                assert x > 0, "x must be positive";
            }
        "#;

        let module = parse(source).unwrap();

        if let Item::Function(func) = &module.items[0] {
            let body = func.body.as_ref().unwrap();
            if let Stmt::Assert(assert_stmt) = &body.stmts[0] {
                // Check that condition is parsed
                assert!(matches!(assert_stmt.condition, Expr::Binary(_)));
                // Check that message is present and is a string literal
                assert!(assert_stmt.message.is_some());
                if let Some(Expr::Literal(lit)) = &assert_stmt.message {
                    assert!(matches!(&lit.value, Literal::String(s) if s == "x must be positive"));
                } else {
                    panic!("expected string literal message");
                }
            } else {
                panic!("expected assert statement");
            }
        }
    }

    #[test]
    fn test_assert_with_template_message() {
        let source = r#"
            fn test() {
                assert x > 0, `x must be positive, got {x}`;
            }
        "#;

        let module = parse(source).unwrap();

        if let Item::Function(func) = &module.items[0] {
            let body = func.body.as_ref().unwrap();
            if let Stmt::Assert(assert_stmt) = &body.stmts[0] {
                // Check that condition is parsed
                assert!(matches!(assert_stmt.condition, Expr::Binary(_)));
                // Check that message is a template string
                assert!(assert_stmt.message.is_some());
                assert!(matches!(
                    assert_stmt.message.as_ref(),
                    Some(Expr::TemplateString(_))
                ));
            } else {
                panic!("expected assert statement");
            }
        }
    }

    #[test]
    fn test_module_data_section() {
        let source = r#"fn main() { }
__DATA__
{"exit": 0, "stdout": "Hello\n"}"#;

        let module = parse(source).unwrap();

        // Should have parsed the function
        assert_eq!(module.items.len(), 1);
        assert!(matches!(&module.items[0], Item::Function(f) if f.name == "main"));

        // Should have captured the data section
        let data = module.data_section().unwrap();
        assert!(data.contains("\"exit\": 0"));
        assert!(data.contains("\"stdout\": \"Hello\\n\""));
    }

    #[test]
    fn test_module_no_data_section() {
        let source = "fn main() { }";

        let module = parse(source).unwrap();
        assert!(module.data_section().is_none());
    }

    #[test]
    fn test_module_data_section_preserves_content() {
        let source = r#"fn run() { }
__DATA__
line 1
line 2
{
  "key": "value"
}"#;

        let module = parse(source).unwrap();
        let data = module.data_section().unwrap();

        assert!(data.starts_with("line 1"));
        assert!(data.contains("line 2"));
        assert!(data.contains("\"key\": \"value\""));
    }
}
