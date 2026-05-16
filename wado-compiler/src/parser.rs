// The parser implementation of Wado with recursive descent parser.
// This module must be synchronized with syntax.rs (canonical syntax definition).

use crate::ast::{
    AssertStmt, AssignExpr, AssociatedConst, AssociatedTypeBinding, AssociatedTypeDecl, AstId,
    AttrArg, Attribute, BinaryExpr, BinaryOp, Block, BreakStmt, CallExpr, CastExpr,
    ChainedComparison, ClosureExpr, ClosureParam, CmImport, ComparisonChainExpr,
    CompoundAssignExpr, CompoundAssignOp, Condition, ConditionElement, ContinueStmt, EnumCase,
    EnumDecl, Expr, ExprStmt, FieldAccessExpr, FlagsDecl, FlagsVariant, ForOfStmt, ForStmt,
    FormatSpec, Function, FunctionType, GenericType, GlobalDecl, IdentExpr, IfExpr, IfStmt,
    ImplBlock, ImportAttributes, IndexExpr, InnerAttribute, InterfaceDecl, InterfaceMethod, Item,
    LabeledBlockStmt, LetStmt, Literal, LiteralExpr, LoopStmt, MatchArm, MatchExpr, MatchesExpr,
    MethodCallExpr, Module, NamedType, NamespacedGenericType, Newtype, Param, PathSegment, Pattern,
    RangeExpr, RangeKind, ResourceDecl, ReturnStmt, SelfKind, StaticMethodCallExpr, Stmt,
    StoresEntry, StructDecl, StructField, StructLiteralExpr, StructLiteralField,
    StructPatternField, TaskReturnStmt, TestDecl, TraitDecl, TryOpExpr, TupleLiteralExpr,
    TupleTypeDecl, Type, UnaryExpr, UnaryOp, UseDecl, UseItem, UseItemSimple, VariantCase,
    VariantDecl, WhileStmt, WorldDecl, WorldExport, WorldExportFn, WorldExportInterface,
    WorldImport,
};
use crate::token::{Span, Token, TokenKind};

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// Tracks when we've split a `GtGt` into two Gt tokens for nested generics.
    /// When true, the next `expect_gt` call should succeed without consuming a token.
    pending_gt: bool,
    /// When true, `Name {` is not parsed as a struct literal. This is set in
    /// expression contexts where a block `{` follows the expression (e.g.,
    /// if/while/match conditions), preventing ambiguity between struct literals
    /// and blocks.
    restrict_struct_literals: bool,
    /// Shebang line, passed from the lexer.
    shebang: Option<String>,
    /// Content of the __DATA__ section, passed from the lexer.
    data_section: Option<String>,
    /// Paths referenced by `#include_str` / `#include_bytes`, collected as they are parsed.
    include_paths: crate::hashmap::IndexSet<String>,
    /// Inner attributes parsed before items. Retained even if parsing fails later,
    /// so callers can check `has_todo()` after a parse error.
    parsed_inner_attributes: Vec<InnerAttribute>,
    /// Next [`AstId`] to allocate. Allocated densely starting from `0` in DFS
    /// parse order so that re-parsing the same source produces the same id
    /// sequence.
    next_ast_id: u32,
    /// Comments collected by the lexer, ordered by source position. The
    /// parser consumes them in lockstep with token consumption: at every
    /// `alloc_ast_id`, all still-unattached comments whose `span.start`
    /// precedes the next-to-consume token's start are attached to the new
    /// id's leading trivia. See [`crate::comment::TriviaMap`].
    comments: Vec<crate::comment::Comment>,
    /// Index of the next unattached comment in `comments`.
    comment_cursor: usize,
    /// Trivia attached to AST nodes during parsing. Exposed via
    /// [`Parser::take_trivia`] for the formatter pipeline.
    trivia: crate::comment::TriviaMap,
}

#[derive(Debug)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
}

impl From<ParseError> for crate::compiler_host::Diagnostic {
    fn from(e: ParseError) -> Self {
        use crate::compiler_host::{Code, DiagnosticSpan, Severity};
        Self {
            severity: Severity::Error,
            code: Code::InvalidSyntax,
            message: format!("parse error: {}", e.message),
            span: Some(DiagnosticSpan::from_span(&e.span, None)),
        }
    }
}

type ParseResult<T> = Result<T, ParseError>;

/// Snapshot of [`Parser`] state captured by [`Parser::checkpoint`] for
/// speculative parses that may need to backtrack. Restored via
/// [`Parser::restore`].
#[derive(Debug, Clone, Copy)]
struct ParserCheckpoint {
    pos: usize,
    comment_cursor: usize,
    next_ast_id: u32,
    pending_gt: bool,
}

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
        Self::build(tokens, None, None, Vec::new())
    }

    /// Creates a new parser with the given tokens, shebang, and data section.
    pub fn with_metadata(
        tokens: Vec<Token>,
        shebang: Option<String>,
        data_section: Option<String>,
    ) -> Self {
        Self::build(tokens, shebang, data_section, Vec::new())
    }

    /// Creates a new parser with full lexer output, including the comment
    /// stream so leading comments can be attached to AST nodes as they are
    /// allocated. Use this on the formatter path; the comment stream is
    /// otherwise harmless when omitted.
    pub fn with_trivia(
        tokens: Vec<Token>,
        shebang: Option<String>,
        data_section: Option<String>,
        comments: Vec<crate::comment::Comment>,
    ) -> Self {
        Self::build(tokens, shebang, data_section, comments)
    }

    fn build(
        tokens: Vec<Token>,
        shebang: Option<String>,
        data_section: Option<String>,
        comments: Vec<crate::comment::Comment>,
    ) -> Self {
        Self {
            tokens,
            pos: 0,
            pending_gt: false,
            restrict_struct_literals: false,
            shebang,
            data_section,
            include_paths: crate::hashmap::IndexSet::default(),
            parsed_inner_attributes: Vec::new(),
            next_ast_id: 0,
            comments,
            comment_cursor: 0,
            trivia: crate::comment::TriviaMap::new(),
        }
    }

    /// Allocate a fresh [`AstId`] for an AST node currently being constructed.
    /// Ids are dense in `0..next_ast_id` and assigned in parse order.
    ///
    /// Side effect: every comment in `self.comments` whose `span.start`
    /// precedes the next-to-consume token's start and has not already been
    /// attached is attached to the returned id as leading trivia. The
    /// outermost AST node being constructed at a given source position
    /// allocates first (DFS parse order), so the comment naturally lands
    /// on the AST node it semantically belongs to.
    fn alloc_ast_id(&mut self) -> crate::ast::AstId {
        let id = crate::ast::AstId(self.next_ast_id);
        self.next_ast_id += 1;
        // Comment-stream lockstep: pure tokenisations skip the Vec
        // allocation entirely. Most AST nodes have no preceding
        // comments, so the empty-cursor check happens on the parser
        // hot path.
        if self.comment_cursor < self.comments.len() {
            let next_token_start = self
                .tokens
                .get(self.pos)
                .map(|t| t.span.start)
                .unwrap_or(usize::MAX);
            if self.comments[self.comment_cursor].span.start < next_token_start {
                let mut leading: Vec<crate::comment::Comment> = Vec::new();
                while self.comment_cursor < self.comments.len()
                    && self.comments[self.comment_cursor].span.start < next_token_start
                {
                    leading.push(self.comments[self.comment_cursor].clone());
                    self.comment_cursor += 1;
                }
                self.trivia.attach_leading(id, leading);
            }
        }
        id
    }

    /// Consume and return the parser's accumulated trivia. Call this once
    /// after `parse()` returns Ok so the formatter pipeline can read
    /// leading-comment attachments. Sets file-tail dangling comments to
    /// any unattached residue.
    pub fn take_trivia(&mut self) -> crate::comment::TriviaMap {
        let mut comments = std::mem::take(&mut self.comments);
        let dangling = comments.split_off(self.comment_cursor);
        self.comment_cursor = 0;
        let mut trivia = std::mem::take(&mut self.trivia);
        trivia.set_dangling(dangling);
        trivia
    }

    /// Snapshot enough parser state to roll back a speculative parse
    /// without losing comments. `alloc_ast_id` advances `comment_cursor`
    /// and writes leading trivia keyed by the freshly allocated id, so a
    /// raw `self.pos = saved` style of backtrack would silently drop
    /// every comment consumed during the discarded branch (the cursor
    /// stays past them, the id they were attached to is never visited
    /// by the unparser). [`Parser::restore`] also rolls back
    /// `comment_cursor` and `next_ast_id`, and prunes any trivia entries
    /// allocated in the discarded range, so the re-parse sees the
    /// comment stream exactly as it was at the checkpoint.
    fn checkpoint(&self) -> ParserCheckpoint {
        ParserCheckpoint {
            pos: self.pos,
            comment_cursor: self.comment_cursor,
            next_ast_id: self.next_ast_id,
            pending_gt: self.pending_gt,
        }
    }

    fn restore(&mut self, cp: ParserCheckpoint) {
        self.pos = cp.pos;
        self.comment_cursor = cp.comment_cursor;
        self.trivia.discard_from(crate::ast::AstId(cp.next_ast_id));
        self.next_ast_id = cp.next_ast_id;
        self.pending_gt = cp.pending_gt;
    }

    /// Returns true if the parsed inner attributes include `#![TODO]`.
    /// Valid even after a parse error, since inner attributes are parsed first.
    pub fn has_todo(&self) -> bool {
        self.parsed_inner_attributes
            .iter()
            .any(|a| a.name == "TODO")
    }

    /// Parse an expression with struct literals restricted. Used in contexts
    /// where the expression is followed by a block `{` (if/while/match conditions).
    fn parse_expr_no_struct_literal(&mut self) -> ParseResult<Expr> {
        let saved = self.restrict_struct_literals;
        self.restrict_struct_literals = true;
        let result = self.parse_expr();
        self.restrict_struct_literals = saved;
        result
    }

    /// Check if `Name {` should be parsed as a struct literal by looking at
    /// the content inside the braces. This avoids relying on naming conventions
    /// to distinguish struct literals from blocks.
    ///
    /// Returns true if the current `{` token is followed by content that looks
    /// like struct field syntax: `{ field: ... }`, `{ field, ... }`, `{ field }`,
    /// or `{ }` (empty struct).
    fn looks_like_struct_literal_content(&self) -> bool {
        let after_brace = &self.peek_nth(1).kind;

        // Empty struct: `Name { }`
        if matches!(after_brace, TokenKind::RBrace) {
            return true;
        }

        // First token must be a valid field name (identifier, keyword, or string literal)
        let is_string_lit = matches!(after_brace, TokenKind::StringLit(_));
        if after_brace.as_ident_name().is_none()
            && after_brace.as_keyword_str().is_none()
            && !is_string_lit
        {
            return false;
        }

        // Check what follows the field name
        match &self.peek_nth(2).kind {
            // `{ field: value }` - but not `{ field:: ... }`
            TokenKind::Colon => !matches!(&self.peek_nth(3).kind, TokenKind::Colon),
            // `{ field, ... }` or `{ field }` - shorthand (not valid for string literal keys)
            TokenKind::Comma | TokenKind::RBrace => !is_string_lit,
            _ => false,
        }
    }

    pub fn parse(&mut self) -> ParseResult<Module> {
        // Parse inner attributes at the start of the module.
        // Store them so has_todo() works even if item parsing fails later.
        let inner_attributes = self.parse_inner_attributes()?;
        self.parsed_inner_attributes.clone_from(&inner_attributes);

        let mut items = Vec::new();

        while !self.is_at_end() {
            items.push(self.parse_item()?);
        }

        Ok(Module::with_metadata(
            items,
            inner_attributes,
            self.shebang.take(),
            self.data_section.take(),
            std::mem::take(&mut self.include_paths),
            self.next_ast_id,
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

    /// Check for `..` or `...` token (the latter will produce an error when consumed via `consume_dot_dot`).
    fn check_dot_dot_or_ellipsis(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::DotDot | TokenKind::DotDotDot)
    }

    /// Consume a `..` token. If `...` is found, emit a helpful error.
    fn consume_dot_dot(&mut self) -> ParseResult<Span> {
        if matches!(self.peek_kind(), TokenKind::DotDotDot) {
            return Err(
                self.error_at_span(self.peek().span, "unexpected `...`; did you mean `..`?")
            );
        }
        let span = self.peek().span;
        self.expect(&TokenKind::DotDot)?;
        Ok(span)
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

    /// Parse a comma-separated list of items until `terminator` is seen.
    /// Does NOT consume the terminator. Handles trailing commas.
    fn parse_comma_separated<T>(
        &mut self,
        terminator: &TokenKind,
        mut parse_item: impl FnMut(&mut Self) -> ParseResult<T>,
    ) -> ParseResult<Vec<T>> {
        let mut items = Vec::new();
        if !self.check(terminator) {
            items.push(parse_item(self)?);
            while self.check(&TokenKind::Comma) {
                self.advance();
                if self.check(terminator) {
                    break;
                }
                items.push(parse_item(self)?);
            }
        }
        Ok(items)
    }

    /// Parse a comma-separated list of types within angle brackets (`<T1, T2>`).
    /// Assumes the opening `<` has already been consumed.
    /// Handles `>>` splitting for nested generics via `pending_gt`.
    fn parse_type_args(&mut self) -> ParseResult<Vec<Type>> {
        let mut args = vec![self.parse_type()?];
        while !self.pending_gt && self.check(&TokenKind::Comma) {
            self.advance();
            args.push(self.parse_type()?);
        }
        self.expect_gt()?;
        Ok(args)
    }

    /// Parse a `+`-separated list of trait bounds: `Bound1 + Bound2 + ...`
    fn parse_trait_bounds(&mut self) -> ParseResult<Vec<crate::ast::TraitBound>> {
        let mut bounds = vec![self.parse_trait_bound()?];
        while self.check(&TokenKind::Plus) {
            self.advance();
            bounds.push(self.parse_trait_bound()?);
        }
        Ok(bounds)
    }

    /// Parse variant pattern bindings: `Name(pat1, pat2, ...)`
    /// Assumes the name has been consumed and current token is `(`.
    ///
    /// `name_id` / `name_span` identify the case-name identifier (the `Some`
    /// in `Some(x)`, or the `Some` part of `Option::Some(x)`). They are used
    /// by the resolver to record use→def references for LSP navigation.
    fn parse_variant_pattern(
        &mut self,
        name: String,
        qualifier: Option<Type>,
        start_span: Span,
        name_id: Option<AstId>,
        name_span: Span,
    ) -> ParseResult<Pattern> {
        self.advance(); // consume (
        let bindings = self.parse_comma_separated(&TokenKind::RParen, Self::parse_pattern)?;
        let end_span = self.peek().span;
        self.expect(&TokenKind::RParen)?;
        Ok(Pattern::Variant {
            variant_name: name,
            variant_qualifier: qualifier,
            name_id,
            name_span,
            bindings,
            span: start_span.merge(&end_span),
        })
    }

    fn parse_pattern_qualified_case_from_first_segment(
        &mut self,
        first_name: String,
        start_span: Span,
    ) -> ParseResult<Pattern> {
        let first_type = if self.check(&TokenKind::Lt) {
            self.advance();
            let args = self.parse_type_args()?;
            Type::Generic(GenericType {
                id: self.alloc_ast_id(),
                name: first_name,
                args,
                span: start_span,
            })
        } else {
            Type::Named(NamedType {
                id: self.alloc_ast_id(),
                name: first_name,
                span: start_span,
                source_interface: None,
            })
        };

        self.expect(&TokenKind::ColonColon)?;
        let second_span = self.peek().span;
        let second_name = self.consume_ident()?;
        let second_args = if self.check(&TokenKind::Lt) {
            self.advance();
            self.parse_type_args()?
        } else {
            Vec::new()
        };

        let (qualifier, case_name, case_span) = if self.check(&TokenKind::ColonColon) {
            self.advance();
            let case_span = self.peek().span;
            let case_name = self.consume_ident()?;
            let qualifier = match first_type {
                Type::Named(ref t) => Type::NamespacedGeneric(NamespacedGenericType {
                    id: self.alloc_ast_id(),
                    namespace: t.name.clone(),
                    name: second_name,
                    args: second_args,
                    span: start_span,
                }),
                _ => {
                    return Err(ParseError {
                        message: "invalid qualified case pattern".to_string(),
                        span: start_span,
                    });
                }
            };
            (qualifier, case_name, case_span)
        } else {
            (first_type, second_name, second_span)
        };

        let case_id = self.alloc_ast_id();
        if self.check(&TokenKind::LParen) {
            return self.parse_variant_pattern(
                case_name,
                Some(qualifier),
                start_span,
                Some(case_id),
                case_span,
            );
        }
        let end_span = self.peek().span;
        Ok(Pattern::Variant {
            variant_name: case_name,
            variant_qualifier: Some(qualifier),
            name_id: Some(case_id),
            name_span: case_span,
            bindings: vec![],
            span: start_span.merge(&end_span),
        })
    }

    fn consume_ident(&mut self) -> ParseResult<String> {
        self.consume_ident_with_span().map(|(name, _)| name)
    }

    /// Consume an identifier (or contextual keyword usable as an identifier) and
    /// return both the name and the span of the identifier token itself.
    ///
    /// Use this when recording `name_span` on AST declaration nodes. The token span
    /// is captured before the token is advanced past, so it accurately covers the
    /// identifier alone (not any surrounding item extent).
    fn consume_ident_with_span(&mut self) -> ParseResult<(String, Span)> {
        // Accept regular identifiers and contextual keywords (flags, type)
        if let Some(name) = self.peek_kind().as_ident_name() {
            let name = name.to_string();
            let span = self.peek().span;
            self.advance();
            Ok((name, span))
        } else {
            Err(ParseError {
                message: format!("expected identifier, found {:?}", self.peek_kind()),
                span: self.peek().span,
            })
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

        // Parse visibility: pub
        let is_pub = if self.check(&TokenKind::Pub) {
            self.advance();
            true
        } else {
            false
        };

        // Parse export keyword (for CM boundary export)
        let is_export = if self.check(&TokenKind::Export) {
            self.advance();
            true
        } else {
            false
        };

        // Parse optional `async` modifier after `export` (only valid on exported fns)
        let is_async = if is_export && self.check(&TokenKind::Async) {
            self.advance();
            true
        } else {
            false
        };

        // Check for contextual keyword "test" (identifier followed by string or block)
        if let TokenKind::Ident(name) = self.peek_kind()
            && name == "test"
        {
            return self.parse_test_decl(attrs).map(Item::Test);
        }

        match self.peek_kind() {
            TokenKind::Use => self.parse_use_decl(is_pub).map(Item::Use),
            TokenKind::Fn => self
                .parse_function(is_pub, is_export, is_async, attrs)
                .map(Item::Function),
            TokenKind::Interface => self
                .parse_interface_decl(is_pub, attrs)
                .map(Item::Interface),
            TokenKind::Struct => self.parse_struct_decl(is_pub, attrs).map(Item::Struct),
            TokenKind::Enum => self.parse_enum_decl(is_pub, attrs).map(Item::Enum),
            TokenKind::Variant => self.parse_variant_decl(is_pub, attrs).map(Item::Variant),
            TokenKind::Flags => self.parse_flags_decl(is_pub, attrs).map(Item::Flags),
            TokenKind::Type => self.parse_type_decl(is_pub, attrs),
            TokenKind::Impl => self.parse_impl_block().map(Item::Impl),
            TokenKind::Trait => self.parse_trait_decl(is_pub, attrs).map(Item::Trait),
            TokenKind::Resource => self.parse_resource_decl(is_pub, attrs).map(Item::Resource),
            TokenKind::World => self.parse_world_decl(is_pub, attrs).map(Item::World),
            TokenKind::Global => self.parse_global_decl(is_pub, attrs).map(Item::Global),
            _ => Err(ParseError {
                message: format!("expected item, found {:?}", self.peek_kind()),
                span: self.peek().span,
            }),
        }
    }

    /// Parse test declaration: `[#[attr]] test "name" { ... }` or `[#[attr]] test { ... }`
    fn parse_test_decl(&mut self, attributes: Vec<Attribute>) -> ParseResult<TestDecl> {
        let id = self.alloc_ast_id();
        let start_span = self.peek().span;
        // Consume the "test" identifier (contextual keyword)
        self.advance();

        // Optional test name (string literal)
        let name = if let TokenKind::StringLit(raw) = self.peek_kind().clone() {
            self.advance();
            Some(raw)
        } else {
            None
        };

        // Parse body block
        let body = self.parse_block()?;
        let end_span = body.span;

        Ok(TestDecl {
            id,
            attributes,
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
        let id = self.alloc_ast_id();
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
        let (name, name_span) = self.consume_ident_with_span()?;

        // Type annotation (required)
        self.expect(&TokenKind::Colon)?;
        let ty = self.parse_type()?;

        // Initializer (required)
        self.expect(&TokenKind::Eq)?;
        let initializer = self.parse_expr()?;

        let end_span = self.peek().span;
        self.expect(&TokenKind::Semicolon)?;

        Ok(GlobalDecl {
            id,
            name,
            name_span,
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

    /// Parse a single inner attribute: `#![name]`, `#![name("arg")]`, or
    /// `#![name(key = "value", other = "v")]`.
    fn parse_inner_attribute(&mut self) -> ParseResult<InnerAttribute> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Hash)?;
        self.expect(&TokenKind::Not)?;
        self.expect(&TokenKind::LBracket)?;

        let name = self.consume_ident()?;

        let args = if self.check(&TokenKind::LParen) {
            self.advance();
            let args = self.parse_attr_arg_list()?;
            self.expect(&TokenKind::RParen)?;
            args
        } else {
            Vec::new()
        };

        self.expect(&TokenKind::RBracket)?;

        Ok(InnerAttribute {
            name,
            args,
            span: start_span,
        })
    }

    /// Parse a comma-separated list of attribute arguments up to the closing
    /// delimiter. Shared between inner attributes (`#![...]`) and outer
    /// attributes (`#[...]`). Does not consume the closing `)`.
    fn parse_attr_arg_list(&mut self) -> ParseResult<Vec<AttrArg>> {
        let mut args: Vec<AttrArg> = Vec::new();
        loop {
            let arg = match self.peek_kind().clone() {
                TokenKind::StringLit(raw) => {
                    self.advance();
                    AttrArg::Str(raw)
                }
                TokenKind::Ident(value) => {
                    self.advance();
                    // Check if this identifier is followed by '=' making it a key=value pair
                    if self.check(&TokenKind::Eq) {
                        self.advance();
                        match self.peek_kind().clone() {
                            TokenKind::StringLit(val) => {
                                self.advance();
                                AttrArg::KeyValue(value, val)
                            }
                            TokenKind::LBracket => {
                                self.advance();
                                let mut items: Vec<String> = Vec::new();
                                if !self.check(&TokenKind::RBracket) {
                                    loop {
                                        if let TokenKind::StringLit(item) = self.peek_kind().clone()
                                        {
                                            self.advance();
                                            items.push(item);
                                        } else {
                                            let span = self.peek().span;
                                            return Err(self.error_at_span(
                                                span,
                                                "expected string literal in attribute array",
                                            ));
                                        }
                                        if self.check(&TokenKind::Comma) {
                                            self.advance();
                                            if self.check(&TokenKind::RBracket) {
                                                break;
                                            }
                                        } else {
                                            break;
                                        }
                                    }
                                }
                                self.expect(&TokenKind::RBracket)?;
                                AttrArg::KeyArray(value, items)
                            }
                            _ => AttrArg::Ident(value),
                        }
                    } else {
                        AttrArg::Ident(value)
                    }
                }
                TokenKind::NumberLit(value) => {
                    self.advance();
                    AttrArg::Number(value)
                }
                _ => break,
            };
            args.push(arg);
            if self.check(&TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        Ok(args)
    }

    fn parse_attribute(&mut self) -> ParseResult<Attribute> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Hash)?;
        self.expect(&TokenKind::LBracket)?;

        let name = self.consume_ident()?;

        let args = if self.check(&TokenKind::LParen) {
            self.advance();
            let args = self.parse_attr_arg_list()?;
            self.expect(&TokenKind::RParen)?;
            args
        } else {
            Vec::new()
        };

        self.expect(&TokenKind::RBracket)?;

        // Parse CM import path if this is a cm attribute
        let cm_import = if name == "cm" {
            args.first().and_then(|s| CmImport::parse(s.as_str()))
        } else {
            None
        };

        Ok(Attribute {
            name,
            args,
            cm_import,
            span: start_span,
        })
    }

    fn parse_resource_decl(
        &mut self,
        is_pub: bool,
        attrs: Vec<Attribute>,
    ) -> ParseResult<ResourceDecl> {
        let id = self.alloc_ast_id();
        let start_span = self.peek().span;
        self.expect(&TokenKind::Resource)?;
        let name = self.consume_ident()?;

        // Parse optional generic type parameters: `resource Future<T> { ... }`
        let type_params = self.parse_generic_params()?;

        // Either `resource Name;` (opaque) or `resource Name { ... }` (with methods)
        let (methods, end_span) = if self.check(&TokenKind::LBrace) {
            self.advance(); // consume '{'

            let mut methods = Vec::new();
            while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
                // Reuse effect method parser for resource methods
                methods.push(self.parse_interface_method()?);
            }

            let end = self.expect(&TokenKind::RBrace)?.span;
            (methods, end)
        } else {
            let end = self.expect(&TokenKind::Semicolon)?.span;
            (Vec::new(), end)
        };

        Ok(ResourceDecl {
            id,
            name,
            is_pub,
            type_params,
            attrs,
            methods,
            span: start_span.merge(&end_span),
        })
    }

    /// Parse use declaration with ESM-like syntax:
    /// `use {items} from "source";`
    /// `use {items} from "source" with { version: "1.0" };`
    /// `use _ from "source";` (wildcard: load module without binding names)
    ///
    /// Items can be:
    /// - Simple: `name` or `name as alias`
    /// - Effect functions: `Effect::{func1, func2}` or `Effect::{func1 as alias}`
    /// - Wildcard: `_` (no braces needed)
    fn parse_use_decl(&mut self, is_pub: bool) -> ParseResult<UseDecl> {
        let id = self.alloc_ast_id();
        let start_span = self.peek().span;
        self.expect(&TokenKind::Use)?;

        // Check for wildcard import: `use _ from "..."`
        let items = if matches!(self.peek_kind(), TokenKind::Ident(name) if name == "_") {
            self.advance(); // consume `_`
            vec![UseItem::Wildcard]
        }
        // Check for namespace import: `use name from "..."` (ident followed by `from`)
        else if matches!(self.peek_kind(), TokenKind::Ident(_))
            && matches!(self.peek_nth(1).kind, TokenKind::From)
        {
            let name = self.consume_ident()?;
            vec![UseItem::Namespace { name }]
        } else {
            // Parse items: `{...}`
            self.expect(&TokenKind::LBrace)?;
            let items = self.parse_use_items()?;
            self.expect(&TokenKind::RBrace)?;
            items
        };

        // Expect `from`
        self.expect(&TokenKind::From)?;

        // Parse source string
        let source_span = self.peek().span;
        let source_id = self.alloc_ast_id();
        let source = self.consume_string()?;

        // Parse optional `with { ... }` attributes
        let attributes = if self.check(&TokenKind::With) {
            self.advance();
            Some(self.parse_import_attributes()?)
        } else {
            None
        };

        let semicolon = self.expect(&TokenKind::Semicolon)?;
        let end_span = semicolon.span;

        Ok(UseDecl {
            id,
            is_pub,
            source,
            source_span,
            source_id,
            items,
            attributes,
            span: start_span.merge(&end_span),
        })
    }

    /// Parse use items inside `{...}`
    fn parse_use_items(&mut self) -> ParseResult<Vec<UseItem>> {
        let mut items = vec![];

        if self.check(&TokenKind::RBrace) {
            return Ok(items);
        }

        loop {
            let name_span = self.peek().span;
            let name = self.consume_ident()?;

            // Check if this is an effect with functions: `Effect::{...}`
            if self.check(&TokenKind::ColonColon) {
                self.advance(); // consume `::`
                self.expect(&TokenKind::LBrace)?;

                // Parse function list inside Effect::{...}
                let functions = self.parse_use_item_simple_list()?;
                self.expect(&TokenKind::RBrace)?;

                items.push(UseItem::InterfaceFunctions {
                    interface_name: name,
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
                items.push(UseItem::Simple {
                    id: self.alloc_ast_id(),
                    name,
                    name_span,
                    alias,
                });
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
    ///
    /// Attribute values are a generic scalar/array/object tree. Unknown
    /// top-level keys are accepted here and validated downstream (e.g. the
    /// Kiln inline-generator collector rejects non-`generator` siblings).
    fn parse_import_attributes(&mut self) -> ParseResult<ImportAttributes> {
        self.expect(&TokenKind::LBrace)?;

        let mut attrs = ImportAttributes::default();

        if !self.check(&TokenKind::RBrace) {
            loop {
                let key = self.consume_ident()?;
                self.expect(&TokenKind::Colon)?;
                let value = self.parse_attr_value()?;
                attrs.entries.insert(key, value);

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

    /// Parse a single [`AttrValue`]: scalar, array, or nested object.
    fn parse_attr_value(&mut self) -> ParseResult<crate::ast::AttrValue> {
        let span = self.peek().span;
        // Handle unary minus on numeric literals (e.g. `-3`, `-1.5`).
        let negate = if matches!(self.peek_kind(), TokenKind::Minus) {
            self.advance();
            true
        } else {
            false
        };
        match self.peek_kind().clone() {
            TokenKind::StringLit(_) => {
                if negate {
                    return Err(ParseError {
                        message: "cannot negate a string literal".to_string(),
                        span,
                    });
                }
                let s = self.consume_string()?;
                Ok(crate::ast::AttrValue::String(s))
            }
            TokenKind::NumberLit(repr) => {
                self.advance();
                parse_attr_number(&repr, negate, span)
            }
            TokenKind::True => {
                if negate {
                    return Err(ParseError {
                        message: "cannot negate a boolean".to_string(),
                        span,
                    });
                }
                self.advance();
                Ok(crate::ast::AttrValue::Bool(true))
            }
            TokenKind::False => {
                if negate {
                    return Err(ParseError {
                        message: "cannot negate a boolean".to_string(),
                        span,
                    });
                }
                self.advance();
                Ok(crate::ast::AttrValue::Bool(false))
            }
            TokenKind::LBracket => {
                if negate {
                    return Err(ParseError {
                        message: "cannot negate an array".to_string(),
                        span,
                    });
                }
                self.advance();
                let mut items = Vec::new();
                if !self.check(&TokenKind::RBracket) {
                    loop {
                        items.push(self.parse_attr_value()?);
                        if !self.check(&TokenKind::Comma) {
                            break;
                        }
                        self.advance();
                        if self.check(&TokenKind::RBracket) {
                            break;
                        }
                    }
                }
                self.expect(&TokenKind::RBracket)?;
                Ok(crate::ast::AttrValue::Array(items))
            }
            TokenKind::LBrace => {
                if negate {
                    return Err(ParseError {
                        message: "cannot negate an object".to_string(),
                        span,
                    });
                }
                self.advance();
                let mut obj: crate::hashmap::IndexMap<String, crate::ast::AttrValue> =
                    crate::hashmap::IndexMap::default();
                if !self.check(&TokenKind::RBrace) {
                    loop {
                        let key = self.consume_ident()?;
                        self.expect(&TokenKind::Colon)?;
                        let v = self.parse_attr_value()?;
                        obj.insert(key, v);
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
                Ok(crate::ast::AttrValue::Object(obj))
            }
            other => Err(ParseError {
                message: format!(
                    "expected attribute value (string, number, bool, array, or object), got {other:?}"
                ),
                span,
            }),
        }
    }

    /// Consume a string literal and return its raw text (escape sequences not interpreted).
    fn consume_string(&mut self) -> ParseResult<String> {
        match &self.peek().kind {
            TokenKind::StringLit(raw) => {
                let raw = raw.clone();
                self.advance();
                Ok(raw)
            }
            _ => Err(ParseError {
                message: "expected string literal".to_string(),
                span: self.peek().span,
            }),
        }
    }

    fn parse_function(
        &mut self,
        is_pub: bool,
        is_export: bool,
        is_async: bool,
        attrs: Vec<Attribute>,
    ) -> ParseResult<Function> {
        let id = self.alloc_ast_id();
        let start_span = self.peek().span;
        self.expect(&TokenKind::Fn)?;

        let (name, name_span) = self.consume_ident_with_span()?;

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

        let (effects, effect_ids, stores) = self.parse_with_clause()?;

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
            id,
            name,
            name_span,
            is_pub,
            is_export,
            is_async,
            type_params,
            attrs,
            params,
            return_type,
            effects,
            effect_ids,
            stores,
            body,
            span,
        })
    }

    fn parse_param_list(&mut self) -> ParseResult<Vec<Param>> {
        let params = self.parse_comma_separated(&TokenKind::RParen, Self::parse_param)?;
        let mut seen_default = false;
        for p in &params {
            if p.default.is_some() {
                seen_default = true;
            } else if seen_default && !matches!(p.self_kind, SelfKind::Ref | SelfKind::MutRef) {
                return Err(ParseError {
                    message: format!(
                        "parameter '{}' has no default — parameters without defaults cannot follow parameters with defaults",
                        p.name
                    ),
                    span: p.span,
                });
            }
        }
        Ok(params)
    }

    fn parse_param(&mut self) -> ParseResult<Param> {
        let id = self.alloc_ast_id();
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
                let self_span = self.peek().span;
                self.advance();
                let self_type = Type::Named(NamedType {
                    id: self.alloc_ast_id(),
                    name: "Self".to_string(),
                    span: start_span,
                    source_interface: None,
                });
                let ty = if is_mut {
                    Type::MutReference(Box::new(self_type))
                } else {
                    Type::Reference(Box::new(self_type))
                };
                return Ok(Param {
                    id,
                    name: "self".to_string(),
                    name_span: self_span,
                    ty,
                    self_kind: if is_mut {
                        SelfKind::MutRef
                    } else {
                        SelfKind::Ref
                    },
                    is_mut: false,
                    default: None,
                    span: start_span,
                });
            }

            return Err(ParseError {
                message: "expected 'self' after '&' in method parameter".to_string(),
                span: self.peek().span,
            });
        }

        // self by value is not allowed — use &self or &mut self instead
        if let TokenKind::Ident(name) = self.peek_kind()
            && name == "self"
            && !matches!(self.peek_nth(1).kind, TokenKind::Colon)
        {
            return Err(ParseError {
                message: "`self` by value is not allowed; use `&self` or `&mut self` instead"
                    .to_string(),
                span: self.peek().span,
            });
        }

        let is_mut = if self.check(&TokenKind::Mut) {
            self.advance();
            true
        } else {
            false
        };

        let (name, name_span) = self.consume_ident_with_span()?;
        self.expect(&TokenKind::Colon)?;
        let ty = self.parse_type()?;

        let default = if self.check(&TokenKind::Eq) {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };

        Ok(Param {
            id,
            name,
            name_span,
            ty,
            self_kind: SelfKind::None,
            is_mut,
            default,
            span: start_span,
        })
    }

    /// Parse `with Effect1, Effect2, stores[param1, param2]` clause.
    /// Returns (effects, stores). The `stores` keyword can appear anywhere in the effect list.
    fn parse_with_clause(&mut self) -> ParseResult<(Vec<String>, Vec<(AstId, Span)>, Vec<String>)> {
        if !self.check(&TokenKind::With) {
            return Ok((Vec::new(), Vec::new(), Vec::new()));
        }
        self.advance();

        // Check if the first item is `stores[...]`
        if self.check(&TokenKind::Stores) {
            let stores = self.parse_stores_list()?;
            return Ok((Vec::new(), Vec::new(), stores));
        }

        let (first_name, first_span) = self.consume_ident_with_span()?;
        let mut effects = vec![first_name];
        let mut effect_ids = vec![(self.alloc_ast_id(), first_span)];

        while self.check(&TokenKind::Comma) {
            // Look ahead: if the token after comma is `ident :`, it's a parameter
            // declaration, not another effect name. Stop consuming.
            if matches!(self.peek_nth(1).kind, TokenKind::Ident(_))
                && self.peek_nth(2).kind == TokenKind::Colon
            {
                break;
            }
            self.advance();
            // Check if next item is `stores[...]`
            if self.check(&TokenKind::Stores) {
                let stores = self.parse_stores_list()?;
                return Ok((effects, effect_ids, stores));
            }
            let (name, span) = self.consume_ident_with_span()?;
            effects.push(name);
            effect_ids.push((self.alloc_ast_id(), span));
        }

        Ok((effects, effect_ids, Vec::new()))
    }

    /// Parse `stores[name1, name2]` — the `stores` keyword has already been peeked.
    /// Empty lists (`stores[]`) and a trailing comma are allowed for syntactic
    /// consistency with other comma-separated lists; both are no-ops semantically.
    fn parse_stores_list(&mut self) -> ParseResult<Vec<String>> {
        self.expect(&TokenKind::Stores)?;
        self.expect(&TokenKind::LBracket)?;
        let names = self.parse_comma_separated(&TokenKind::RBracket, Self::consume_ident)?;
        self.expect(&TokenKind::RBracket)?;
        Ok(names)
    }

    /// Parse `with` clause for function types: `with Effect1, stores[0, 1]`
    /// In function type position, stores entries are positional indices.
    fn parse_with_clause_for_fn_type(
        &mut self,
    ) -> ParseResult<(Vec<String>, Vec<(AstId, Span)>, Vec<StoresEntry>)> {
        if !self.check(&TokenKind::With) {
            return Ok((Vec::new(), Vec::new(), Vec::new()));
        }
        self.advance();

        // Check if the first item is `stores[...]`
        if self.check(&TokenKind::Stores) {
            let stores = self.parse_stores_list_for_fn_type()?;
            return Ok((Vec::new(), Vec::new(), stores));
        }

        let (first_name, first_span) = self.consume_ident_with_span()?;
        let mut effects = vec![first_name];
        let mut effect_ids = vec![(self.alloc_ast_id(), first_span)];

        while self.check(&TokenKind::Comma) {
            // Lookahead: if the token after comma is `ident:`, this is a parameter
            // in the enclosing parameter list, not another effect. Stop here.
            if matches!(self.peek_nth(1).kind, TokenKind::Ident(_))
                && self.peek_nth(2).kind == TokenKind::Colon
            {
                break;
            }
            self.advance();
            if self.check(&TokenKind::Stores) {
                let stores = self.parse_stores_list_for_fn_type()?;
                return Ok((effects, effect_ids, stores));
            }
            let (name, span) = self.consume_ident_with_span()?;
            effects.push(name);
            effect_ids.push((self.alloc_ast_id(), span));
        }

        Ok((effects, effect_ids, Vec::new()))
    }

    /// Parse `stores[0, 1]` or `stores[name]` in function type position.
    /// Empty lists and a trailing comma are allowed for syntactic consistency
    /// with other comma-separated lists; both are no-ops semantically.
    fn parse_stores_list_for_fn_type(&mut self) -> ParseResult<Vec<StoresEntry>> {
        self.expect(&TokenKind::Stores)?;
        self.expect(&TokenKind::LBracket)?;
        let entries = self.parse_comma_separated(&TokenKind::RBracket, Self::parse_stores_entry)?;
        self.expect(&TokenKind::RBracket)?;
        Ok(entries)
    }

    /// Parse a single stores entry: either a number (positional) or identifier (named).
    fn parse_stores_entry(&mut self) -> ParseResult<StoresEntry> {
        if let TokenKind::NumberLit(num) = self.peek_kind() {
            let n = num.parse::<u32>().map_err(|_| ParseError {
                message: "stores index must be a non-negative integer".to_string(),
                span: self.peek().span,
            })?;
            self.advance();
            Ok(StoresEntry::Index(n))
        } else {
            Ok(StoresEntry::Name(self.consume_ident()?))
        }
    }

    fn parse_block(&mut self) -> ParseResult<Block> {
        let id = self.alloc_ast_id();
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
            id,
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

        // Check for `task return expr;` — identifier "task" followed by keyword `return`
        if let TokenKind::Ident(name) = self.peek_kind()
            && name == "task"
            && matches!(self.peek_nth(1).kind, TokenKind::Return)
        {
            return self.parse_task_return_stmt();
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
            TokenKind::Match => self.parse_match_stmt(),
            TokenKind::With => self.parse_with_handler_stmt(),
            _ => self.parse_expr_stmt_in_block(),
        }
    }

    /// Parse a match statement (no trailing semicolon required, like if/while/loop).
    fn parse_match_stmt(&mut self) -> ParseResult<Stmt> {
        let expr = self.parse_match_expr()?;
        // Trailing semicolon is optional (consumed if present)
        if self.check(&TokenKind::Semicolon) {
            self.advance();
        }
        let Expr::Match(m) = expr else {
            unreachable!("parse_match_expr must return Expr::Match");
        };
        Ok(Stmt::Match(m))
    }

    /// Parse a `with E => h do { ... }` statement.
    ///
    /// The trailing `}` of the do-block makes the statement boundary
    /// unambiguous, so the trailing semicolon is optional — same as
    /// `if`, `while`, `for`, `loop`, and `match` in statement position.
    /// The expression is wrapped in a `Stmt::Expr` so the rest of the
    /// pipeline (effect-check, TIR lowering, dispatch synthesis)
    /// continues to see a single `Expr::WithHandler`.
    fn parse_with_handler_stmt(&mut self) -> ParseResult<Stmt> {
        let id = self.alloc_ast_id();
        let expr = self.parse_with_handler_expr()?;
        if self.check(&TokenKind::Semicolon) {
            self.advance();
        }
        let span = expr.span();
        Ok(Stmt::Expr(crate::ast::ExprStmt { id, expr, span }))
    }

    /// Parse an expression statement in a block, with optional trailing semicolon
    fn parse_expr_stmt_in_block(&mut self) -> ParseResult<Stmt> {
        let start_span = self.peek().span;
        let id = self.alloc_ast_id();
        let expr = self.parse_expr()?;

        // Semicolon is optional if followed by `}` (end of block)
        let end_span = if self.check(&TokenKind::Semicolon) {
            self.advance().span
        } else if !self.check(&TokenKind::RBrace) {
            return Err(ParseError {
                message: format!("expected `;` or `}}`, found {:?}", self.peek_kind()),
                span: self.peek().span,
            });
        } else {
            expr.span()
        };

        Ok(Stmt::Expr(ExprStmt {
            id,
            expr,
            span: start_span.merge(&end_span),
        }))
    }

    fn parse_labeled_block_stmt(&mut self) -> ParseResult<Stmt> {
        let start_span = self.peek().span;
        let id = self.alloc_ast_id();

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
            id,
            label,
            block,
            span: start_span.merge(&end_span),
        }))
    }

    fn parse_assert_stmt(&mut self) -> ParseResult<Stmt> {
        let start_span = self.peek().span;
        let id = self.alloc_ast_id();
        self.expect(&TokenKind::Assert)?;

        let condition = self.parse_expr()?;

        // Check for optional message after comma
        let message = if self.check(&TokenKind::Comma) {
            self.advance(); // consume comma
            Some(self.parse_expr()?)
        } else {
            None
        };

        let semi_span = self.expect(&TokenKind::Semicolon)?.span;

        Ok(Stmt::Assert(AssertStmt {
            id,
            condition,
            message,
            span: start_span.merge(&semi_span),
        }))
    }

    fn parse_let_stmt(&mut self) -> ParseResult<Stmt> {
        let mut stmt = self.parse_let_stmt_inner()?;
        let semi_span = self.expect(&TokenKind::Semicolon)?.span;
        // Extend let statement span to include the trailing semicolon
        if let Stmt::Let(ref mut l) = stmt {
            l.span = l.span.merge(&semi_span);
        }
        Ok(stmt)
    }

    /// Parse let statement without consuming trailing semicolon
    /// Used in for loop init: `for (let mut i = 0; ...)`
    fn parse_let_stmt_inner(&mut self) -> ParseResult<Stmt> {
        let start_span = self.peek().span;
        // Allocate the let-stmt id BEFORE descending into children so
        // leading comments preceding `let` (e.g. `// note\nlet x = ...`)
        // attach to the stmt itself rather than to its first child id.
        // Matches the `alloc_ast_id`-at-start convention used by every
        // outermost-first parser (`parse_block`, `parse_function`,
        // `parse_match_arm`, …).
        let id = self.alloc_ast_id();

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

        let pattern_start_span = self.peek().span;
        let pattern = self.parse_pattern()?;
        let name_span = pattern_start_span;

        let ty = if self.check(&TokenKind::Colon) {
            self.advance();
            Some(self.parse_type()?)
        } else {
            None
        };

        let (value, end_span) = if self.check(&TokenKind::Eq) {
            self.advance();
            let v = self.parse_expr()?;
            let s = v.span();
            (Some(v), s)
        } else {
            // No initializer: type annotation is required
            if ty.is_none() {
                let span = self.peek().span;
                return Err(ParseError {
                    message:
                        "type annotation required for variable declaration without initializer"
                            .to_string(),
                    span,
                });
            }
            // Use current position (before the semicolon) as end span
            (None, self.peek().span)
        };

        Ok(Stmt::Let(LetStmt {
            id,
            pattern,
            name_span,
            is_mut,
            is_reactive,
            ty,
            value,
            span: start_span.merge(&end_span),
        }))
    }

    fn parse_return_stmt(&mut self) -> ParseResult<Stmt> {
        let start_span = self.peek().span;
        let id = self.alloc_ast_id();
        self.expect(&TokenKind::Return)?;

        let value = if self.check(&TokenKind::Semicolon) || self.check(&TokenKind::RBrace) {
            None
        } else {
            Some(self.parse_expr()?)
        };

        let end_span = if self.check(&TokenKind::Semicolon) {
            self.advance().span
        } else if self.check(&TokenKind::RBrace) {
            value.as_ref().map(Expr::span).unwrap_or(start_span)
        } else {
            return Err(self.error_at_span(
                self.peek().span,
                &format!("expected Semicolon, found {:?}", self.peek_kind()),
            ));
        };

        Ok(Stmt::Return(ReturnStmt {
            id,
            value,
            span: start_span.merge(&end_span),
        }))
    }

    /// Parse `task return expr;` — delivers the async task result without terminating the function.
    fn parse_task_return_stmt(&mut self) -> ParseResult<Stmt> {
        let start_span = self.peek().span;
        let id = self.alloc_ast_id();
        // Consume the `task` identifier
        self.advance();
        // Consume the `return` keyword
        self.expect(&TokenKind::Return)?;

        let value = self.parse_expr()?;

        let end_span = if self.check(&TokenKind::Semicolon) {
            self.advance().span
        } else if self.check(&TokenKind::RBrace) {
            value.span()
        } else {
            return Err(self.error_at_span(
                self.peek().span,
                &format!("expected Semicolon, found {:?}", self.peek_kind()),
            ));
        };

        Ok(Stmt::TaskReturn(TaskReturnStmt {
            id,
            value,
            span: start_span.merge(&end_span),
        }))
    }

    fn parse_if_stmt(&mut self) -> ParseResult<Stmt> {
        let start_span = self.peek().span;
        let id = self.alloc_ast_id();
        self.expect(&TokenKind::If)?;

        let condition = if self.check(&TokenKind::Let) {
            self.advance(); // consume 'let'
            self.parse_let_chain(start_span)?
        } else {
            self.parse_condition_with_optional_let_chain(start_span)?
        };

        let then_block = self.parse_block()?;

        let else_block = if self.check(&TokenKind::Else) {
            self.advance();
            if self.check(&TokenKind::If) {
                // `else if` - parse as nested if statement wrapped in a block
                let block_id = self.alloc_ast_id();
                let if_stmt = self.parse_if_stmt()?;
                let span = match &if_stmt {
                    Stmt::If(s) => s.span,
                    _ => unreachable!("parse_if_stmt must return Stmt::If"),
                };
                Some(Block {
                    id: block_id,
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
            id,
            condition,
            then_block,
            else_block,
            span,
        }))
    }

    /// Returns true if the token stream starting at the current position contains
    /// a `&&` followed immediately by `let` at depth 0 (outside parentheses/brackets).
    /// Used to decide whether to parse the if/while condition as a let chain.
    fn condition_has_and_let(&self) -> bool {
        let mut i = self.pos;
        let mut depth = 0usize;
        while i < self.tokens.len() {
            match &self.tokens[i].kind {
                TokenKind::LParen | TokenKind::LBracket => depth += 1,
                TokenKind::RParen | TokenKind::RBracket => depth = depth.saturating_sub(1),
                // Opening `{` at depth 0 is the then-block — stop scanning
                TokenKind::LBrace if depth == 0 => break,
                TokenKind::Eof => break,
                TokenKind::And if depth == 0 => {
                    let next = i + 1;
                    if next < self.tokens.len() && self.tokens[next].kind == TokenKind::Let {
                        return true;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        false
    }

    /// Parse an if/while condition that may or may not contain a let chain.
    /// Only enters let-chain mode when `&&` + `let` is detected ahead.
    /// Otherwise falls back to the normal expression parser.
    fn parse_condition_with_optional_let_chain(
        &mut self,
        start_span: Span,
    ) -> ParseResult<Condition> {
        if !self.condition_has_and_let() {
            // No `&& let` in the condition — parse as a regular expression
            return Ok(Condition::Expr(self.parse_expr_no_struct_literal()?));
        }

        // Condition contains `&& let` — parse as a let chain.
        // The leading elements before any `let` are bool expressions.
        let mut elements = Vec::new();

        loop {
            if self.check(&TokenKind::Let) {
                self.advance();
                let elem_span = self.peek().span;
                let pattern = self.parse_pattern()?;
                self.expect(&TokenKind::Eq)?;
                let expr = self.parse_chain_element_expr()?;
                let span = elem_span.merge(&expr.span());
                elements.push(ConditionElement::Let {
                    pattern,
                    expr,
                    span,
                });
            } else {
                elements.push(ConditionElement::Expr(self.parse_chain_element_expr()?));
            }

            if !self.check(&TokenKind::And) {
                break;
            }
            self.advance(); // consume &&
        }

        let end_span = elements
            .last()
            .map(|e| match e {
                ConditionElement::Let { span, .. } => *span,
                ConditionElement::Expr(e) => e.span(),
            })
            .unwrap_or(start_span);
        Ok(Condition::LetChain {
            elements,
            span: start_span.merge(&end_span),
        })
    }

    /// Parse a let-chain condition (after 'let' has been consumed).
    /// Handles `if let PAT = EXPR && GUARD && let PAT2 = EXPR2 { ... }`.
    /// In chain context, `&&` is always a chain separator (not logical and).
    /// To use `&&` inside an expression, wrap it in parentheses.
    fn parse_let_chain(&mut self, start_span: Span) -> ParseResult<Condition> {
        let mut elements = Vec::new();

        // Parse first let element
        let elem_span = self.peek().span;
        let pattern = self.parse_pattern()?;
        self.expect(&TokenKind::Eq)?;
        let expr = self.parse_chain_element_expr()?;
        let span = elem_span.merge(&expr.span());
        elements.push(ConditionElement::Let {
            pattern,
            expr,
            span,
        });

        // Parse subsequent chain elements separated by &&
        while self.check(&TokenKind::And) {
            self.advance(); // consume &&
            if self.check(&TokenKind::Let) {
                self.advance(); // consume 'let'
                let elem_span = self.peek().span;
                let pattern = self.parse_pattern()?;
                self.expect(&TokenKind::Eq)?;
                let expr = self.parse_chain_element_expr()?;
                let span = elem_span.merge(&expr.span());
                elements.push(ConditionElement::Let {
                    pattern,
                    expr,
                    span,
                });
            } else {
                elements.push(ConditionElement::Expr(self.parse_chain_element_expr()?));
            }
        }

        let end_span = elements
            .last()
            .map(|e| match e {
                ConditionElement::Let { span, .. } => *span,
                ConditionElement::Expr(e) => e.span(),
            })
            .unwrap_or(start_span);

        Ok(Condition::LetChain {
            elements,
            span: start_span.merge(&end_span),
        })
    }

    /// Parse a chain element expression: like `parse_or_expr` but without `&&` handling.
    /// In a let-chain, `&&` is a chain separator, not a logical-and operator.
    /// To use `&&` inside a chain expression, wrap it in parentheses.
    /// Struct literals are suppressed so that `{` is recognized as the then-block.
    fn parse_chain_element_expr(&mut self) -> ParseResult<Expr> {
        let saved = self.restrict_struct_literals;
        self.restrict_struct_literals = true;

        let mut left = self.parse_comparison_expr()?;

        while self.check(&TokenKind::Or) {
            let left_span = left.span();
            self.advance();
            let right = self.parse_comparison_expr()?;
            let merged_span = left_span.merge(&right.span());
            left = Expr::Binary(Box::new(BinaryExpr {
                id: self.alloc_ast_id(),
                left,
                op: BinaryOp::Or,
                right,
                span: merged_span,
            }));
        }

        self.restrict_struct_literals = saved;
        Ok(left)
    }

    fn parse_while_stmt(&mut self) -> ParseResult<Stmt> {
        let start_span = self.peek().span;
        let id = self.alloc_ast_id();
        self.expect(&TokenKind::While)?;

        let condition = if self.check(&TokenKind::Let) {
            self.advance(); // consume 'let'
            self.parse_let_chain(start_span)?
        } else {
            Condition::Expr(self.parse_expr_no_struct_literal()?)
        };

        let body = self.parse_block()?;
        let span = start_span.merge(&body.span);

        Ok(Stmt::While(WhileStmt {
            id,
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
        let id = self.alloc_ast_id();
        self.expect(&TokenKind::For)?;

        // Check for for-of syntax: `for let [mut] pattern of array { ... }`
        if self.check(&TokenKind::Let) {
            // Save full parser state for potential backtrack — `parse_pattern`
            // allocates AstIds and may consume comments via `alloc_ast_id`.
            let cp = self.checkpoint();

            self.advance(); // consume 'let'

            // Check for optional 'mut'
            let is_mut = self.check(&TokenKind::Mut);
            if is_mut {
                self.advance();
            }

            // Try to parse a let-pattern followed by 'of'
            let pattern = self.parse_pattern();
            if let Ok(binding) = pattern
                && matches!(self.peek().kind, TokenKind::Of)
            {
                // This is a for-of loop
                self.advance(); // consume 'of'
                let iterable = self.parse_expr_no_struct_literal()?;
                let body = self.parse_block()?;
                let span = start_span.merge(&body.span);

                return Ok(Stmt::ForOf(ForOfStmt {
                    id,
                    binding,
                    is_mut,
                    iterable,
                    body,
                    span,
                }));
            }

            // Not a for-of loop, backtrack and parse as C-style for
            self.restore(cp);
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
                id: self.alloc_ast_id(),
                expr,
                span: start_span,
            })))
        };
        self.expect(&TokenKind::Semicolon)?;

        // Parse condition (optional): `i < 10` or `let Some(x) = expr`
        let condition = if self.check(&TokenKind::Semicolon) {
            None
        } else if self.check(&TokenKind::Let) {
            let let_span = self.peek().span;
            self.advance(); // consume 'let'
            Some(self.parse_let_chain(let_span)?)
        } else {
            Some(Condition::Expr(self.parse_expr_no_struct_literal()?))
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
            Some(self.parse_expr_no_struct_literal()?)
        };

        if has_parens {
            self.expect(&TokenKind::RParen)?;
        }

        let body = self.parse_block()?;
        let span = start_span.merge(&body.span);

        Ok(Stmt::For(ForStmt {
            id,
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
        let id = self.alloc_ast_id();
        self.expect(&TokenKind::Loop)?;

        let body = self.parse_block()?;
        let span = start_span.merge(&body.span);

        Ok(Stmt::Loop(LoopStmt { id, body, span }))
    }

    /// Parse break statement: `break;`, `break label;`, or `break label: expr;`
    fn parse_break_stmt(&mut self) -> ParseResult<Stmt> {
        let start_span = self.peek().span;
        let id = self.alloc_ast_id();
        self.expect(&TokenKind::Break)?;

        // Check for optional label
        let (label, label_span, value) = if let TokenKind::Ident(name) = self.peek_kind().clone() {
            let label_tok_span = self.advance().span;
            // Check for colon followed by expression (break with value)
            if self.check(&TokenKind::Colon) {
                self.advance(); // consume ':'
                let expr = self.parse_expr()?;
                (Some(name), Some(label_tok_span), Some(Box::new(expr)))
            } else {
                // Just a label, no value
                (Some(name), Some(label_tok_span), None)
            }
        } else {
            // No label, no value
            (None, None, None)
        };

        let end_span = if self.check(&TokenKind::Semicolon) {
            self.advance().span
        } else if self.check(&TokenKind::RBrace) {
            value
                .as_ref()
                .map(|v| v.span())
                .or(label_span)
                .unwrap_or(start_span)
        } else {
            return Err(self.error_at_span(
                self.peek().span,
                &format!("expected Semicolon, found {:?}", self.peek_kind()),
            ));
        };

        Ok(Stmt::Break(BreakStmt {
            id,
            label,
            value,
            span: start_span.merge(&end_span),
        }))
    }

    /// Parse continue statement: `continue;`
    fn parse_continue_stmt(&mut self) -> ParseResult<Stmt> {
        let span = self.peek().span;
        let id = self.alloc_ast_id();
        self.expect(&TokenKind::Continue)?;

        if self.check(&TokenKind::Semicolon) {
            self.advance();
        } else if !self.check(&TokenKind::RBrace) {
            return Err(self.error_at_span(
                self.peek().span,
                &format!("expected Semicolon, found {:?}", self.peek_kind()),
            ));
        }

        Ok(Stmt::Continue(ContinueStmt { id, span }))
    }

    fn parse_pattern(&mut self) -> ParseResult<Pattern> {
        let start_span = self.peek().span;
        let first = self.parse_pattern_atom()?;

        // Check for range pattern: `literal..<literal` or `literal..=literal`
        let range_kind = match self.peek_kind() {
            TokenKind::DotDotLt => Some(RangeKind::Exclusive),
            TokenKind::DotDotEq => Some(RangeKind::Inclusive),
            _ => None,
        };
        if let Some(kind) = range_kind {
            self.advance();
            let end_span = self.peek().span;
            let end = self.parse_pattern_atom()?;
            let span = start_span.merge(&end_span);
            return Ok(Pattern::Range {
                start: Box::new(first),
                end: Box::new(end),
                kind,
                span,
            });
        }

        if self.check(&TokenKind::Pipe) {
            let mut alternatives = vec![first];
            while self.check(&TokenKind::Pipe) {
                self.advance();
                alternatives.push(self.parse_pattern_atom()?);
            }
            Ok(Pattern::Or(alternatives))
        } else {
            Ok(first)
        }
    }

    fn parse_pattern_atom(&mut self) -> ParseResult<Pattern> {
        if self.check(&TokenKind::Mut) {
            let mut_span = self.peek().span;
            self.advance();
            let ident_span = self.peek().span;
            let name = self.consume_ident()?;
            return Ok(Pattern::MutIdent {
                id: self.alloc_ast_id(),
                name,
                span: mut_span.merge(&ident_span),
            });
        }
        if self.check(&TokenKind::LParen) {
            // Tuple pattern with parentheses: (a, b, c)
            self.advance();
            let patterns = self.parse_comma_separated(&TokenKind::RParen, Self::parse_pattern)?;
            self.expect(&TokenKind::RParen)?;
            Ok(Pattern::Tuple(patterns, false))
        } else if self.check(&TokenKind::LBracket) {
            // Tuple pattern with brackets: [a, b, c] or [a, b, ..]
            self.advance();
            let mut patterns = Vec::new();
            let mut has_rest = false;
            if !self.check(&TokenKind::RBracket) {
                if self.check_dot_dot_or_ellipsis() {
                    self.consume_dot_dot()?;
                    has_rest = true;
                    // Rest-only: [..] — consume and produce empty pattern list
                } else {
                    patterns.push(self.parse_pattern()?);
                    while self.check(&TokenKind::Comma) {
                        self.advance();
                        if self.check(&TokenKind::RBracket) {
                            break;
                        }
                        if self.check_dot_dot_or_ellipsis() {
                            self.consume_dot_dot()?;
                            has_rest = true;
                            if self.check(&TokenKind::Comma) {
                                self.advance();
                            }
                            break;
                        }
                        patterns.push(self.parse_pattern()?);
                    }
                }
            }
            self.expect(&TokenKind::RBracket)?;
            Ok(Pattern::Tuple(patterns, has_rest))
        } else if self.check(&TokenKind::LBrace) {
            // Unnamed struct pattern: { x, y }
            self.parse_struct_pattern_fields(None)
        } else if let Some(name) = self.peek_kind().as_ident_name() {
            // Accept identifiers and contextual keywords (flags, type) as pattern names.
            // Case does NOT affect parsing: disambiguation between variant cases and
            // variable bindings is deferred to the resolver using type information.
            let name = name.to_string();
            let start_span = self.peek().span;
            self.advance();
            if name == "_" {
                Ok(Pattern::Wildcard)
            } else if self.check(&TokenKind::Lt) || self.check(&TokenKind::ColonColon) {
                self.parse_pattern_qualified_case_from_first_segment(name, start_span)
            } else if self.check(&TokenKind::LParen) {
                // Variant with bindings: Some(x), just(n), etc.
                let name_id = self.alloc_ast_id();
                self.parse_variant_pattern(name, None, start_span, Some(name_id), start_span)
            } else if self.check(&TokenKind::LBrace) {
                // Named struct pattern: Point { x, y }
                self.parse_struct_pattern_fields(Some(name))
            } else {
                // Bare identifier: could be a variable binding or a variant/enum case
                // without payload. The resolver disambiguates using type information.
                Ok(Pattern::Ident {
                    id: self.alloc_ast_id(),
                    name,
                    span: start_span,
                })
            }
        } else if let TokenKind::NumberLit(repr) = self.peek_kind().clone() {
            // Literal pattern: 42 or 3.14
            self.advance();
            Ok(Pattern::Literal(Literal::Number(repr)))
        } else if let TokenKind::StringLit(raw) = self.peek_kind().clone() {
            // Literal pattern: "hello"
            self.advance();
            Ok(Pattern::Literal(Literal::String(raw)))
        } else if let TokenKind::CharLit(raw) = self.peek_kind().clone() {
            // Literal pattern: 'a'
            self.advance();
            Ok(Pattern::Literal(Literal::Char(raw)))
        } else if self.check(&TokenKind::True) {
            // Literal pattern: true
            self.advance();
            Ok(Pattern::Literal(Literal::Bool(true)))
        } else if self.check(&TokenKind::False) {
            // Literal pattern: false
            self.advance();
            Ok(Pattern::Literal(Literal::Bool(false)))
        } else if self.check(&TokenKind::Null) {
            // Literal pattern: null
            self.advance();
            Ok(Pattern::Literal(Literal::Null))
        } else if self.check(&TokenKind::Minus) {
            // Negative literal pattern: -42 or -3.14
            self.advance();
            if let TokenKind::NumberLit(repr) = self.peek_kind().clone() {
                self.advance();
                Ok(Pattern::Literal(Literal::Number(format!("-{repr}"))))
            } else {
                Err(ParseError {
                    message: format!(
                        "expected numeric literal after '-', found {:?}",
                        self.peek_kind()
                    ),
                    span: self.peek().span,
                })
            }
        } else {
            Err(ParseError {
                message: format!("expected pattern, found {:?}", self.peek_kind()),
                span: self.peek().span,
            })
        }
    }

    /// Parse `{ field, field: pattern, .. }` for struct destructuring.
    /// The `{` token must be the current token.
    fn parse_struct_pattern_fields(&mut self, type_name: Option<String>) -> ParseResult<Pattern> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::LBrace)?;

        let mut fields = Vec::new();
        let mut has_rest = false;

        if !self.check(&TokenKind::RBrace) {
            // Check for leading `..`
            if self.check_dot_dot_or_ellipsis() {
                self.consume_dot_dot()?;
                has_rest = true;
            } else {
                loop {
                    // Check for `..` (rest pattern)
                    if self.check_dot_dot_or_ellipsis() {
                        self.consume_dot_dot()?;
                        has_rest = true;
                        // Trailing comma after `..` is optional
                        if self.check(&TokenKind::Comma) {
                            self.advance();
                        }
                        break;
                    }

                    let field_span = self.peek().span;
                    let field_name = if let TokenKind::StringLit(s) = self.peek_kind().clone() {
                        // Allow string literals as field names for JSON compatibility
                        self.advance();
                        s
                    } else if let Some(name) = self.peek_kind().as_ident_name() {
                        let name = name.to_string();
                        self.advance();
                        name
                    } else {
                        return Err(ParseError {
                            message: format!(
                                "expected field name in struct pattern, found {:?}",
                                self.peek_kind()
                            ),
                            span: self.peek().span,
                        });
                    };

                    // Check for `: pattern` (rename/nested)
                    let pattern = if self.check(&TokenKind::Colon) {
                        self.advance();
                        self.parse_pattern()?
                    } else if field_name == "_" {
                        Pattern::Wildcard
                    } else {
                        // Shorthand: `{ x }` means `{ x: x }`
                        Pattern::Ident {
                            id: self.alloc_ast_id(),
                            name: field_name.clone(),
                            span: field_span,
                        }
                    };

                    fields.push(StructPatternField {
                        field_name,
                        pattern,
                        span: field_span,
                    });

                    if !self.check(&TokenKind::Comma) {
                        break;
                    }
                    self.advance();
                    if self.check(&TokenKind::RBrace) {
                        break;
                    }
                }
            }
        }

        let end_span = self.peek().span;
        self.expect(&TokenKind::RBrace)?;

        Ok(Pattern::Struct {
            type_name,
            fields,
            has_rest,
            span: start_span.merge(&end_span),
        })
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
        let expr = self.parse_range_expr()?;

        // Check for simple assignment
        if self.check(&TokenKind::Eq) {
            self.advance();
            let value = self.parse_assignment_expr()?; // Right-associative
            return Ok(Expr::Assign(Box::new(AssignExpr {
                id: self.alloc_ast_id(),
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
            TokenKind::AmpEq => Some(CompoundAssignOp::BitAnd),
            TokenKind::PipeEq => Some(CompoundAssignOp::BitOr),
            TokenKind::CaretEq => Some(CompoundAssignOp::BitXor),
            TokenKind::ShlEq => Some(CompoundAssignOp::Shl),
            TokenKind::ShrEq => Some(CompoundAssignOp::Shr),
            _ => None,
        };

        if let Some(op) = compound_op {
            self.advance();
            let value = self.parse_assignment_expr()?; // Right-associative
            let value_span = value.span();

            return Ok(Expr::CompoundAssign(Box::new(CompoundAssignExpr {
                id: self.alloc_ast_id(),
                target: expr,
                op,
                value,
                span: start_span.merge(&value_span),
            })));
        }

        Ok(expr)
    }

    /// Parse range expression: `a..<b` or `a..=b`
    /// Range operators are non-associative and sit between logical OR and assignment.
    fn parse_range_expr(&mut self) -> ParseResult<Expr> {
        let left = self.parse_or_expr()?;

        let kind = match self.peek_kind() {
            TokenKind::DotDotLt => Some(RangeKind::Exclusive),
            TokenKind::DotDotEq => Some(RangeKind::Inclusive),
            _ => None,
        };

        if let Some(kind) = kind {
            let left_span = left.span();
            self.advance();
            let right = self.parse_or_expr()?;

            // Non-associative: reject `a..<b..<c`
            if matches!(self.peek_kind(), TokenKind::DotDotLt | TokenKind::DotDotEq) {
                return Err(self.error_at_span(
                    self.peek().span,
                    "range operators are non-associative; use parentheses",
                ));
            }

            let span = left_span.merge(&right.span());
            return Ok(Expr::Range(Box::new(RangeExpr {
                id: self.alloc_ast_id(),
                start: left,
                end: right,
                kind,
                span,
            })));
        }

        Ok(left)
    }

    /// Consume the current token and produce an `Expr::Literal` with the given
    /// payload. Used for one-token literals (numbers, strings, booleans, etc.).
    fn consume_literal(&mut self, value: Literal, span: Span) -> Expr {
        self.advance();
        Expr::Literal(LiteralExpr {
            id: self.alloc_ast_id(),
            value,
            span,
        })
    }

    /// Parse a left-associative binary expression: `next (op next)*`.
    /// `classify` returns `Some(op)` if the current token starts an operator at this
    /// precedence level (and that single-token operator should be consumed), or `None`.
    fn parse_left_assoc_binary(
        &mut self,
        mut next: impl FnMut(&mut Self) -> ParseResult<Expr>,
        classify: impl Fn(&TokenKind) -> Option<BinaryOp>,
    ) -> ParseResult<Expr> {
        let mut left = next(self)?;
        while let Some(op) = classify(self.peek_kind()) {
            let left_span = left.span();
            self.advance();
            let right = next(self)?;
            let span = left_span.merge(&right.span());
            left = Expr::Binary(Box::new(BinaryExpr {
                id: self.alloc_ast_id(),
                left,
                op,
                right,
                span,
            }));
        }
        Ok(left)
    }

    fn parse_or_expr(&mut self) -> ParseResult<Expr> {
        self.parse_left_assoc_binary(Self::parse_and_expr, |k| match k {
            TokenKind::Or => Some(BinaryOp::Or),
            _ => None,
        })
    }

    fn parse_and_expr(&mut self) -> ParseResult<Expr> {
        self.parse_left_assoc_binary(Self::parse_comparison_expr, |k| match k {
            TokenKind::And => Some(BinaryOp::And),
            _ => None,
        })
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
                id: self.alloc_ast_id(),
                left: first,
                op: cmp.op,
                right: cmp.right,
                span: merged_span,
            })));
        }

        // Multiple comparisons: return a ComparisonChainExpr
        let full_span = first_span.merge(&current.span());
        Ok(Expr::ComparisonChain(Box::new(ComparisonChainExpr {
            id: self.alloc_ast_id(),
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
        self.parse_left_assoc_binary(Self::parse_bitxor_expr, |k| match k {
            TokenKind::Pipe => Some(BinaryOp::BitOr),
            _ => None,
        })
    }

    fn parse_bitxor_expr(&mut self) -> ParseResult<Expr> {
        self.parse_left_assoc_binary(Self::parse_bitand_expr, |k| match k {
            TokenKind::Caret => Some(BinaryOp::BitXor),
            _ => None,
        })
    }

    fn parse_bitand_expr(&mut self) -> ParseResult<Expr> {
        self.parse_left_assoc_binary(Self::parse_shift_expr, |k| match k {
            TokenKind::Ampersand => Some(BinaryOp::BitAnd),
            _ => None,
        })
    }

    fn parse_shift_expr(&mut self) -> ParseResult<Expr> {
        self.parse_left_assoc_binary(Self::parse_additive_expr, |k| match k {
            TokenKind::LtLt => Some(BinaryOp::Shl),
            TokenKind::GtGt => Some(BinaryOp::Shr),
            _ => None,
        })
    }

    fn parse_additive_expr(&mut self) -> ParseResult<Expr> {
        self.parse_left_assoc_binary(Self::parse_multiplicative_expr, |k| match k {
            TokenKind::Plus => Some(BinaryOp::Add),
            TokenKind::Minus => Some(BinaryOp::Sub),
            _ => None,
        })
    }

    fn parse_cast_expr(&mut self) -> ParseResult<Expr> {
        let mut expr = self.parse_unary_expr()?;
        while *self.peek_kind() == TokenKind::As {
            let start_span = self.peek().span;
            self.advance();
            let target_type = self.parse_type()?;
            expr = Expr::Cast(Box::new(CastExpr {
                id: self.alloc_ast_id(),
                expr,
                target_type,
                span: start_span,
            }));
        }
        Ok(expr)
    }

    fn parse_multiplicative_expr(&mut self) -> ParseResult<Expr> {
        self.parse_left_assoc_binary(Self::parse_cast_expr, |k| match k {
            TokenKind::Star => Some(BinaryOp::Mul),
            TokenKind::Slash => Some(BinaryOp::Div),
            TokenKind::Percent => Some(BinaryOp::Mod),
            _ => None,
        })
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
            let id = self.alloc_ast_id();
            return Ok(Expr::Unary(Box::new(UnaryExpr { id, op, expr, span })));
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
            let id = self.alloc_ast_id();
            return Ok(Expr::Unary(Box::new(UnaryExpr { id, op, expr, span })));
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
                    // Inside function call arguments, struct literals are allowed
                    let saved = self.restrict_struct_literals;
                    self.restrict_struct_literals = false;
                    let (args, has_trailing_comma) = self.parse_arg_list()?;
                    self.restrict_struct_literals = saved;
                    let rparen_span = self.peek().span;
                    self.expect(&TokenKind::RParen)?;
                    let merged_span = callee_span.merge(&rparen_span);
                    expr = Expr::Call(Box::new(CallExpr {
                        id: self.alloc_ast_id(),
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
                        let saved = self.restrict_struct_literals;
                        self.restrict_struct_literals = false;
                        let (args, has_trailing_comma) = self.parse_arg_list()?;
                        self.restrict_struct_literals = saved;
                        let rparen_span = self.peek().span;
                        self.expect(&TokenKind::RParen)?;
                        let merged_span = callee_span.merge(&rparen_span);
                        expr = Expr::Call(Box::new(CallExpr {
                            id: self.alloc_ast_id(),
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

                    // Support identifier and number literal for field access
                    // Integer literals are used for tuple field access: t.0, t.1, etc.
                    // Number literals like "0.0" after a dot are split into two field accesses.
                    let (field, second_field) = if let TokenKind::NumberLit(s) = &self.peek().kind {
                        // Check if it's a simple integer or contains a dot
                        if s.contains('.') {
                            // Handle cases like `t.0.0` where the lexer tokenizes "0.0" as a number
                            // Split the number literal into two field indices
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
                                    message: format!("expected field name, found NumberLit({s:?})"),
                                    span: field_span,
                                });
                            }
                        } else {
                            // Simple integer literal for tuple field access
                            let field_name = s.clone();
                            self.advance();
                            (field_name, None)
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
                                id: self.alloc_ast_id(),
                                receiver: expr,
                                method: field,
                                method_id: self.alloc_ast_id(),
                                method_span: field_span,
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
                            id: self.alloc_ast_id(),
                            receiver: expr,
                            method: field,
                            method_id: self.alloc_ast_id(),
                            method_span: field_span,
                            type_args: vec![],
                            args,
                            has_trailing_comma,
                            span: merged_span,
                        }));
                    } else {
                        let merged_span = receiver_span.merge(&field_span);
                        expr = Expr::FieldAccess(Box::new(FieldAccessExpr {
                            id: self.alloc_ast_id(),
                            expr,
                            field,
                            field_id: self.alloc_ast_id(),
                            field_span,
                            span: merged_span,
                        }));

                        // If we parsed a float literal as two fields (e.g., "0.0" -> "0", "0"),
                        // add the second field access
                        if let Some(second) = second_field {
                            let second_span = expr.span();
                            expr = Expr::FieldAccess(Box::new(FieldAccessExpr {
                                id: self.alloc_ast_id(),
                                expr,
                                field: second,
                                field_id: self.alloc_ast_id(),
                                field_span: second_span,
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
                        id: self.alloc_ast_id(),
                        expr,
                        index,
                        span: merged_span,
                    }));
                }
                TokenKind::Matches => {
                    expr = self.parse_matches_expr(expr)?;
                }
                TokenKind::Question => {
                    let start_span = expr.span();
                    let question_token = self.advance();
                    let span = start_span.merge(&question_token.span);
                    let id = self.alloc_ast_id();
                    expr = Expr::TryOp(Box::new(TryOpExpr { id, expr, span }));
                }
                _ => break,
            }
        }

        Ok(expr)
    }

    /// Speculatively parse `Type::<Args>::method(...)` after `name::` was already
    /// consumed and `<` is the next token. On any speculative failure, restore the
    /// parser to `cp` (just before the `::`) and produce a bare `Ident`.
    fn parse_generic_static_method_call_or_backtrack(
        &mut self,
        start_span: Span,
        name: String,
        cp: ParserCheckpoint,
    ) -> ParseResult<Expr> {
        self.advance(); // consume <

        let mut type_args = Vec::new();
        let mut spec_ok = true;

        match self.parse_type() {
            Ok(t) => type_args.push(t),
            Err(_) => spec_ok = false,
        }
        while spec_ok && self.check(&TokenKind::Comma) {
            self.advance();
            match self.parse_type() {
                Ok(t) => type_args.push(t),
                Err(_) => spec_ok = false,
            }
        }
        if spec_ok {
            spec_ok = self.expect_gt().is_ok();
        }

        if spec_ok && self.check(&TokenKind::ColonColon) {
            self.advance(); // consume ::
            let (method, method_span) = self.consume_ident_with_span()?;

            // Method-level turbofish: method::<U>(...)
            let method_type_args =
                if self.check(&TokenKind::ColonColon) && self.peek_nth(1).kind == TokenKind::Lt {
                    self.advance(); // consume ::
                    self.parse_call_type_args()?
                } else {
                    Vec::new()
                };

            self.expect(&TokenKind::LParen)?;
            let (args, has_trailing_comma) = self.parse_arg_list()?;
            let end_span = self.expect(&TokenKind::RParen)?.span;

            return Ok(Expr::StaticMethodCall(Box::new(StaticMethodCallExpr {
                id: self.alloc_ast_id(),
                target_type: Type::Generic(GenericType {
                    id: self.alloc_ast_id(),
                    name,
                    args: type_args,
                    span: start_span,
                }),
                method,
                method_id: self.alloc_ast_id(),
                method_span,
                type_args: method_type_args,
                args,
                has_trailing_comma,
                span: start_span.merge(&end_span),
            })));
        }

        self.restore(cp);
        Ok(Expr::Ident(IdentExpr {
            id: self.alloc_ast_id(),
            name,
            span: start_span,
            segments: Vec::new(),
        }))
    }

    /// Parse a `name::seg::seg...` qualified path identifier. The leading `name`
    /// has already been consumed and `::` has just been consumed by the caller,
    /// so the parser is positioned at the first segment after that `::`.
    fn parse_qualified_path(&mut self, start_span: Span, name: String) -> ParseResult<Expr> {
        let mut segments = vec![PathSegment {
            id: self.alloc_ast_id(),
            name: name.clone(),
            span: start_span,
        }];
        let first_seg_span = self.peek().span;
        let first_seg_name = self.consume_ident()?;
        segments.push(PathSegment {
            id: self.alloc_ast_id(),
            name: first_seg_name.clone(),
            span: first_seg_span,
        });
        let mut qualified_name = format!("{name}::{first_seg_name}");
        let mut end_span = first_seg_span;
        while self.check(&TokenKind::ColonColon)
            && matches!(self.peek_nth(1).kind, TokenKind::Ident(_))
        {
            self.advance(); // consume ::
            let seg_span = self.peek().span;
            let seg_name = self.consume_ident()?;
            segments.push(PathSegment {
                id: self.alloc_ast_id(),
                name: seg_name.clone(),
                span: seg_span,
            });
            qualified_name = format!("{qualified_name}::{seg_name}");
            end_span = seg_span;
        }
        Ok(Expr::Ident(IdentExpr {
            id: self.alloc_ast_id(),
            name: qualified_name,
            span: start_span.merge(&end_span),
            segments,
        }))
    }

    fn parse_primary_expr(&mut self) -> ParseResult<Expr> {
        let start_span = self.peek().span;

        // Handle identifiers and contextual keywords (flags, type)
        if let Some(name) = self.peek_kind().as_ident_name() {
            // Contextual keyword `resume` in expression position. Only matched
            // here; outside of expressions (e.g. `let resume = ...`), `resume`
            // remains an ordinary identifier.
            if name == "resume" {
                return self.parse_resume_expr();
            }
            let name = name.to_string();
            self.advance();
            // Check for qualified name (Effect::function) or static method call
            if self.check(&TokenKind::ColonColon) {
                let cp = self.checkpoint();
                self.advance(); // consume ::

                if self.check(&TokenKind::Lt) {
                    return self
                        .parse_generic_static_method_call_or_backtrack(start_span, name, cp);
                }
                return self.parse_qualified_path(start_span, name);
            } else if self.check(&TokenKind::Colon) && self.peek_nth(1).kind == TokenKind::LBrace {
                // Labeled block expression: `label: { ... }`
                self.advance(); // consume ':'
                let block = self.parse_block()?;
                let end_span = block.span;
                return Ok(Expr::LabeledBlock(Box::new(crate::ast::LabeledBlockExpr {
                    id: self.alloc_ast_id(),
                    label: name,
                    block,
                    span: start_span.merge(&end_span),
                })));
            } else if self.check(&TokenKind::LBrace)
                && !self.restrict_struct_literals
                && self.looks_like_struct_literal_content()
            {
                // Struct literal: `Name { field: value, ... }`
                // Detected by looking at content inside braces, not naming convention.
                // Restricted in contexts where a block follows the expression
                // (e.g., if/while/match conditions) to avoid ambiguity.
                return self.parse_struct_literal(Some(name), start_span);
            }
            return Ok(Expr::Ident(IdentExpr {
                id: self.alloc_ast_id(),
                name,
                segments: Vec::new(),
                span: start_span,
            }));
        }

        match self.peek_kind().clone() {
            TokenKind::NumberLit(repr) => {
                Ok(self.consume_literal(Literal::Number(repr), start_span))
            }
            TokenKind::StringLit(raw) => Ok(self.consume_literal(Literal::String(raw), start_span)),
            TokenKind::TemplateStringLit(parts) => {
                self.advance();
                self.parse_template_string_parts(parts, start_span)
            }
            TokenKind::True => Ok(self.consume_literal(Literal::Bool(true), start_span)),
            TokenKind::False => Ok(self.consume_literal(Literal::Bool(false), start_span)),
            TokenKind::Null => Ok(self.consume_literal(Literal::Null, start_span)),
            TokenKind::CharLit(raw) => Ok(self.consume_literal(Literal::Char(raw), start_span)),
            TokenKind::LParen => self.parse_paren_or_unit_expr(start_span),
            TokenKind::LBracket => {
                self.advance();
                // Inside brackets, struct literals are always allowed
                let saved = self.restrict_struct_literals;
                self.restrict_struct_literals = false;
                let result = self.parse_tuple_literal(start_span);
                self.restrict_struct_literals = saved;
                result
            }
            TokenKind::Pipe => self.parse_closure(),
            TokenKind::Or => self.parse_zero_arg_closure_expr(start_span),
            TokenKind::LBrace => self.parse_implicit_struct_literal_expr(start_span),
            TokenKind::If => self.parse_if_expr(),
            TokenKind::Match => self.parse_match_expr(),
            TokenKind::With => self.parse_with_handler_expr(),
            TokenKind::Hash => self.parse_compile_time_literal_expr(start_span),
            TokenKind::Matches => Err(ParseError {
                message:
                    "'matches' is an infix operator — write 'expr matches { pattern }' instead"
                        .to_owned(),
                span: start_span,
            }),
            _ => Err(ParseError {
                message: format!("expected expression, found {:?}", self.peek_kind()),
                span: start_span,
            }),
        }
    }

    /// Parse `(expr)` or the unit literal `()`. The leading `(` has not been consumed.
    fn parse_paren_or_unit_expr(&mut self, start_span: Span) -> ParseResult<Expr> {
        self.advance(); // consume `(`
        if self.check(&TokenKind::RParen) {
            self.advance();
            return Ok(Expr::Literal(LiteralExpr {
                id: self.alloc_ast_id(),
                value: Literal::Unit,
                span: start_span,
            }));
        }
        // Struct literals are always allowed inside parentheses.
        let saved = self.restrict_struct_literals;
        self.restrict_struct_literals = false;
        let expr = self.parse_expr()?;
        self.restrict_struct_literals = saved;
        let end_span = self.expect(&TokenKind::RParen)?.span;
        Ok(expr.with_span(start_span.merge(&end_span)))
    }

    /// Parse `|| body` — a zero-parameter closure. `||` (logical-or token) has
    /// not been consumed; it is only a closure here because primary position has
    /// no left operand for the binary operator.
    fn parse_zero_arg_closure_expr(&mut self, start_span: Span) -> ParseResult<Expr> {
        self.advance(); // consume `||`
        let body = if self.check(&TokenKind::LBrace) {
            let block = self.parse_block()?;
            Expr::Block(Box::new(block))
        } else {
            self.parse_expr()?
        };
        let body_span = body.span();
        Ok(Expr::Closure(Box::new(ClosureExpr {
            id: self.alloc_ast_id(),
            params: vec![],
            body,
            span: start_span.merge(&body_span),
        })))
    }

    /// Parse an implicit (untyped) struct literal `{ field: value, ... }`.
    /// The leading `{` has not been consumed; this method also rejects braces
    /// that don't look like a struct literal.
    fn parse_implicit_struct_literal_expr(&mut self, start_span: Span) -> ParseResult<Expr> {
        self.advance(); // consume `{`

        // `{ ident :` / `{ ident ,` / `{ ident }`
        if let TokenKind::Ident(_) = self.peek_kind()
            && matches!(
                self.peek_nth(1).kind,
                TokenKind::Colon | TokenKind::Comma | TokenKind::RBrace
            )
        {
            return self.parse_struct_literal(None, start_span);
        }

        // `{ "field" :` — string literal field name
        if let TokenKind::StringLit(_) = self.peek_kind()
            && matches!(self.peek_nth(1).kind, TokenKind::Colon)
        {
            return self.parse_struct_literal(None, start_span);
        }

        // `{ true: ... }` — keyword used as field name; route through struct
        // literal parser to produce a clearer error.
        if matches!(
            self.peek_kind(),
            TokenKind::True | TokenKind::False | TokenKind::Null
        ) && matches!(self.peek_nth(1).kind, TokenKind::Colon)
        {
            return self.parse_struct_literal(None, start_span);
        }

        // Empty struct literal `{}`.
        if self.check(&TokenKind::RBrace) {
            return self.parse_struct_literal(None, start_span);
        }

        Err(ParseError {
            message: "implicit struct literal requires field syntax: { field: value }".into(),
            span: start_span,
        })
    }

    /// Parse a `#name` compile-time literal: `#file`, `#line`, `#function`,
    /// `#data`, `#include_str("...")`, `#include_bytes("...")`. The leading `#`
    /// has not been consumed.
    fn parse_compile_time_literal_expr(&mut self, start_span: Span) -> ParseResult<Expr> {
        self.advance(); // consume `#`
        let TokenKind::Ident(raw_name) = self.peek_kind() else {
            return Err(ParseError {
                message: "expected identifier after `#` for compile-time literal".into(),
                span: start_span,
            });
        };
        let name = raw_name.clone();
        let name_span = self.advance().span;

        if name == "include_str" || name == "include_bytes" {
            let is_str = name == "include_str";
            self.expect(&TokenKind::LParen)?;
            let path = match self.peek_kind() {
                TokenKind::StringLit(s) => {
                    let s = s.clone();
                    self.advance();
                    s
                }
                _ => {
                    return Err(ParseError {
                        message: format!("expected string literal path argument for `#{name}`"),
                        span: self.peek().span,
                    });
                }
            };
            let close_span = self.expect(&TokenKind::RParen)?.span;
            self.include_paths.insert(path.clone());
            let value = if is_str {
                Literal::IncludeStr(path)
            } else {
                Literal::IncludeBytes(path)
            };
            return Ok(Expr::Literal(LiteralExpr {
                id: self.alloc_ast_id(),
                value,
                span: start_span.merge(&close_span),
            }));
        }

        let value = match name.as_str() {
            "file" => Literal::LocationFile,
            "line" => Literal::LocationLine,
            "function" => Literal::LocationFunction,
            "data" => Literal::DataSection,
            _ => {
                return Err(ParseError {
                    message: format!(
                        "unknown compile-time literal `#{name}`, expected `#file`, `#line`, `#function`, `#data`, `#include_str`, or `#include_bytes`"
                    ),
                    span: start_span.merge(&name_span),
                });
            }
        };
        Ok(Expr::Literal(LiteralExpr {
            id: self.alloc_ast_id(),
            value,
            span: start_span.merge(&name_span),
        }))
    }

    /// Parse if expression: `if condition { expr } else { expr }`
    fn parse_if_expr(&mut self) -> ParseResult<Expr> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::If)?;

        let condition = if self.check(&TokenKind::Let) {
            self.advance(); // consume 'let'
            self.parse_let_chain(start_span)?
        } else {
            self.parse_condition_with_optional_let_chain(start_span)?
        };

        let then_block = self.parse_block()?;

        let else_block = if self.check(&TokenKind::Else) {
            self.advance();
            if self.check(&TokenKind::If) {
                // `else if` - parse as nested if expression wrapped in a block
                let block_id = self.alloc_ast_id();
                let if_expr = self.parse_if_expr()?;
                let span = if_expr.span();
                Some(Block {
                    id: block_id,
                    stmts: vec![Stmt::Expr(ExprStmt {
                        id: self.alloc_ast_id(),
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
            id: self.alloc_ast_id(),
            condition,
            then_block,
            else_block,
            span,
        })))
    }

    /// Parse an effect handler installation expression:
    /// `with E1 => h1, E2 => h2 do { body }` or `with &mut h do { body }` (bundled).
    ///
    /// Each binding is one of:
    /// - `EffectName => expr` — install `expr` as the handler for `EffectName`.
    ///   The `=>` reads as "case E is dispatched to expr", mirroring match arms;
    ///   this is not an assignment.
    /// - `expr` (no `=>`) — bundled handler, used for every effect `expr` implements.
    ///
    /// See `docs/wep-2026-04-11-effect-handler.md`.
    fn parse_with_handler_expr(&mut self) -> ParseResult<Expr> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::With)?;
        let id = self.alloc_ast_id();

        let mut handlers = Vec::new();
        loop {
            handlers.push(self.parse_effect_handler_binding()?);
            if !self.check(&TokenKind::Comma) {
                break;
            }
            self.advance(); // consume `,`
        }

        // `do` is a contextual keyword: lex returns it as an Ident.
        let is_do = matches!(
            self.peek_kind(),
            TokenKind::Ident(name) if name == "do"
        );
        if !is_do {
            return Err(self.error_at_span(
                self.peek().span,
                &format!(
                    "expected `do` after handler bindings, found {:?}",
                    self.peek_kind()
                ),
            ));
        }
        self.advance();

        let body = self.parse_block()?;
        let span = start_span.merge(&body.span);

        Ok(Expr::WithHandler(Box::new(crate::ast::WithHandlerExpr {
            id,
            handlers,
            body,
            span,
        })))
    }

    /// Parse one binding inside a `with ... do` clause. Two shapes:
    /// - `Effect => handler_expr` — explicit effect on the LHS. The effect is
    ///   any type expression, so `Stdout => ...` and `Stream<u8> => ...` both
    ///   parse here. Later compiler phases decide which forms they support.
    /// - `handler_expr` (no `=>`) — bundled handler.
    fn parse_effect_handler_binding(&mut self) -> ParseResult<crate::ast::EffectHandlerBinding> {
        let id = self.alloc_ast_id();
        let start_span = self.peek().span;

        // Speculatively try the explicit form. The LHS is a type, so we have
        // to commit to type parsing only if a `=>` follows; otherwise this is
        // a bundled handler whose expression starts with an identifier.
        if matches!(self.peek_kind(), TokenKind::Ident(_)) {
            let cp = self.checkpoint();
            if let Ok(ty) = self.parse_type() {
                if self.check(&TokenKind::FatArrow) {
                    self.advance(); // consume `=>`
                    // Handler expressions are parsed as unary expressions so
                    // we don't greedily consume the trailing `,` or `do`.
                    // Trade-off: cast / `if` / `match` expressions can't sit
                    // directly in handler position; wrap them in `()`.
                    let handler = self.parse_unary_expr()?;
                    let span = start_span.merge(&handler.span());
                    return Ok(crate::ast::EffectHandlerBinding {
                        id,
                        effect: Some(ty),
                        handler,
                        span,
                    });
                }
                // Looked like a type but no `=` followed — fall back to
                // bundled-handler parsing of the whole expression.
                self.restore(cp);
            } else {
                // parse_type rejected the prefix; treat as a bundled handler.
                self.restore(cp);
            }
        }

        // Bundled handler form: `with &mut value do { ... }`
        let handler = self.parse_unary_expr()?;
        let span = start_span.merge(&handler.span());
        Ok(crate::ast::EffectHandlerBinding {
            id,
            effect: None,
            handler,
            span,
        })
    }

    /// Parse a `resume value` expression. Valid only inside an effect handler
    /// method body; the resolver (later phase) is responsible for that check.
    /// `resume` is a contextual keyword: the lexer hands it to us as an Ident,
    /// so we consume it by name rather than via a dedicated `TokenKind`.
    fn parse_resume_expr(&mut self) -> ParseResult<Expr> {
        let start_span = self.peek().span;
        self.advance(); // consume `resume` ident
        let id = self.alloc_ast_id();
        let value = self.parse_expr()?;
        let span = start_span.merge(&value.span());
        Ok(Expr::Resume(Box::new(crate::ast::ResumeExpr {
            id,
            value,
            span,
        })))
    }

    /// Parse match expression: `match expr { pattern => body, ... }`
    fn parse_match_expr(&mut self) -> ParseResult<Expr> {
        let start_span = self.peek().span;
        let id = self.alloc_ast_id();
        self.expect(&TokenKind::Match)?;

        // Parse scrutinee expression (no struct literals - ambiguous with match body braces)
        let scrutinee = self.parse_expr_no_struct_literal()?;

        // Expect opening brace
        self.expect(&TokenKind::LBrace)?;

        // Parse match arms
        let mut arms = Vec::new();
        while !self.check(&TokenKind::RBrace) {
            let arm = self.parse_match_arm()?;
            arms.push(arm);

            // Trailing comma is optional
            if self.check(&TokenKind::Comma) {
                self.advance();
            }
        }

        let end_span = self.expect(&TokenKind::RBrace)?.span;

        Ok(Expr::Match(Box::new(MatchExpr {
            id,
            expr: scrutinee,
            arms,
            span: start_span.merge(&end_span),
        })))
    }

    /// Parse a single match arm: `pattern [&& guard] => body`
    fn parse_match_arm(&mut self) -> ParseResult<MatchArm> {
        let start_span = self.peek().span;
        // Allocate the arm's id before any child node, so leading
        // comments preceding the arm are pinned to `arm.id` rather
        // than leaking into the pattern's first descendant id.
        let id = self.alloc_ast_id();

        // Parse pattern
        let pattern = self.parse_pattern()?;

        // Check for optional guard: `&& guard_expr`
        let guard = if self.check(&TokenKind::And) {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };

        // Expect =>
        self.expect(&TokenKind::FatArrow)?;

        // Parse arm body - either a block, a return statement, or an expression
        let body = if self.check(&TokenKind::LBrace) {
            // Block body: `{ ... }`
            let block = self.parse_block()?;
            Expr::Block(Box::new(block))
        } else if self.check(&TokenKind::Return) {
            // Return in match arm: `return expr` — wrap in a synthetic block.
            // The comma/closing brace terminates the arm; no semicolon needed.
            let ret_start = self.peek().span;
            self.advance(); // consume 'return'
            let value = if self.check(&TokenKind::Comma) || self.check(&TokenKind::RBrace) {
                None
            } else {
                Some(self.parse_expr()?)
            };
            let ret_end = value.as_ref().map_or(ret_start, super::ast::Expr::span);
            let ret_span = ret_start.merge(&ret_end);
            let ret_stmt = Stmt::Return(ReturnStmt {
                id: self.alloc_ast_id(),
                value,
                span: ret_span,
            });
            let block_id = self.alloc_ast_id();
            Expr::Block(Box::new(Block {
                id: block_id,
                stmts: vec![ret_stmt],
                span: ret_span,
            }))
        } else {
            // Expression body: `expr`
            self.parse_expr()?
        };

        let end_span = body.span();
        Ok(MatchArm {
            id,
            pattern,
            guard,
            body,
            span: start_span.merge(&end_span),
        })
    }

    /// Parse matches expression: `expr matches { pattern [&& guard] }`
    /// This is an infix operator that returns a boolean.
    fn parse_matches_expr(&mut self, expr: Expr) -> ParseResult<Expr> {
        let start_span = expr.span();
        self.expect(&TokenKind::Matches)?;

        // Expect opening brace
        self.expect(&TokenKind::LBrace)?;

        // Parse pattern
        let pattern = self.parse_pattern()?;

        // Check for optional guard: `&& guard_expr`
        let guard = if self.check(&TokenKind::And) {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };

        // Expect closing brace
        let end_span = self.expect(&TokenKind::RBrace)?.span;

        Ok(Expr::Matches(Box::new(MatchesExpr {
            id: self.alloc_ast_id(),
            expr,
            pattern,
            guard,
            span: start_span.merge(&end_span),
        })))
    }

    /// Parse tuple literal: `[expr, expr, ...]` or `[]`
    fn parse_tuple_literal(&mut self, start_span: Span) -> ParseResult<Expr> {
        let elements =
            self.parse_comma_separated(&TokenKind::RBracket, Self::parse_tuple_element)?;

        let end_token = self.expect(&TokenKind::RBracket)?;
        let end_span = end_token.span;

        Ok(Expr::TupleLiteral(Box::new(TupleLiteralExpr {
            id: self.alloc_ast_id(),
            elements,
            span: start_span.merge(&end_span),
        })))
    }

    /// Parse a tuple element: either a spread `..expr` or a regular expression.
    fn parse_tuple_element(&mut self) -> ParseResult<Expr> {
        if self.check_dot_dot_or_ellipsis() {
            let span = self.consume_dot_dot()?;
            let expr = self.parse_expr()?;
            return Ok(Expr::Spread(Box::new(expr), span));
        }
        self.parse_expr()
    }

    /// Parse argument list. Returns (args, `has_trailing_comma`).
    fn parse_arg_list(&mut self) -> ParseResult<(Vec<Expr>, bool)> {
        let pos_before = self.pos;
        let args = self.parse_comma_separated(&TokenKind::RParen, Self::parse_expr)?;
        // Detect trailing comma: pos moved past at least one comma, and we're at RParen
        let has_trailing_comma = !args.is_empty()
            && self.check(&TokenKind::RParen)
            && self.tokens[self.pos - 1].kind == TokenKind::Comma;
        let _ = pos_before;
        Ok((args, has_trailing_comma))
    }

    fn parse_closure(&mut self) -> ParseResult<Expr> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Pipe)?;

        let params = self.parse_comma_separated(&TokenKind::Pipe, Self::parse_closure_param)?;

        self.expect(&TokenKind::Pipe)?;

        // Check for block body: |params| { ... }
        let body = if self.check(&TokenKind::LBrace) {
            let block = self.parse_block()?;
            Expr::Block(Box::new(block))
        } else {
            self.parse_expr()?
        };

        let body_span = body.span();
        Ok(Expr::Closure(Box::new(ClosureExpr {
            id: self.alloc_ast_id(),
            params,
            body,
            span: start_span.merge(&body_span),
        })))
    }

    fn parse_closure_param(&mut self) -> ParseResult<ClosureParam> {
        let id = self.alloc_ast_id();
        let is_mut = if self.check(&TokenKind::Mut) {
            self.advance();
            true
        } else {
            false
        };
        let (name, name_span) = self.consume_ident_with_span()?;
        let ty = if self.check(&TokenKind::Colon) {
            self.advance();
            Some(self.parse_type()?)
        } else {
            None
        };
        let default = if self.check(&TokenKind::Eq) {
            self.advance();
            // Stop at `|` so the closing pipe terminates the default, matching
            // the expectation that trivial defaults don't include bitwise-or.
            // Wrap complex defaults in parentheses when bit-or is needed.
            Some(self.parse_bitxor_expr()?)
        } else {
            None
        };
        Ok(ClosureParam {
            id,
            name,
            name_span,
            ty,
            is_mut,
            default,
        })
    }

    /// Parse a type or a type pack spread (`..T`) inside a tuple type.
    fn parse_type_or_pack(&mut self) -> ParseResult<Type> {
        if self.check_dot_dot_or_ellipsis() {
            let span = self.consume_dot_dot()?;
            let name = self.consume_ident()?;
            return Ok(Type::TypePackSpread(name, span));
        }
        self.parse_type()
    }

    fn parse_type(&mut self) -> ParseResult<Type> {
        let start_span = self.peek().span;

        // Never type: !
        if self.check(&TokenKind::Not) {
            self.advance();
            let id = self.alloc_ast_id();
            return Ok(Type::Named(NamedType {
                id,
                name: "!".to_string(),
                span: start_span,
                source_interface: None,
            }));
        }

        // Function type: fn(T1, T2) -> R
        if self.check(&TokenKind::Fn) {
            self.advance();
            self.expect(&TokenKind::LParen)?;

            // Parse parameter types
            let params = self.parse_comma_separated(&TokenKind::RParen, Self::parse_type)?;
            self.expect(&TokenKind::RParen)?;

            // Parse return type (optional)
            let return_type = if self.check(&TokenKind::Arrow) {
                self.advance();
                self.parse_type()?
            } else {
                Type::Named(NamedType {
                    id: self.alloc_ast_id(),
                    name: "()".to_string(),
                    span: start_span,
                    source_interface: None,
                })
            };

            // Parse effects and stores (optional): with Effect1, stores[0]
            let (effects, effect_ids, stores) = self.parse_with_clause_for_fn_type()?;

            return Ok(Type::Function(Box::new(FunctionType {
                params,
                return_type,
                effects,
                effect_ids,
                stores,
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
                let id = self.alloc_ast_id();
                return Ok(Type::Named(NamedType {
                    id,
                    name: "()".to_string(),
                    span: start_span,
                    source_interface: None,
                }));
            }
            // Parenthesized type for grouping (not tuple in this case)
            let inner = self.parse_type()?;
            self.expect(&TokenKind::RParen)?;
            return Ok(inner);
        }

        // Tuple type: [] or [T1, T2, ...] or [i32, ..T, bool]
        if self.check(&TokenKind::LBracket) {
            self.advance();
            let types =
                self.parse_comma_separated(&TokenKind::RBracket, Self::parse_type_or_pack)?;
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
                let args = self.parse_type_args()?;

                return Ok(Type::NamespacedGeneric(NamespacedGenericType {
                    id: self.alloc_ast_id(),
                    namespace: name,
                    name: type_name,
                    args,
                    span: start_span,
                }));
            } else {
                // Namespaced type without generics: namespace::type
                return Ok(Type::NamespacedGeneric(NamespacedGenericType {
                    id: self.alloc_ast_id(),
                    namespace: name,
                    name: type_name,
                    args: Vec::new(),
                    span: start_span,
                }));
            }
        }

        if self.check(&TokenKind::Lt) {
            self.advance();
            let args = self.parse_type_args()?;

            Ok(Type::Generic(GenericType {
                id: self.alloc_ast_id(),
                name,
                args,
                span: start_span,
            }))
        } else {
            Ok(Type::Named(NamedType {
                id: self.alloc_ast_id(),
                name,
                span: start_span,
                source_interface: None,
            }))
        }
    }

    // Placeholder implementations for other declarations

    fn parse_interface_decl(
        &mut self,
        is_pub: bool,
        attrs: Vec<Attribute>,
    ) -> ParseResult<InterfaceDecl> {
        let id = self.alloc_ast_id();
        let start_span = self.peek().span;
        self.expect(&TokenKind::Interface)?;
        let (name, name_span) = self.consume_ident_with_span()?;
        self.expect(&TokenKind::LBrace)?;

        let mut methods = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            methods.push(self.parse_interface_method()?);
        }

        let end_span = self.expect(&TokenKind::RBrace)?.span;

        Ok(InterfaceDecl {
            id,
            name,
            name_span,
            is_pub,
            attrs,
            methods,
            span: start_span.merge(&end_span),
        })
    }

    fn parse_interface_method(&mut self) -> ParseResult<InterfaceMethod> {
        // Parse any attributes on the method (e.g., #[cm("...")])
        let attrs = self.parse_attributes()?;

        let id = self.alloc_ast_id();
        let start_span = self.peek().span;

        // Check for async keyword
        let is_async = if self.check(&TokenKind::Async) {
            self.advance();
            true
        } else {
            false
        };

        self.expect(&TokenKind::Fn)?;
        let (name, name_span) = self.consume_ident_with_span()?;

        let _type_params = self.parse_generic_params()?;

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

        Ok(InterfaceMethod {
            id,
            name,
            name_span,
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

        while !self.pending_gt && !self.check(&TokenKind::Gt) && !self.is_at_end() {
            let start_span = self.peek().span;

            // Parse effect parameter: `effect E`
            let is_effect = if self.check(&TokenKind::Effect) {
                self.advance();
                true
            } else {
                false
            };

            // Parse type pack parameter: `..T`
            let is_pack = if self.check_dot_dot_or_ellipsis() {
                self.consume_dot_dot()?;
                true
            } else {
                false
            };

            if is_effect && is_pack {
                return Err(self.error_at_span(
                    start_span,
                    "a parameter cannot be both an effect and a type pack",
                ));
            }

            if is_pack && params.iter().any(|p: &crate::ast::GenericParam| p.is_pack) {
                return Err(self.error_at_span(
                    start_span,
                    "only one type pack parameter is allowed per generic parameter list",
                ));
            }

            let (name, name_span) = self.consume_ident_with_span()?;

            // Parse optional trait bounds: `T: Ord`, `T: Ord + Clone`, `T: Builder<Output = T>`
            let bounds = if self.check(&TokenKind::Colon) {
                self.advance();
                self.parse_trait_bounds()?
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
                id: self.alloc_ast_id(),
                name,
                name_span,
                is_effect,
                is_pack,
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

        self.expect_gt()?;
        Ok(params)
    }

    /// Parse a single trait bound: `Ord` or `Builder<Output = T, Error = E>`.
    fn parse_trait_bound(&mut self) -> ParseResult<crate::ast::TraitBound> {
        let span = self.peek().span;
        let name = self.consume_ident()?;
        let assoc_types = if self.check(&TokenKind::Lt) {
            self.advance();
            let mut assoc = Vec::new();
            loop {
                if self.check(&TokenKind::Gt) {
                    break;
                }
                let assoc_span = self.peek().span;
                let assoc_name = self.consume_ident()?;
                self.expect(&TokenKind::Eq)?;
                let ty = self.parse_type()?;
                assoc.push(crate::ast::AssocTypeBound {
                    name: assoc_name,
                    ty,
                    span: assoc_span,
                });
                if self.check(&TokenKind::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
            self.expect_gt()?;
            assoc
        } else {
            Vec::new()
        };
        Ok(crate::ast::TraitBound {
            name,
            assoc_types,
            span,
        })
    }

    /// Parse type arguments for turbofish syntax: `<T1, T2, ...>`
    /// Used in function calls like `identity::<i32>(x)`
    fn parse_call_type_args(&mut self) -> ParseResult<Vec<Type>> {
        self.expect(&TokenKind::Lt)?;
        self.parse_type_args()
    }

    fn parse_struct_decl(
        &mut self,
        is_pub: bool,
        attrs: Vec<Attribute>,
    ) -> ParseResult<StructDecl> {
        let id = self.alloc_ast_id();
        let start_span = self.peek().span;
        self.expect(&TokenKind::Struct)?;
        let (name, name_span) = self.consume_ident_with_span()?;

        // Parse generic parameters like <T, U>
        let type_params = self.parse_generic_params()?;

        self.expect(&TokenKind::LBrace)?;

        let fields = self.parse_struct_fields()?;

        let end_span = self.expect(&TokenKind::RBrace)?.span;

        Ok(StructDecl {
            id,
            name,
            name_span,
            is_pub,
            type_params,
            fields,
            attrs,
            span: start_span.merge(&end_span),
        })
    }

    fn parse_struct_fields(&mut self) -> ParseResult<Vec<StructField>> {
        let mut fields = Vec::new();

        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            let attrs = self.parse_attributes()?;
            let id = self.alloc_ast_id();
            let start_span = self.peek().span;
            let is_pub = if self.check(&TokenKind::Pub) {
                self.advance();
                true
            } else {
                false
            };
            // Allow keywords as field names (unambiguous in context)
            let name_span = self.peek().span;
            let name = self.consume_field_name()?;
            self.expect(&TokenKind::Colon)?;
            let ty = self.parse_type()?;

            let default = if self.check(&TokenKind::Eq) {
                self.advance();
                Some(self.parse_expr()?)
            } else {
                None
            };

            // Field span covers the full declaration — start of the
            // first token through the end of the default expression
            // (or the type, if there is no default). A start-only span
            // would leave descendant spans extending past the parent,
            // breaking AstId-keyed trivia attribution that picks the
            // outermost node ending on the comment's line.
            let last_end = default.as_ref().map_or_else(|| ty.span(), Expr::span);
            let span = start_span.merge(&last_end);

            fields.push(StructField {
                id,
                name,
                name_span,
                is_pub,
                ty,
                attrs,
                default,
                span,
            });

            if !self.check(&TokenKind::RBrace) {
                self.expect(&TokenKind::Comma)?;
            }
        }

        Ok(fields)
    }

    fn parse_enum_decl(&mut self, is_pub: bool, attrs: Vec<Attribute>) -> ParseResult<EnumDecl> {
        let id = self.alloc_ast_id();
        let start_span = self.peek().span;
        self.expect(&TokenKind::Enum)?;
        let (name, name_span) = self.consume_ident_with_span()?;

        // Parse generic parameters like <T>
        let type_params = self.parse_generic_params()?;

        self.expect(&TokenKind::LBrace)?;

        let mut cases = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            let case_attrs = self.parse_attributes()?;
            cases.push(self.parse_enum_case(case_attrs)?);
            if !self.check(&TokenKind::RBrace) {
                self.expect(&TokenKind::Comma)?;
            }
        }

        let end_span = self.expect(&TokenKind::RBrace)?.span;

        Ok(EnumDecl {
            id,
            name,
            name_span,
            is_pub,
            type_params,
            cases,
            attrs,
            span: start_span.merge(&end_span),
        })
    }

    fn parse_enum_case(&mut self, attrs: Vec<Attribute>) -> ParseResult<EnumCase> {
        let id = self.alloc_ast_id();
        let start_span = self.peek().span;
        let (name, name_span) = self.consume_ident_with_span()?;

        // Enum cases have no payload (unlike variant cases)
        Ok(EnumCase {
            id,
            name,
            name_span,
            attrs,
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
        let id = self.alloc_ast_id();
        let start_span = self.peek().span;
        self.expect(&TokenKind::Flags)?;
        let (name, name_span) = self.consume_ident_with_span()?;

        self.expect(&TokenKind::LBrace)?;

        let mut flags = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            let flag_attrs = self.parse_attributes()?;
            let flag_id = self.alloc_ast_id();
            let flag_span = self.peek().span;
            let (flag_name, flag_name_span) = self.consume_ident_with_span()?;
            flags.push(FlagsVariant {
                id: flag_id,
                name: flag_name,
                name_span: flag_name_span,
                attrs: flag_attrs,
                span: flag_span,
            });
            // Comma is optional for the last item
            if !self.check(&TokenKind::RBrace) {
                self.expect(&TokenKind::Comma)?;
            }
        }

        let end_span = self.expect(&TokenKind::RBrace)?.span;

        Ok(FlagsDecl {
            id,
            name,
            name_span,
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
    fn parse_variant_decl(
        &mut self,
        is_pub: bool,
        attrs: Vec<Attribute>,
    ) -> ParseResult<VariantDecl> {
        let id = self.alloc_ast_id();
        let start_span = self.peek().span;
        self.expect(&TokenKind::Variant)?;
        let (name, name_span) = self.consume_ident_with_span()?;

        // Parse generic parameters like <T>
        let type_params = self.parse_generic_params()?;

        self.expect(&TokenKind::LBrace)?;

        let mut cases = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            let case_attrs = self.parse_attributes()?;
            cases.push(self.parse_variant_case(case_attrs)?);
            if !self.check(&TokenKind::RBrace) {
                self.expect(&TokenKind::Comma)?;
            }
        }

        let end_span = self.expect(&TokenKind::RBrace)?.span;

        Ok(VariantDecl {
            id,
            name,
            name_span,
            is_pub,
            type_params,
            cases,
            attrs,
            span: start_span.merge(&end_span),
        })
    }

    fn parse_variant_case(&mut self, attrs: Vec<Attribute>) -> ParseResult<VariantCase> {
        let id = self.alloc_ast_id();
        let start_span = self.peek().span;
        let (name, name_span) = self.consume_ident_with_span()?;

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

        // Case span covers the full extent — start through the payload
        // type's end, when present. A start-only span would leave the
        // payload's descendant ids extending past the parent.
        let last_end = payload.as_ref().map_or(name_span, Type::span);
        let span = start_span.merge(&last_end);

        Ok(VariantCase {
            id,
            name,
            name_span,
            payload,
            attrs,
            span,
        })
    }

    /// Parse a `type` declaration: either a newtype (`type Name = T;`) or
    /// a tuple type family declaration (`type [..T];`).
    fn parse_type_decl(&mut self, is_pub: bool, attrs: Vec<Attribute>) -> ParseResult<Item> {
        let start_span = self.peek().span;
        // peek past `type` to see if next token is `[` (tuple type decl)
        if self.peek_nth(1).kind == TokenKind::LBracket {
            let id = self.alloc_ast_id();
            self.expect(&TokenKind::Type)?;
            // Parse `[..T]` as a type (will be a Tuple containing TypePackSpread)
            let _ty = self.parse_type()?;
            let end_span = self.expect(&TokenKind::Semicolon)?.span;
            return Ok(Item::TupleTypeDecl(TupleTypeDecl {
                id,
                is_pub,
                attrs,
                span: start_span.merge(&end_span),
            }));
        }
        self.parse_newtype(is_pub, attrs).map(Item::Newtype)
    }

    fn parse_newtype(&mut self, is_pub: bool, attrs: Vec<Attribute>) -> ParseResult<Newtype> {
        let id = self.alloc_ast_id();
        let start_span = self.peek().span;
        self.expect(&TokenKind::Type)?;
        let (name, name_span) = self.consume_ident_with_span()?;
        let type_params = self.parse_generic_params()?;
        self.expect(&TokenKind::Eq)?;
        let ty = self.parse_type()?;
        self.expect(&TokenKind::Semicolon)?;

        Ok(Newtype {
            id,
            name,
            name_span,
            is_pub,
            type_params,
            ty,
            attrs,
            span: start_span,
        })
    }

    fn parse_impl_block(&mut self) -> ParseResult<ImplBlock> {
        let id = self.alloc_ast_id();
        let start_span = self.peek().span;
        self.expect(&TokenKind::Impl)?;

        // Parse generic parameters like <T> (Rust-style: impl<T: Ord> Array<T>)
        let mut type_params = self.parse_generic_params()?;

        // Parse first type (could be trait name or target type)
        // Supports bounds on type args: impl Array<T: Ord> { ... }
        let first_type = self.parse_impl_target_type(&mut type_params)?;

        // Check if this is `impl Trait for Type` or just `impl Type`
        let (trait_type, ty) = if self.check(&TokenKind::For) {
            self.advance(); // consume 'for'
            let target_type = self.parse_impl_target_type(&mut type_params)?;
            (Some(first_type), target_type)
        } else {
            (None, first_type)
        };

        // `impl Trait for Type;` — synthesis request (compiler generates the body)
        if self.check(&TokenKind::Semicolon) {
            let end_span = self.advance().span;
            if trait_type.is_none() {
                return Err(self.error_at_span(
                    start_span.merge(&end_span),
                    "synthesis request requires a trait: `impl Trait for Type;`",
                ));
            }
            return Ok(ImplBlock {
                id,
                type_params,
                trait_type,
                ty,
                associated_types: Vec::new(),
                constants: Vec::new(),
                methods: Vec::new(),
                is_synthesize_request: true,
                has_rest: false,
                span: start_span.merge(&end_span),
            });
        }

        self.expect(&TokenKind::LBrace)?;

        let mut associated_types = Vec::new();
        let mut constants = Vec::new();
        let mut methods = Vec::new();
        let mut has_rest = false;
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            // Effect-handler rest pattern: `..` denotes "trap on any unimplemented
            // operation of this effect". Must appear last in the impl block; an
            // explicit semicolon after `..` is optional.
            if self.check_dot_dot_or_ellipsis() {
                let dot_span = self.consume_dot_dot()?;
                if self.check(&TokenKind::Semicolon) {
                    self.advance();
                }
                if !self.check(&TokenKind::RBrace) {
                    return Err(self.error_at_span(
                        dot_span,
                        "`..` rest pattern must be the last item in the impl block",
                    ));
                }
                has_rest = true;
                break;
            }

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
                    id: self.alloc_ast_id(),
                    name: assoc_name,
                    ty: assoc_ty,
                    span: type_span.merge(&end),
                });
            } else {
                let is_pub = if self.check(&TokenKind::Pub) {
                    self.advance();
                    true
                } else {
                    false
                };

                // Check if this is an associated constant: `[pub] const NAME: Type = expr;`
                if self.check(&TokenKind::Const) {
                    let const_span = self.peek().span;
                    self.advance();
                    let const_name = self.consume_ident()?;
                    self.expect(&TokenKind::Colon)?;
                    let const_ty = self.parse_type()?;
                    self.expect(&TokenKind::Eq)?;
                    let const_value = self.parse_expr()?;
                    let end = self.expect(&TokenKind::Semicolon)?.span;
                    constants.push(AssociatedConst {
                        id: self.alloc_ast_id(),
                        name: const_name,
                        is_pub,
                        ty: const_ty,
                        value: const_value,
                        span: const_span.merge(&end),
                    });
                } else {
                    // Methods cannot be exported at the CM boundary
                    methods.push(self.parse_function(is_pub, false, false, attrs)?);
                }
            }
        }

        let end_span = self.expect(&TokenKind::RBrace)?.span;

        Ok(ImplBlock {
            id,
            type_params,
            trait_type,
            ty,
            associated_types,
            constants,
            methods,
            is_synthesize_request: false,
            has_rest,
            span: start_span.merge(&end_span),
        })
    }

    /// Parse a type in impl block context, supporting bounds on generic type args.
    /// `impl Array<T: Ord>` extracts T: Ord into `type_params` and returns Generic("Array", [Named("T")]).
    /// `impl Foo<Array<String>, V>` parses `Array<String>` as a full nested generic type.
    /// Falls back to normal `parse_type()` for non-identifier starts (e.g., reference types).
    fn parse_impl_target_type(
        &mut self,
        type_params: &mut Vec<crate::ast::GenericParam>,
    ) -> ParseResult<Type> {
        // If not starting with an identifier, fall back to normal type parsing
        if !matches!(self.peek_kind(), TokenKind::Ident(_)) {
            return self.parse_type();
        }

        // Check if this is Ident < ... pattern where <...> may contain bounds or complex types.
        // If there's no <, use normal parse_type.
        if self.peek_nth(1).kind != TokenKind::Lt {
            return self.parse_type();
        }

        // We have Ident < ... - parse each type arg individually.
        // Each arg is either:
        //   - A bounded type param: `T: Ord` (ident followed by colon) → extract into type_params
        //   - A full type: `Array<String>`, `i32`, `V`, etc. → parse with parse_type()
        //
        // Only bounded type params are added to type_params. Params without bounds are either
        // concrete types (like `i32` in `impl IndexValue<i32>`) or bare type params handled by
        // the resolver. Adding bare params would shift the index of real type params, breaking
        // associated type resolution (e.g., `type Output = T` for `impl IndexValue<i32> for Array<T>`).
        let start_span = self.peek().span;
        let name = self.consume_ident()?;
        self.advance(); // consume '<'

        let mut args: Vec<Type> = Vec::new();
        while !self.pending_gt && !self.check(&TokenKind::Gt) && !self.is_at_end() {
            // Bounded type param: `ident : bounds`
            if matches!(self.peek_kind(), TokenKind::Ident(_))
                && self.peek_nth(1).kind == TokenKind::Colon
            {
                let param_span = self.peek().span;
                let (param_name, param_name_span) = self.consume_ident_with_span()?;
                self.advance(); // consume ':'
                let bounds = self.parse_trait_bounds()?;
                if !type_params.iter().any(|p| p.name == param_name) {
                    type_params.push(crate::ast::GenericParam {
                        id: self.alloc_ast_id(),
                        name: param_name.clone(),
                        name_span: param_name_span,
                        is_effect: false,
                        is_pack: false,
                        bounds,
                        default: None,
                        span: param_span,
                    });
                }
                args.push(Type::Named(crate::ast::NamedType {
                    id: self.alloc_ast_id(),
                    name: param_name,
                    span: param_span,
                    source_interface: None,
                }));
            } else {
                // Full type: bare ident, generic type like Array<String>, reference, etc.
                args.push(self.parse_type()?);
            }

            if self.check(&TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }

        self.expect_gt()?;
        let end_span = self.tokens[self.pos - 1].span; // span of >

        Ok(Type::Generic(crate::ast::GenericType {
            id: self.alloc_ast_id(),
            name,
            args,
            span: start_span.merge(&end_span),
        }))
    }

    /// Parse a trait declaration
    /// ```wado
    /// trait Display {
    ///     fn display(&self) -> String;
    /// }
    /// ```
    fn parse_trait_decl(&mut self, is_pub: bool, attrs: Vec<Attribute>) -> ParseResult<TraitDecl> {
        let id = self.alloc_ast_id();
        let start_span = self.peek().span;
        self.expect(&TokenKind::Trait)?;

        let (name, name_span) = self.consume_ident_with_span()?;

        // Parse generic parameters like <T>
        let type_params = self.parse_generic_params()?;

        self.expect(&TokenKind::LBrace)?;

        let mut associated_types = Vec::new();
        let mut methods = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            let attrs = self.parse_attributes()?;

            // Check if this is an associated type declaration: `type Name;` or `type Name: Bound1 + Bound2;`
            if self.check(&TokenKind::Type) {
                let type_span = self.peek().span;
                let assoc_id = self.alloc_ast_id();
                self.advance();
                let assoc_name = self.consume_ident()?;
                let bounds = if self.check(&TokenKind::Colon) {
                    self.advance();
                    self.parse_trait_bounds()?
                } else {
                    Vec::new()
                };
                let end = self.expect(&TokenKind::Semicolon)?.span;
                associated_types.push(AssociatedTypeDecl {
                    id: assoc_id,
                    name: assoc_name,
                    bounds,
                    span: type_span.merge(&end),
                });
            } else {
                // Trait methods are not pub (visibility comes from trait itself)
                // Trait methods cannot be exported at the CM boundary
                let _ = attrs; // attrs currently unused for trait methods
                methods.push(self.parse_function(false, false, false, Vec::new())?);
            }
        }

        let end_span = self.expect(&TokenKind::RBrace)?.span;

        Ok(TraitDecl {
            id,
            name,
            name_span,
            is_pub,
            type_params,
            associated_types,
            methods,
            attrs,
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
    fn parse_world_decl(&mut self, is_pub: bool, attrs: Vec<Attribute>) -> ParseResult<WorldDecl> {
        let id = self.alloc_ast_id();
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
            id,
            name,
            is_pub,
            attrs,
            imports,
            exports,
            span: start_span.merge(&end_span),
        })
    }

    /// Parse a world import declaration (bare WIT-faithful form).
    /// ```wado
    /// import Stdout;
    /// ```
    fn parse_world_import(&mut self) -> ParseResult<WorldImport> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Import)?;
        let interface_name = self.consume_ident()?;
        let close_span = self.peek().span;
        self.expect(&TokenKind::Semicolon)?;

        Ok(WorldImport {
            interface_name,
            span: start_span.merge(&close_span),
        })
    }

    /// Parse a world export declaration.
    /// ```wado
    /// export Run;                                 // interface export
    /// export async fn run() -> Result<(), ()>;    // freestanding function export
    /// export fn get_version() -> string;
    /// ```
    fn parse_world_export(&mut self) -> ParseResult<WorldExport> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Export)?;

        // Distinguish interface export (`export Foo;`) from function export
        // (`export [async] fn ...;`). `async` and `fn` start function form;
        // a plain identifier starts interface form.
        if !self.check(&TokenKind::Async) && !self.check(&TokenKind::Fn) {
            let interface_name = self.consume_ident()?;
            let close_span = self.peek().span;
            self.expect(&TokenKind::Semicolon)?;
            return Ok(WorldExport::Interface(WorldExportInterface {
                interface_name,
                span: start_span.merge(&close_span),
            }));
        }

        let is_async = if self.check(&TokenKind::Async) {
            self.advance();
            true
        } else {
            false
        };

        self.expect(&TokenKind::Fn)?;
        let name = self.consume_ident()?;

        let _type_params = self.parse_generic_params()?;

        self.expect(&TokenKind::LParen)?;
        let params = self.parse_param_list()?;
        self.expect(&TokenKind::RParen)?;

        let return_type = if self.check(&TokenKind::Arrow) {
            self.advance();
            Some(self.parse_type()?)
        } else {
            None
        };

        let close_span = self.peek().span;
        self.expect(&TokenKind::Semicolon)?;

        Ok(WorldExport::Function(WorldExportFn {
            name,
            is_async,
            params,
            return_type,
            span: start_span.merge(&close_span),
        }))
    }

    /// Parse structured template token parts into AST template parts.
    fn parse_template_string_parts(
        &mut self,
        token_parts: Vec<crate::token::TemplateTokenPart>,
        span: Span,
    ) -> ParseResult<Expr> {
        use crate::ast::{TemplatePart, TemplateStringExpr};
        use crate::token::TemplateTokenPart;

        let mut parts = Vec::new();

        for token_part in token_parts {
            match token_part {
                TemplateTokenPart::Literal(s) => {
                    parts.push(TemplatePart::String(s));
                }
                TemplateTokenPart::Interpolation(raw) => {
                    let (expr_str, format_spec) = self.split_interpolation_format(&raw, span)?;
                    let expr = self.parse_interpolation_expr(expr_str, span)?;
                    parts.push(TemplatePart::Interpolation {
                        expr: Box::new(expr),
                        format: format_spec,
                    });
                }
            }
        }

        Ok(Expr::TemplateString(Box::new(TemplateStringExpr {
            id: self.alloc_ast_id(),
            parts,
            span,
        })))
    }

    /// Split an interpolation source into expression and optional format specifier.
    /// Input is the raw source text between `{` and `}` (e.g. `pi:.2f` or `Module::func()`).
    fn split_interpolation_format<'b>(
        &mut self,
        raw: &'b str,
        span: Span,
    ) -> ParseResult<(&'b str, Option<FormatSpec>)> {
        let mut in_string = false;
        let mut backtick_depth = 0u32;
        let mut brace_depth = 0u32;
        let mut escape_next = false;

        let chars: Vec<char> = raw.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            let ch = chars[i];

            if escape_next {
                escape_next = false;
                i += 1;
                continue;
            }

            match ch {
                '\\' => {
                    if in_string || backtick_depth > 0 {
                        escape_next = true;
                    }
                }
                '"' if backtick_depth == 0 => {
                    in_string = !in_string;
                }
                '`' if !in_string => {
                    backtick_depth = u32::from(backtick_depth == 0);
                }
                '{' if !in_string && backtick_depth == 0 => {
                    brace_depth += 1;
                }
                '}' if !in_string && backtick_depth == 0 => {
                    brace_depth -= 1;
                }
                ':' if !in_string && backtick_depth == 0 && brace_depth == 0 => {
                    // Check for :: (scope resolution)
                    if i + 1 < chars.len() && chars[i + 1] == ':' {
                        i += 2;
                        continue;
                    }
                    // Format specifier found
                    let expr_str = raw[..raw.char_indices().nth(i).unwrap().0].trim();
                    let spec_start = raw.char_indices().nth(i + 1).map_or(raw.len(), |c| c.0);
                    let spec = &raw[spec_start..];
                    if spec.is_empty() {
                        return Err(ParseError {
                            message: "empty format specifier in template string".to_string(),
                            span,
                        });
                    }
                    return Ok((
                        expr_str,
                        Some(FormatSpec {
                            spec: spec.to_string(),
                        }),
                    ));
                }
                _ => {}
            }

            i += 1;
        }

        Ok((raw.trim(), None))
    }

    /// Parse an interpolation expression string.
    fn parse_interpolation_expr(&mut self, expr_str: &str, span: Span) -> ParseResult<Expr> {
        if expr_str.is_empty() {
            return Err(ParseError {
                message: "empty interpolation expression in template string".to_string(),
                span,
            });
        }

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
                let field_name_span = self.peek().span;
                // Allow string literals as field names for JSON compatibility
                let field_name = if let TokenKind::StringLit(s) = self.peek_kind().clone() {
                    self.advance();
                    s
                } else {
                    self.consume_field_name()?
                };
                let field_name_id = self.alloc_ast_id();

                let (value, is_shorthand, field_span) = if self.check(&TokenKind::Colon) {
                    self.advance();
                    let value = self.parse_expr()?;
                    let span = field_name_span.merge(&value.span());
                    (value, false, span)
                } else {
                    // Shorthand: `{ x }` is equivalent to `{ x: x }`
                    (
                        Expr::Ident(IdentExpr {
                            id: self.alloc_ast_id(),
                            name: field_name.clone(),
                            segments: Vec::new(),
                            span: field_name_span,
                        }),
                        true,
                        field_name_span,
                    )
                };

                fields.push(StructLiteralField {
                    name: field_name,
                    name_id: field_name_id,
                    name_span: field_name_span,
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

        let (name_id, name_span) = if name.is_some() {
            (Some(self.alloc_ast_id()), Some(start_span))
        } else {
            (None, None)
        };

        Ok(Expr::StructLiteral(Box::new(StructLiteralExpr {
            id: self.alloc_ast_id(),
            name,
            name_id,
            name_span,
            fields,
            has_trailing_comma,
            span: start_span.merge(&end_span),
        })))
    }
}

fn parse_attr_number(repr: &str, negate: bool, span: Span) -> ParseResult<crate::ast::AttrValue> {
    // Strip numeric underscores for parsing; keep them in the original repr for errors.
    let cleaned: String = repr.chars().filter(|c| *c != '_').collect();
    // Float if it contains '.' or 'e'/'E' (but not hex prefix).
    let is_hex = cleaned.starts_with("0x") || cleaned.starts_with("0X");
    let is_float =
        !is_hex && (cleaned.contains('.') || cleaned.contains('e') || cleaned.contains('E'));
    if is_float {
        let f: f64 = cleaned.parse().map_err(|_| ParseError {
            message: format!("invalid numeric attribute value: {repr}"),
            span,
        })?;
        let v = if negate { -f } else { f };
        Ok(crate::ast::AttrValue::Float(v))
    } else {
        let n: i64 = if let Some(hex) = cleaned
            .strip_prefix("0x")
            .or_else(|| cleaned.strip_prefix("0X"))
        {
            i64::from_str_radix(hex, 16)
        } else if let Some(oct) = cleaned
            .strip_prefix("0o")
            .or_else(|| cleaned.strip_prefix("0O"))
        {
            i64::from_str_radix(oct, 8)
        } else if let Some(bin) = cleaned
            .strip_prefix("0b")
            .or_else(|| cleaned.strip_prefix("0B"))
        {
            i64::from_str_radix(bin, 2)
        } else {
            cleaned.parse()
        }
        .map_err(|_| ParseError {
            message: format!("invalid integer attribute value: {repr}"),
            span,
        })?;
        let v = if negate { -n } else { n };
        Ok(crate::ast::AttrValue::Int(v))
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
                matches!(&use_decl.items[0], UseItem::Simple { name, alias, .. } if name == "println" && alias.is_none())
            );
            assert!(
                matches!(&use_decl.items[1], UseItem::Simple { name, alias, .. } if name == "Stdout" && alias.is_none())
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
                matches!(&use_decl.items[0], UseItem::Simple { name, alias, .. } if name == "println" && alias.as_deref() == Some("print"))
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
            if let UseItem::InterfaceFunctions {
                interface_name,
                functions,
            } = &use_decl.items[0]
            {
                assert_eq!(interface_name, "Stdout");
                assert_eq!(functions.len(), 1);
                assert_eq!(functions[0].name, "write_via_stream");
            } else {
                panic!("expected InterfaceFunctions");
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
            assert_eq!(attrs.version(), Some("0.3.0".to_string()));
        } else {
            panic!("expected use declaration");
        }
    }

    #[test]
    fn test_use_decl_namespace() {
        let module = parse(r#"use utils from "./utils.wado";"#).unwrap();

        if let Item::Use(use_decl) = &module.items[0] {
            assert_eq!(use_decl.source, "./utils.wado");
            assert_eq!(use_decl.items.len(), 1);
            assert!(matches!(&use_decl.items[0], UseItem::Namespace { name } if name == "utils"));
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
    fn test_function_with_stores() {
        let module = parse("fn store(data: &Data) with stores[data] { }").unwrap();
        if let Item::Function(func) = &module.items[0] {
            assert!(func.effects.is_empty());
            assert_eq!(func.stores, vec!["data"]);
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_function_with_effects_and_stores() {
        let module = parse("fn store(data: &Data) with Stdout, stores[data] { }").unwrap();
        if let Item::Function(func) = &module.items[0] {
            assert_eq!(func.effects, vec!["Stdout"]);
            assert_eq!(func.stores, vec!["data"]);
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_function_with_multiple_stores() {
        let module = parse("fn store(a: &Data, b: &Data) with stores[a, b] { }").unwrap();
        if let Item::Function(func) = &module.items[0] {
            assert!(func.effects.is_empty());
            assert_eq!(func.stores, vec!["a", "b"]);
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_fn_type_with_effect_in_param_list() {
        // `fn(T) -> T with E` inside a parameter list must not consume the next parameter
        let module =
            parse("fn apply<T>(f: fn(T) -> T with Stdout, x: T) -> T with Stdout { return f(x); }")
                .unwrap();
        if let Item::Function(func) = &module.items[0] {
            assert_eq!(func.params.len(), 2);
            assert_eq!(func.params[0].name, "f");
            assert_eq!(func.params[1].name, "x");
            if let Type::Function(ft) = &func.params[0].ty {
                assert_eq!(ft.effects, vec!["Stdout"]);
            } else {
                panic!("expected function type for param f");
            }
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_function_with_stores_self() {
        let module = parse(
            "impl Data { fn store_self(&self) -> Container with stores[self] { return Container { data: self }; } }",
        )
        .unwrap();
        if let Item::Impl(impl_block) = &module.items[0] {
            let method = &impl_block.methods[0];
            assert_eq!(method.stores, vec!["self"]);
        } else {
            panic!("expected impl block");
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
        let source = r"
            world CliCommand {
                import Stdout;

                export async fn run() -> Result<(), ()>;
            }
        ";

        let module = parse(source).unwrap();
        assert_eq!(module.items.len(), 1);

        if let Item::World(world) = &module.items[0] {
            assert_eq!(world.name, "CliCommand");
            assert_eq!(world.imports.len(), 1);
            assert_eq!(world.exports.len(), 1);

            // Check import
            let import = &world.imports[0];
            assert_eq!(import.interface_name, "Stdout");

            // Check export
            let WorldExport::Function(export) = &world.exports[0] else {
                panic!("expected function export");
            };
            assert_eq!(export.name, "run");
            assert!(export.is_async);
            assert!(export.params.is_empty());
            assert!(export.return_type.is_some());
        } else {
            panic!("expected world declaration");
        }
    }

    #[test]
    fn test_world_interface_export() {
        let source = r"
            world Service {
                import Stdout;

                export Handler;
            }
        ";

        let module = parse(source).unwrap();
        if let Item::World(world) = &module.items[0] {
            assert_eq!(world.name, "Service");
            assert_eq!(world.exports.len(), 1);
            let WorldExport::Interface(iface) = &world.exports[0] else {
                panic!("expected interface export");
            };
            assert_eq!(iface.interface_name, "Handler");
        } else {
            panic!("expected world declaration");
        }
    }

    #[test]
    fn test_world_multiple_imports_exports() {
        let source = r"
            world TestWorld {
                import Stdout;
                import Stderr;
                import Environment;

                export async fn run() -> Result<(), ()>;
                export fn get_version() -> string;
            }
        ";

        let module = parse(source).unwrap();

        if let Item::World(world) = &module.items[0] {
            assert_eq!(world.name, "TestWorld");
            assert_eq!(world.imports.len(), 3);
            assert_eq!(world.exports.len(), 2);

            assert_eq!(world.imports[2].interface_name, "Environment");

            let WorldExport::Function(sync_export) = &world.exports[1] else {
                panic!("expected function export");
            };
            assert_eq!(sync_export.name, "get_version");
            assert!(!sync_export.is_async);
        } else {
            panic!("expected world declaration");
        }
    }

    #[test]
    fn test_effect_with_async_method() {
        let source = r"
            interface Http {
                async fn get(url: String) -> Response;
                fn status() -> i32;
            }
        ";

        let module = parse(source).unwrap();

        if let Item::Interface(effect) = &module.items[0] {
            assert_eq!(effect.name, "Http");
            assert_eq!(effect.methods.len(), 2);

            // First method is async
            assert!(effect.methods[0].is_async);
            assert_eq!(effect.methods[0].name, "get");

            // Second method is sync
            assert!(!effect.methods[1].is_async);
            assert_eq!(effect.methods[1].name, "status");
        } else {
            panic!("expected interface declaration");
        }
    }

    #[test]
    fn test_effect_with_cm_attribute() {
        let source = r#"
            pub interface Stdout {
                #[cm("wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream")]
                async fn write_via_stream(data: Stream<u8>) -> Result<(), ErrorCode>;
            }
        "#;

        let module = parse(source).unwrap();

        if let Item::Interface(effect) = &module.items[0] {
            assert_eq!(effect.name, "Stdout");
            assert!(effect.is_pub);
            assert_eq!(effect.methods.len(), 1);

            let method = &effect.methods[0];
            assert!(method.is_async);
            assert_eq!(method.name, "write_via_stream");
            assert_eq!(method.attrs.len(), 1);

            let attr = &method.attrs[0];
            assert_eq!(attr.name, "cm");
            assert!(attr.cm_import.is_some());

            let cm = attr.cm_import.as_ref().unwrap();
            assert_eq!(cm.namespace, "wasi");
            assert_eq!(cm.package, "cli");
            assert_eq!(cm.interface, "stdout");
            assert_eq!(cm.function.as_deref(), Some("write-via-stream"));
        } else {
            panic!("expected interface declaration");
        }
    }

    #[test]
    fn test_export_with_params() {
        let source = r"
            world TestWorld {
                export fn process(input: String, count: i32) -> Result<String, Error>;
            }
        ";

        let module = parse(source).unwrap();

        if let Item::World(world) = &module.items[0] {
            assert_eq!(world.exports.len(), 1);
            let WorldExport::Function(export) = &world.exports[0] else {
                panic!("expected function export");
            };
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
        let source = r"
            pub fn stream_new() -> i64;
            pub fn stream_write(tx: i32, ptr: i32, len: i32) -> i32;
            fn internal_helper();
        ";

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
                    if let Pattern::Ident { name, .. } = &let_stmt.pattern {
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
        let source = r"
            fn test() {
                assert x > 0;
            }
        ";

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
        let source = r"
            fn test() {
                assert x > 0, `x must be positive, got {x}`;
            }
        ";

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

    fn parse_expr_from(source: &str) -> Expr {
        let wrapped = format!("fn __test__() {{ {source}; }}");
        let module = parse(&wrapped).unwrap();
        if let Item::Function(func) = &module.items[0] {
            let body = func.body.as_ref().unwrap();
            if let Stmt::Expr(expr_stmt) = &body.stmts[0] {
                return expr_stmt.expr.clone();
            }
        }
        panic!("failed to parse expression");
    }

    fn parse_pattern_from(source: &str) -> Pattern {
        let wrapped = format!("fn __test__() {{ if let {source} = x {{}} }}");
        let module = parse(&wrapped).unwrap();
        if let Item::Function(func) = &module.items[0] {
            let body = func.body.as_ref().unwrap();
            if let Stmt::If(if_stmt) = &body.stmts[0]
                && let Condition::LetChain { elements, .. } = &if_stmt.condition
                && let crate::ast::ConditionElement::Let { pattern, .. } = &elements[0]
            {
                return pattern.clone();
            }
        }
        panic!("failed to parse pattern from: {source}");
    }

    #[test]
    fn test_comma_separated_trailing_comma() {
        // Trailing comma in arg list
        let module = parse("fn run() { foo(1, 2, 3,); }").unwrap();
        if let Item::Function(func) = &module.items[0] {
            let body = func.body.as_ref().unwrap();
            if let Stmt::Expr(expr_stmt) = &body.stmts[0]
                && let Expr::Call(call) = &expr_stmt.expr
            {
                assert_eq!(call.args.len(), 3);
                assert!(call.has_trailing_comma);
            } else {
                panic!("expected call expression");
            }
        }
    }

    #[test]
    fn test_comma_separated_empty() {
        // Empty param list
        let module = parse("fn foo() {}").unwrap();
        if let Item::Function(func) = &module.items[0] {
            assert!(func.params.is_empty());
        }
    }

    #[test]
    fn test_comma_separated_single_item() {
        let module = parse("fn foo(x: i32) {}").unwrap();
        if let Item::Function(func) = &module.items[0] {
            assert_eq!(func.params.len(), 1);
            assert_eq!(func.params[0].name, "x");
        }
    }

    #[test]
    fn test_type_args_nested_generics() {
        // Array<Array<i32>> should parse correctly (>> splitting)
        let module = parse("fn foo(x: Array<Array<i32>>) {}").unwrap();
        if let Item::Function(func) = &module.items[0] {
            let ty = &func.params[0].ty;
            if let Type::Generic(g) = ty {
                assert_eq!(g.name, "Array");
                assert_eq!(g.args.len(), 1);
                if let Type::Generic(inner) = &g.args[0] {
                    assert_eq!(inner.name, "Array");
                } else {
                    panic!("expected inner generic type");
                }
            } else {
                panic!("expected generic type");
            }
        }
    }

    #[test]
    fn test_turbofish_nested_generics() {
        // Turbofish with nested generics tests >> splitting in parse_call_type_args.
        // Use a simpler case: Result::<i32, String>::Ok(1)
        let module = parse("fn run() { Result::<i32, String>::Ok(1); }").unwrap();
        if let Item::Function(func) = &module.items[0] {
            let body = func.body.as_ref().unwrap();
            // Just verify it parses without error (the >> splitting works)
            assert!(!body.stmts.is_empty());
        }
    }

    #[test]
    fn test_trait_bounds_multiple() {
        let module = parse("fn foo<T: Ord + Clone>() {}").unwrap();
        if let Item::Function(func) = &module.items[0] {
            assert_eq!(func.type_params.len(), 1);
            assert_eq!(func.type_params[0].bounds.len(), 2);
            assert_eq!(func.type_params[0].bounds[0].name, "Ord");
            assert_eq!(func.type_params[0].bounds[1].name, "Clone");
        }
    }

    #[test]
    fn test_trait_bounds_with_assoc_type() {
        let module = parse("fn foo<T: Container<Item = i32>>() {}").unwrap();
        if let Item::Function(func) = &module.items[0] {
            let bound = &func.type_params[0].bounds[0];
            assert_eq!(bound.name, "Container");
            assert_eq!(bound.assoc_types.len(), 1);
            assert_eq!(bound.assoc_types[0].name, "Item");
        }
    }

    #[test]
    fn test_pattern_case_insensitive_variant_with_bindings() {
        // Both uppercase and lowercase identifiers followed by ( should parse as variant patterns
        let upper = parse_pattern_from("Some(x)");
        let lower = parse_pattern_from("some(x)");
        assert!(
            matches!(&upper, Pattern::Variant { variant_name, bindings, .. }
            if variant_name == "Some" && bindings.len() == 1)
        );
        assert!(
            matches!(&lower, Pattern::Variant { variant_name, bindings, .. }
            if variant_name == "some" && bindings.len() == 1)
        );
    }

    #[test]
    fn test_pattern_bare_identifier_always_ident() {
        // Bare identifiers (no parens, no braces) are always Pattern::Ident
        // regardless of case — the resolver disambiguates
        let upper = parse_pattern_from("None");
        let lower = parse_pattern_from("none");
        assert!(matches!(&upper, Pattern::Ident { name, .. } if name == "None"));
        assert!(matches!(&lower, Pattern::Ident { name, .. } if name == "none"));
    }

    #[test]
    fn test_pattern_struct_case_insensitive() {
        // Both cases followed by { should parse as struct patterns
        let upper = parse_pattern_from("Point { x, y }");
        let lower = parse_pattern_from("point { x, y }");
        assert!(matches!(&upper, Pattern::Struct { type_name: Some(n), .. } if n == "Point"));
        assert!(matches!(&lower, Pattern::Struct { type_name: Some(n), .. } if n == "point"));
    }

    #[test]
    fn test_pattern_variant_multiple_bindings() {
        let pat = parse_pattern_from("Pair(a, b)");
        if let Pattern::Variant {
            variant_name,
            bindings,
            ..
        } = &pat
        {
            assert_eq!(variant_name, "Pair");
            assert_eq!(bindings.len(), 2);
        } else {
            panic!("expected variant pattern");
        }
    }

    #[test]
    fn test_pattern_variant_trailing_comma() {
        let pat = parse_pattern_from("Some(x,)");
        if let Pattern::Variant {
            variant_name,
            bindings,
            ..
        } = &pat
        {
            assert_eq!(variant_name, "Some");
            assert_eq!(bindings.len(), 1);
        } else {
            panic!("expected variant pattern");
        }
    }

    #[test]
    fn test_pattern_variant_namespaced_qualifier() {
        let pat = parse_pattern_from("shapes::Shape::Circle(r)");
        if let Pattern::Variant {
            variant_name,
            variant_qualifier,
            bindings,
            ..
        } = &pat
        {
            assert_eq!(variant_name, "Circle");
            assert_eq!(bindings.len(), 1);
            assert!(matches!(
                variant_qualifier,
                Some(Type::NamespacedGeneric(ns))
                    if ns.namespace == "shapes" && ns.name == "Shape" && ns.args.is_empty()
            ));
        } else {
            panic!("expected variant pattern");
        }
    }

    #[test]
    fn test_pattern_variant_generic_qualifier() {
        let pat = parse_pattern_from("Result<i32, String>::Ok(v)");
        if let Pattern::Variant {
            variant_name,
            variant_qualifier,
            bindings,
            ..
        } = &pat
        {
            assert_eq!(variant_name, "Ok");
            assert_eq!(bindings.len(), 1);
            assert!(matches!(
                variant_qualifier,
                Some(Type::Generic(g)) if g.name == "Result" && g.args.len() == 2
            ));
        } else {
            panic!("expected variant pattern");
        }
    }

    #[test]
    fn test_closure_params_trailing_comma() {
        let expr = parse_expr_from("|x: i32, y: i32,| x + y");
        assert!(matches!(&expr, Expr::Closure(c) if c.params.len() == 2));
    }

    #[test]
    fn test_tuple_type_empty() {
        let module = parse("fn foo() -> [] {}").unwrap();
        if let Item::Function(func) = &module.items[0] {
            let ret = func.return_type.as_ref().unwrap();
            assert!(matches!(ret, Type::Tuple(types) if types.is_empty()));
        }
    }

    #[test]
    fn test_tuple_type_trailing_comma() {
        let module = parse("fn foo(x: [i32, String,]) {}").unwrap();
        if let Item::Function(func) = &module.items[0] {
            if let Type::Tuple(types) = &func.params[0].ty {
                assert_eq!(types.len(), 2);
            } else {
                panic!("expected tuple type");
            }
        }
    }

    #[test]
    fn test_fn_type_trailing_comma() {
        let module = parse("fn foo(f: fn(i32, i32,) -> i32) {}").unwrap();
        if let Item::Function(func) = &module.items[0] {
            if let Type::Function(ft) = &func.params[0].ty {
                assert_eq!(ft.params.len(), 2);
            } else {
                panic!("expected function type");
            }
        }
    }

    #[test]
    fn test_effect_method_with_generics() {
        // Ensure effect methods with generic parameters parse correctly
        // (previously used a naive skip that could break on nested <>)
        let source = r"
            interface Store {
                fn get<K>(key: K) -> String;
            }
        ";
        let module = parse(source).unwrap();
        if let Item::Interface(effect) = &module.items[0] {
            assert_eq!(effect.methods.len(), 1);
            assert_eq!(effect.methods[0].name, "get");
        }
    }

    #[test]
    fn test_namespaced_generic_type() {
        let module = parse("fn foo(x: core::Result<i32, String>) {}").unwrap();
        if let Item::Function(func) = &module.items[0] {
            if let Type::NamespacedGeneric(ng) = &func.params[0].ty {
                assert_eq!(ng.namespace, "core");
                assert_eq!(ng.name, "Result");
                assert_eq!(ng.args.len(), 2);
            } else {
                panic!("expected namespaced generic type");
            }
        }
    }

    #[test]
    fn test_impl_block_bounded_type_params() {
        let source = "impl Array<T: Ord> { fn sort(&mut self) {} }";
        let module = parse(source).unwrap();
        if let Item::Impl(impl_block) = &module.items[0] {
            assert_eq!(impl_block.type_params.len(), 1);
            assert_eq!(impl_block.type_params[0].name, "T");
            assert_eq!(impl_block.type_params[0].bounds.len(), 1);
            assert_eq!(impl_block.type_params[0].bounds[0].name, "Ord");
        }
    }

    #[test]
    fn test_trait_decl_associated_type_bounds() {
        let source = r"
            trait Container {
                type Item: Eq + Ord;
                fn get(&self) -> Self::Item;
            }
        ";
        let module = parse(source).unwrap();
        if let Item::Trait(trait_decl) = &module.items[0] {
            assert_eq!(trait_decl.associated_types.len(), 1);
            assert_eq!(trait_decl.associated_types[0].bounds.len(), 2);
            assert_eq!(trait_decl.associated_types[0].bounds[0].name, "Eq");
            assert_eq!(trait_decl.associated_types[0].bounds[1].name, "Ord");
        }
    }

    #[test]
    fn test_empty_tuple_literal() {
        let expr = parse_expr_from("[]");
        assert!(matches!(&expr, Expr::TupleLiteral(t) if t.elements.is_empty()));
    }

    #[test]
    fn test_tuple_literal_trailing_comma() {
        let expr = parse_expr_from("[1, 2, 3,]");
        assert!(matches!(&expr, Expr::TupleLiteral(t) if t.elements.len() == 3));
    }

    #[test]
    fn test_inner_attribute_generated_bare() {
        let module = parse("#![generated]\n").unwrap();
        assert!(module.has_generated());
        assert_eq!(module.inner_attributes().len(), 1);
        assert!(module.inner_attributes()[0].args.is_empty());
    }

    #[test]
    fn test_inner_attribute_generated_key_value_metadata() {
        let source = r#"#![generated(by = "tool", sources = ["a.wit", "b.wit"])]
"#;
        let module = parse(source).unwrap();
        assert!(module.has_generated());
        assert_eq!(module.generated_meta("by"), Some("tool"));
        // Array values are exposed via generated_meta_array.
        let sources = module.generated_meta_array("sources").unwrap();
        let owned: Vec<&str> = sources.iter().map(String::as_str).collect();
        assert_eq!(owned, vec!["a.wit", "b.wit"]);
        // Unknown keys return None.
        assert_eq!(module.generated_meta("unknown"), None);
        assert!(module.generated_meta_array("unknown").is_none());
    }

    #[test]
    fn test_inner_attribute_generated_single_source_array() {
        // A single source file is still expressed as an array literal for
        // uniformity: `sources = ["a.wit"]`.
        let source = r#"#![generated(by = "tool", sources = ["a.wit"])]
"#;
        let module = parse(source).unwrap();
        let sources = module.generated_meta_array("sources").unwrap();
        assert_eq!(sources, &["a.wit".to_string()]);
    }

    #[test]
    fn test_inner_attribute_generated_round_trips_through_unparse() {
        let source = r#"#![generated(by = "tool", sources = ["a.wit", "b.wit"])]
"#;
        let module = parse(source).unwrap();
        let formatted = crate::format(source).unwrap();
        // The attribute must round-trip unchanged through the formatter.
        assert!(
            formatted.starts_with(r#"#![generated(by = "tool", sources = ["a.wit", "b.wit"])]"#,),
            "formatted output did not preserve metadata: {formatted}",
        );
        // Metadata is also queryable after unparse+reparse.
        let reparsed = parse(&formatted).unwrap();
        assert_eq!(reparsed.generated_meta("by"), Some("tool"));
        assert_eq!(
            reparsed.generated_meta_array("sources").unwrap(),
            &["a.wit".to_string(), "b.wit".to_string()],
        );
        // Sanity: the original module still works too.
        assert_eq!(
            module.generated_meta_array("sources").unwrap(),
            &["a.wit".to_string(), "b.wit".to_string()],
        );
    }

    /// Extract the substring covered by `span` (byte offsets).
    fn slice<'a>(source: &'a str, span: &Span) -> &'a str {
        &source[span.start..span.end]
    }

    #[test]
    fn function_name_span_covers_identifier_only() {
        let source = "fn greet() {}";
        let module = parse(source).unwrap();
        let Item::Function(func) = &module.items[0] else {
            panic!("expected function");
        };
        assert_eq!(slice(source, &func.name_span), "greet");
        assert_eq!(func.name_span.line, 1);
        assert_eq!(func.name_span.column, 4);
        assert_eq!(func.name_span.end_column, 9);
    }

    #[test]
    fn param_name_span_covers_identifier_only() {
        let source = "fn add(a: i32, b: i32) -> i32 { return a + b; }";
        let module = parse(source).unwrap();
        let Item::Function(func) = &module.items[0] else {
            panic!("expected function");
        };
        assert_eq!(slice(source, &func.params[0].name_span), "a");
        assert_eq!(slice(source, &func.params[1].name_span), "b");
    }

    #[test]
    fn self_param_name_span_covers_self_token() {
        let source = "impl Point { fn get(&self) -> i32 { return 0; } }";
        let module = parse(source).unwrap();
        let Item::Impl(imp) = &module.items[0] else {
            panic!("expected impl");
        };
        let m = &imp.methods[0];
        assert_eq!(slice(source, &m.params[0].name_span), "self");
    }

    #[test]
    fn struct_and_field_name_spans() {
        let source = "struct Point { x: i32, y: i32 }";
        let module = parse(source).unwrap();
        let Item::Struct(s) = &module.items[0] else {
            panic!("expected struct");
        };
        assert_eq!(slice(source, &s.name_span), "Point");
        assert_eq!(slice(source, &s.fields[0].name_span), "x");
        assert_eq!(slice(source, &s.fields[1].name_span), "y");
    }

    #[test]
    fn enum_case_name_span() {
        let source = "enum Color { Red, Green, Blue }";
        let module = parse(source).unwrap();
        let Item::Enum(e) = &module.items[0] else {
            panic!("expected enum");
        };
        assert_eq!(slice(source, &e.name_span), "Color");
        assert_eq!(slice(source, &e.cases[0].name_span), "Red");
        assert_eq!(slice(source, &e.cases[1].name_span), "Green");
        assert_eq!(slice(source, &e.cases[2].name_span), "Blue");
    }

    #[test]
    fn variant_and_case_name_spans() {
        let source = "variant Shape { Circle(f64), Point }";
        let module = parse(source).unwrap();
        let Item::Variant(v) = &module.items[0] else {
            panic!("expected variant");
        };
        assert_eq!(slice(source, &v.name_span), "Shape");
        assert_eq!(slice(source, &v.cases[0].name_span), "Circle");
        assert_eq!(slice(source, &v.cases[1].name_span), "Point");
    }

    #[test]
    fn flags_and_member_name_spans() {
        let source = "flags Perms { Read, Write }";
        let module = parse(source).unwrap();
        let Item::Flags(f) = &module.items[0] else {
            panic!("expected flags");
        };
        assert_eq!(slice(source, &f.name_span), "Perms");
        assert_eq!(slice(source, &f.flags[0].name_span), "Read");
        assert_eq!(slice(source, &f.flags[1].name_span), "Write");
    }

    #[test]
    fn trait_name_span() {
        let source = "trait Greet { fn greet(&self) -> i32; }";
        let module = parse(source).unwrap();
        let Item::Trait(t) = &module.items[0] else {
            panic!("expected trait");
        };
        assert_eq!(slice(source, &t.name_span), "Greet");
        assert_eq!(slice(source, &t.methods[0].name_span), "greet");
    }

    #[test]
    fn newtype_name_span() {
        let source = "type Meters = f64;";
        let module = parse(source).unwrap();
        let Item::Newtype(n) = &module.items[0] else {
            panic!("expected newtype");
        };
        assert_eq!(slice(source, &n.name_span), "Meters");
    }

    #[test]
    fn global_name_span() {
        let source = "global PI: f64 = 3.14;";
        let module = parse(source).unwrap();
        let Item::Global(g) = &module.items[0] else {
            panic!("expected global");
        };
        assert_eq!(slice(source, &g.name_span), "PI");
    }

    #[test]
    fn generic_param_name_span() {
        let source = "fn identity<T>(x: T) -> T { return x; }";
        let module = parse(source).unwrap();
        let Item::Function(func) = &module.items[0] else {
            panic!("expected function");
        };
        assert_eq!(slice(source, &func.type_params[0].name_span), "T");
    }

    #[test]
    fn closure_param_name_span() {
        // Wrap the closure in a function body so it parses as a module-level item.
        let source = "fn test() { let f = |x: i32| x + 1; }";
        let module = parse(source).unwrap();
        let Item::Function(func) = &module.items[0] else {
            panic!("expected function");
        };
        let body = func.body.as_ref().unwrap();
        let Stmt::Let(let_stmt) = &body.stmts[0] else {
            panic!("expected let stmt");
        };
        let Some(Expr::Closure(c)) = let_stmt.value.as_ref() else {
            panic!("expected closure expression");
        };
        assert_eq!(slice(source, &c.params[0].name_span), "x");
    }

    #[test]
    fn let_stmt_name_span_for_ident_pattern() {
        let source = "fn test() { let foo = 1; }";
        let module = parse(source).unwrap();
        let Item::Function(func) = &module.items[0] else {
            panic!("expected function");
        };
        let body = func.body.as_ref().unwrap();
        let Stmt::Let(let_stmt) = &body.stmts[0] else {
            panic!("expected let stmt");
        };
        assert_eq!(slice(source, &let_stmt.name_span), "foo");
    }

    fn binding_effect_name(b: &crate::ast::EffectHandlerBinding) -> Option<&str> {
        match b.effect.as_ref()? {
            Type::Named(t) => Some(t.name.as_str()),
            Type::Generic(t) => Some(t.name.as_str()),
            _ => None,
        }
    }

    #[test]
    fn parse_with_handler_explicit_effect() {
        let expr = parse_expr_from("with Stdout => &mut mock do { println(\"hi\"); }");
        let Expr::WithHandler(w) = expr else {
            panic!("expected WithHandler, got {expr:?}");
        };
        assert_eq!(w.handlers.len(), 1);
        let binding = &w.handlers[0];
        assert_eq!(binding_effect_name(binding), Some("Stdout"));
        // Handler must be a `&mut <expr>` unary expression.
        assert!(matches!(binding.handler, Expr::Unary(_)));
        // Body has one statement.
        assert_eq!(w.body.stmts.len(), 1);
    }

    #[test]
    fn parse_with_handler_multiple_handlers() {
        let expr = parse_expr_from("with Stdout => &mut s, Stderr => &mut e do { f(); }");
        let Expr::WithHandler(w) = expr else {
            panic!("expected WithHandler");
        };
        assert_eq!(w.handlers.len(), 2);
        assert_eq!(binding_effect_name(&w.handlers[0]), Some("Stdout"));
        assert_eq!(binding_effect_name(&w.handlers[1]), Some("Stderr"));
    }

    #[test]
    fn parse_with_handler_generic_effect() {
        // Generic effect names (`Stream<u8>`) must parse cleanly; later
        // compiler phases decide whether to support them.
        let expr = parse_expr_from("with Stream<u8> => &mut cm do { f(); }");
        let Expr::WithHandler(w) = expr else {
            panic!("expected WithHandler");
        };
        assert_eq!(w.handlers.len(), 1);
        let Some(Type::Generic(generic)) = w.handlers[0].effect.as_ref() else {
            panic!("expected generic effect type");
        };
        assert_eq!(generic.name, "Stream");
        assert_eq!(generic.args.len(), 1);
    }

    #[test]
    fn parse_with_handler_bundled() {
        let expr = parse_expr_from("with &mut bundle do { run(); }");
        let Expr::WithHandler(w) = expr else {
            panic!("expected WithHandler");
        };
        assert_eq!(w.handlers.len(), 1);
        assert!(w.handlers[0].effect.is_none());
    }

    #[test]
    fn parse_resume_expression() {
        let expr = parse_expr_from("resume 42");
        let Expr::Resume(r) = expr else {
            panic!("expected Resume");
        };
        assert!(matches!(r.value, Expr::Literal(_)));
    }

    #[test]
    fn parse_impl_block_rest_pattern() {
        let source = r"
            impl Foo for Bar {
                fn op(&self) -> i32 { return 1; }
                ..
            }
        ";
        let module = parse(source).unwrap();
        let Item::Impl(impl_block) = &module.items[0] else {
            panic!("expected impl block");
        };
        assert!(impl_block.has_rest);
        assert_eq!(impl_block.methods.len(), 1);
    }

    #[test]
    fn parse_impl_block_no_rest() {
        let source = r"
            impl Foo for Bar {
                fn op(&self) -> i32 { return 1; }
            }
        ";
        let module = parse(source).unwrap();
        let Item::Impl(impl_block) = &module.items[0] else {
            panic!("expected impl block");
        };
        assert!(!impl_block.has_rest);
    }

    #[test]
    fn parse_impl_block_rest_must_be_last() {
        // `..` followed by another method is rejected.
        let source = r"
            impl Foo for Bar {
                ..
                fn op(&self) -> i32 { return 1; }
            }
        ";
        let err = parse(source).unwrap_err();
        assert!(
            err.message.contains("must be the last item"),
            "unexpected error message: {}",
            err.message
        );
    }

    #[test]
    fn span_end_column_tracks_multi_line_token() {
        // A multi-line block comment followed by a token: the token's end_column
        // should reflect the post-token cursor on the correct line.
        let source = "fn a() {\n    return 1;\n}";
        let module = parse(source).unwrap();
        let Item::Function(func) = &module.items[0] else {
            panic!("expected function");
        };
        // The function span spans line 1..line 3; end_column points at column of
        // first char past the closing brace.
        assert_eq!(func.span.line, 1);
        assert_eq!(func.span.end_line, 3);
        assert_eq!(func.span.end_column, 2); // column after `}` on line 3
    }

    #[test]
    fn return_with_value_no_semicolon_at_block_end() {
        let module = parse("fn f() -> i32 { return 1 }").unwrap();
        let Item::Function(func) = &module.items[0] else {
            panic!("expected function");
        };
        let body = func.body.as_ref().unwrap();
        assert_eq!(body.stmts.len(), 1);
        assert!(matches!(&body.stmts[0], Stmt::Return(r) if r.value.is_some()));
    }

    #[test]
    fn bare_return_no_semicolon_at_block_end() {
        let module = parse("fn f() { return }").unwrap();
        let Item::Function(func) = &module.items[0] else {
            panic!("expected function");
        };
        let body = func.body.as_ref().unwrap();
        assert_eq!(body.stmts.len(), 1);
        assert!(matches!(&body.stmts[0], Stmt::Return(r) if r.value.is_none()));
    }

    #[test]
    fn break_no_semicolon_at_block_end() {
        let module = parse("fn f() { loop { break } }").unwrap();
        let Item::Function(func) = &module.items[0] else {
            panic!("expected function");
        };
        let body = func.body.as_ref().unwrap();
        let Stmt::Loop(loop_stmt) = &body.stmts[0] else {
            panic!("expected loop");
        };
        assert_eq!(loop_stmt.body.stmts.len(), 1);
        let Stmt::Break(brk) = &loop_stmt.body.stmts[0] else {
            panic!("expected break");
        };
        assert!(brk.label.is_none());
        assert!(brk.value.is_none());
    }

    #[test]
    fn break_label_no_semicolon_at_block_end() {
        let module = parse("fn f() { loop { break done } }").unwrap();
        let Item::Function(func) = &module.items[0] else {
            panic!("expected function");
        };
        let body = func.body.as_ref().unwrap();
        let Stmt::Loop(loop_stmt) = &body.stmts[0] else {
            panic!("expected loop");
        };
        let Stmt::Break(brk) = &loop_stmt.body.stmts[0] else {
            panic!("expected break");
        };
        assert_eq!(brk.label.as_deref(), Some("done"));
        assert!(brk.value.is_none());
        // Span must NOT extend past the label into the `}`
        assert!(brk.span.end_column <= loop_stmt.body.span.end_column);
    }

    #[test]
    fn break_label_with_value_no_semicolon_at_block_end() {
        let module = parse("fn f() { loop { break done: 42 } }").unwrap();
        let Item::Function(func) = &module.items[0] else {
            panic!("expected function");
        };
        let body = func.body.as_ref().unwrap();
        let Stmt::Loop(loop_stmt) = &body.stmts[0] else {
            panic!("expected loop");
        };
        let Stmt::Break(brk) = &loop_stmt.body.stmts[0] else {
            panic!("expected break");
        };
        assert_eq!(brk.label.as_deref(), Some("done"));
        assert!(brk.value.is_some());
    }

    #[test]
    fn continue_no_semicolon_at_block_end() {
        let module = parse("fn f() { loop { continue } }").unwrap();
        let Item::Function(func) = &module.items[0] else {
            panic!("expected function");
        };
        let body = func.body.as_ref().unwrap();
        let Stmt::Loop(loop_stmt) = &body.stmts[0] else {
            panic!("expected loop");
        };
        assert_eq!(loop_stmt.body.stmts.len(), 1);
        assert!(matches!(&loop_stmt.body.stmts[0], Stmt::Continue(_)));
    }

    #[test]
    fn task_return_no_semicolon_at_block_end() {
        let module = parse("fn f() { task return 42 }").unwrap();
        let Item::Function(func) = &module.items[0] else {
            panic!("expected function");
        };
        let body = func.body.as_ref().unwrap();
        assert_eq!(body.stmts.len(), 1);
        assert!(matches!(&body.stmts[0], Stmt::TaskReturn(_)));
    }

    #[test]
    fn break_label_span_does_not_include_rbrace() {
        // Regression: `break label }` used to include `}` in the span
        // because the label token's span wasn't saved.
        let source = "fn f() { loop { break done } }";
        let module = parse(source).unwrap();
        let Item::Function(func) = &module.items[0] else {
            panic!("expected function");
        };
        let body = func.body.as_ref().unwrap();
        let Stmt::Loop(loop_stmt) = &body.stmts[0] else {
            panic!("expected loop");
        };
        let Stmt::Break(brk) = &loop_stmt.body.stmts[0] else {
            panic!("expected break");
        };
        // "break done" occupies columns 17..26 (1-indexed). The span
        // must end at the label, not at the `}` which is at column 28.
        assert!(
            brk.span.end_column < 28,
            "break span should not include `}}`: end_column={}",
            brk.span.end_column
        );
    }
}
