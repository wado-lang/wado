// Recursive descent parser for Wado

use crate::ast::*;
use crate::token::{Span, Token, TokenKind};

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

#[derive(Debug)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
}

type ParseResult<T> = Result<T, ParseError>;

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    pub fn parse(&mut self) -> ParseResult<Module> {
        let mut items = Vec::new();

        while !self.is_at_end() {
            items.push(self.parse_item()?);
        }

        Ok(Module { items })
    }

    // Token handling

    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn peek_kind(&self) -> &TokenKind {
        &self.tokens[self.pos].kind
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

        match self.peek_kind() {
            TokenKind::Use => self.parse_use_decl().map(Item::Use),
            TokenKind::Fn => self.parse_function(is_pub).map(Item::Function),
            TokenKind::Effect => self.parse_effect_decl(is_pub).map(Item::Effect),
            TokenKind::Struct => self.parse_struct_decl(is_pub).map(Item::Struct),
            TokenKind::Record => self.parse_record_decl(is_pub).map(Item::Record),
            TokenKind::Enum => self.parse_enum_decl(is_pub).map(Item::Enum),
            TokenKind::Type => self.parse_type_alias(is_pub).map(Item::Type),
            TokenKind::Impl => self.parse_impl_block().map(Item::Impl),
            TokenKind::Resource => self.parse_resource_decl(attrs).map(Item::Resource),
            TokenKind::World => self.parse_world_decl().map(Item::World),
            _ => Err(ParseError {
                message: format!("expected item, found {:?}", self.peek_kind()),
                span: self.peek().span,
            }),
        }
    }

    fn parse_attributes(&mut self) -> ParseResult<Vec<Attribute>> {
        let mut attrs = Vec::new();

        while self.check(&TokenKind::Hash) {
            attrs.push(self.parse_attribute()?);
        }

        Ok(attrs)
    }

    fn parse_attribute(&mut self) -> ParseResult<Attribute> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Hash)?;
        self.expect(&TokenKind::LBracket)?;

        let name = self.consume_ident()?;

        let args = if self.check(&TokenKind::LParen) {
            self.advance();
            // Parse attribute arguments as a string literal for now
            let arg = if let TokenKind::StringLit(s) = self.peek_kind().clone() {
                self.advance();
                Some(s)
            } else {
                None
            };
            self.expect(&TokenKind::RParen)?;
            arg
        } else {
            None
        };

        self.expect(&TokenKind::RBracket)?;

        // Parse WASI import path if this is a wasi attribute
        let wasi_import = if name == "wasi" {
            args.as_ref().and_then(|s| WasiImport::parse(s))
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
        self.expect(&TokenKind::Semicolon)?;

        Ok(ResourceDecl {
            name,
            attrs,
            span: start_span,
        })
    }

    fn parse_use_decl(&mut self) -> ParseResult<UseDecl> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Use)?;

        // Read all path segments (module path or module::Effect)
        let mut all_segments = vec![self.consume_ident()?];

        while self.check(&TokenKind::ColonColon) {
            let saved_pos = self.pos;
            self.advance(); // consume `::`
            if self.check(&TokenKind::LBrace) {
                // End of path, `::` followed by `{`
                // Restore the `::` so the next section can process it
                self.pos = saved_pos;
                break;
            }
            all_segments.push(self.consume_ident()?);
        }

        // Determine if this is module import or effect import
        // Pattern 1: `use module::path::{items}`     -> all_segments = module path, then ::{
        // Pattern 2: `use module::path::Effect::{items}` -> all_segments includes Effect, then ::{
        let (path, target) = if self.check(&TokenKind::ColonColon) {
            self.advance(); // consume the final `::`

            if self.check(&TokenKind::LBrace) {
                // `::` followed by `{` - all segments are module path
                (all_segments, UseTarget::Module)
            } else {
                // `::` followed by identifier - that's the Effect name
                let effect_name = self.consume_ident()?;
                self.expect(&TokenKind::ColonColon)?; // must be followed by `::`
                // all_segments is the module path
                (all_segments, UseTarget::Effect(effect_name))
            }
        } else {
            // No final `::` - all segments are module path
            (all_segments, UseTarget::Module)
        };

        self.expect(&TokenKind::LBrace)?;

        // Parse items with optional renaming (as alias)
        let mut items = vec![];
        if !self.check(&TokenKind::RBrace) {
            loop {
                let name = self.consume_ident()?;
                let alias = if self.check(&TokenKind::As) {
                    self.advance();
                    Some(self.consume_ident()?)
                } else {
                    None
                };
                items.push(UseItem { name, alias });

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
        self.expect(&TokenKind::Semicolon)?;

        Ok(UseDecl {
            path,
            target,
            items,
            span: start_span,
        })
    }

    fn parse_function(&mut self, is_pub: bool) -> ParseResult<Function> {
        let start_span = self.peek().span;
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

        let effects = if self.check(&TokenKind::With) {
            self.advance();
            self.parse_effect_list()?
        } else {
            Vec::new()
        };

        let body = self.parse_block()?;

        Ok(Function {
            name,
            is_pub,
            params,
            return_type,
            effects,
            body,
            span: start_span,
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
        let name = self.consume_ident()?;
        self.expect(&TokenKind::Colon)?;
        let ty = self.parse_type()?;

        Ok(Param {
            name,
            ty,
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
            stmts.push(self.parse_stmt()?);
        }

        self.expect(&TokenKind::RBrace)?;

        Ok(Block {
            stmts,
            span: start_span,
        })
    }

    fn parse_stmt(&mut self) -> ParseResult<Stmt> {
        match self.peek_kind() {
            TokenKind::Let | TokenKind::Reactive => self.parse_let_stmt(),
            TokenKind::Return => self.parse_return_stmt(),
            TokenKind::If => self.parse_if_stmt(),
            TokenKind::While => self.parse_while_stmt(),
            TokenKind::For => self.parse_for_stmt(),
            _ => self.parse_expr_stmt(),
        }
    }

    fn parse_let_stmt(&mut self) -> ParseResult<Stmt> {
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

        let name = self.consume_ident()?;

        let ty = if self.check(&TokenKind::Colon) {
            self.advance();
            Some(self.parse_type()?)
        } else {
            None
        };

        self.expect(&TokenKind::Eq)?;
        let value = self.parse_expr()?;
        self.expect(&TokenKind::Semicolon)?;

        Ok(Stmt::Let(LetStmt {
            name,
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

        let value = if !self.check(&TokenKind::Semicolon) {
            Some(self.parse_expr()?)
        } else {
            None
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

        let condition = self.parse_expr()?;
        let then_block = self.parse_block()?;

        let else_block = if self.check(&TokenKind::Else) {
            self.advance();
            Some(self.parse_block()?)
        } else {
            None
        };

        Ok(Stmt::If(IfStmt {
            condition,
            then_block,
            else_block,
            span: start_span,
        }))
    }

    fn parse_while_stmt(&mut self) -> ParseResult<Stmt> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::While)?;

        let condition = self.parse_expr()?;
        let body = self.parse_block()?;

        Ok(Stmt::While(WhileStmt {
            condition,
            body,
            span: start_span,
        }))
    }

    fn parse_for_stmt(&mut self) -> ParseResult<Stmt> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::For)?;

        let pattern = self.parse_pattern()?;
        self.expect(&TokenKind::In)?;
        let iter = self.parse_expr()?;
        let body = self.parse_block()?;

        Ok(Stmt::For(ForStmt {
            pattern,
            iter,
            body,
            span: start_span,
        }))
    }

    fn parse_pattern(&mut self) -> ParseResult<Pattern> {
        if self.check(&TokenKind::LParen) {
            // Tuple pattern: (a, b, c)
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
        } else if let TokenKind::Ident(name) = self.peek_kind().clone() {
            self.advance();
            if name == "_" {
                Ok(Pattern::Wildcard)
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

    fn parse_expr_stmt(&mut self) -> ParseResult<Stmt> {
        let start_span = self.peek().span;
        let expr = self.parse_expr()?;
        self.expect(&TokenKind::Semicolon)?;

        Ok(Stmt::Expr(ExprStmt {
            expr,
            span: start_span,
        }))
    }

    // Expression parsing with precedence climbing

    fn parse_expr(&mut self) -> ParseResult<Expr> {
        self.parse_or_expr()
    }

    fn parse_or_expr(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_and_expr()?;

        while self.check(&TokenKind::Or) {
            let start_span = self.peek().span;
            self.advance();
            let right = self.parse_and_expr()?;
            left = Expr::Binary(Box::new(BinaryExpr {
                left,
                op: BinaryOp::Or,
                right,
                span: start_span,
            }));
        }

        Ok(left)
    }

    fn parse_and_expr(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_equality_expr()?;

        while self.check(&TokenKind::And) {
            let start_span = self.peek().span;
            self.advance();
            let right = self.parse_equality_expr()?;
            left = Expr::Binary(Box::new(BinaryExpr {
                left,
                op: BinaryOp::And,
                right,
                span: start_span,
            }));
        }

        Ok(left)
    }

    fn parse_equality_expr(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_comparison_expr()?;

        loop {
            let op = match self.peek_kind() {
                TokenKind::EqEq => BinaryOp::Eq,
                TokenKind::NotEq => BinaryOp::NotEq,
                _ => break,
            };
            let start_span = self.peek().span;
            self.advance();
            let right = self.parse_comparison_expr()?;
            left = Expr::Binary(Box::new(BinaryExpr {
                left,
                op,
                right,
                span: start_span,
            }));
        }

        Ok(left)
    }

    fn parse_comparison_expr(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_additive_expr()?;

        loop {
            let op = match self.peek_kind() {
                TokenKind::Lt => BinaryOp::Lt,
                TokenKind::LtEq => BinaryOp::LtEq,
                TokenKind::Gt => BinaryOp::Gt,
                TokenKind::GtEq => BinaryOp::GtEq,
                _ => break,
            };
            let start_span = self.peek().span;
            self.advance();
            let right = self.parse_additive_expr()?;
            left = Expr::Binary(Box::new(BinaryExpr {
                left,
                op,
                right,
                span: start_span,
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
            let start_span = self.peek().span;
            self.advance();
            let right = self.parse_multiplicative_expr()?;
            left = Expr::Binary(Box::new(BinaryExpr {
                left,
                op,
                right,
                span: start_span,
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
            let start_span = self.peek().span;
            self.advance();
            let right = self.parse_unary_expr()?;
            left = Expr::Binary(Box::new(BinaryExpr {
                left,
                op,
                right,
                span: start_span,
            }));
        }

        Ok(left)
    }

    fn parse_unary_expr(&mut self) -> ParseResult<Expr> {
        let op = match self.peek_kind() {
            TokenKind::Minus => Some(UnaryOp::Neg),
            TokenKind::Not => Some(UnaryOp::Not),
            TokenKind::Ampersand => Some(UnaryOp::Ref),
            TokenKind::Star => Some(UnaryOp::Deref),
            _ => None,
        };

        if let Some(op) = op {
            let start_span = self.peek().span;
            self.advance();
            let expr = self.parse_unary_expr()?;
            return Ok(Expr::Unary(Box::new(UnaryExpr {
                op,
                expr,
                span: start_span,
            })));
        }

        self.parse_postfix_expr()
    }

    fn parse_postfix_expr(&mut self) -> ParseResult<Expr> {
        let mut expr = self.parse_primary_expr()?;

        loop {
            match self.peek_kind() {
                TokenKind::LParen => {
                    let start_span = self.peek().span;
                    self.advance();
                    let args = self.parse_arg_list()?;
                    self.expect(&TokenKind::RParen)?;
                    expr = Expr::Call(Box::new(CallExpr {
                        callee: expr,
                        args,
                        span: start_span,
                    }));
                }
                TokenKind::Dot => {
                    let start_span = self.peek().span;
                    self.advance();
                    let field = self.consume_ident()?;

                    if self.check(&TokenKind::LParen) {
                        self.advance();
                        let args = self.parse_arg_list()?;
                        self.expect(&TokenKind::RParen)?;
                        expr = Expr::MethodCall(Box::new(MethodCallExpr {
                            receiver: expr,
                            method: field,
                            args,
                            span: start_span,
                        }));
                    } else {
                        expr = Expr::FieldAccess(Box::new(FieldAccessExpr {
                            expr,
                            field,
                            span: start_span,
                        }));
                    }
                }
                TokenKind::LBracket => {
                    let start_span = self.peek().span;
                    self.advance();
                    let index = self.parse_expr()?;
                    self.expect(&TokenKind::RBracket)?;
                    expr = Expr::Index(Box::new(IndexExpr {
                        expr,
                        index,
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
                Ok(Expr::Ident(IdentExpr {
                    name,
                    span: start_span,
                }))
            }
            TokenKind::IntLit(value) => {
                self.advance();
                Ok(Expr::Literal(LiteralExpr {
                    value: Literal::Int(value),
                    span: start_span,
                }))
            }
            TokenKind::FloatLit(value) => {
                self.advance();
                Ok(Expr::Literal(LiteralExpr {
                    value: Literal::Float(value),
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
                self.expect(&TokenKind::RParen)?;
                Ok(expr)
            }
            TokenKind::Pipe => self.parse_closure(),
            _ => Err(ParseError {
                message: format!("expected expression, found {:?}", self.peek_kind()),
                span: start_span,
            }),
        }
    }

    fn parse_arg_list(&mut self) -> ParseResult<Vec<Expr>> {
        let mut args = Vec::new();

        if !self.check(&TokenKind::RParen) {
            args.push(self.parse_expr()?);

            while self.check(&TokenKind::Comma) {
                self.advance();
                if self.check(&TokenKind::RParen) {
                    break;
                }
                args.push(self.parse_expr()?);
            }
        }

        Ok(args)
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

        let body = self.parse_expr()?;

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

        // Reference type: &T
        if self.check(&TokenKind::Ampersand) {
            self.advance();
            let inner = self.parse_type()?;
            return Ok(Type::Reference(Box::new(inner)));
        }

        // Tuple type: () or (T, U, ...)
        if self.check(&TokenKind::LParen) {
            self.advance();
            if self.check(&TokenKind::RParen) {
                self.advance();
                // Unit type ()
                return Ok(Type::Tuple(Vec::new()));
            }
            // Tuple with elements
            let mut types = vec![self.parse_type()?];
            while self.check(&TokenKind::Comma) {
                self.advance();
                if self.check(&TokenKind::RParen) {
                    break;
                }
                types.push(self.parse_type()?);
            }
            self.expect(&TokenKind::RParen)?;
            return Ok(Type::Tuple(types));
        }

        let name = self.consume_ident()?;

        if self.check(&TokenKind::Lt) {
            self.advance();
            let mut args = vec![self.parse_type()?];
            while self.check(&TokenKind::Comma) {
                self.advance();
                args.push(self.parse_type()?);
            }
            self.expect(&TokenKind::Gt)?;

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

    fn parse_effect_decl(&mut self, is_pub: bool) -> ParseResult<EffectDecl> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Effect)?;
        let name = self.consume_ident()?;
        self.expect(&TokenKind::LBrace)?;

        let mut methods = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            methods.push(self.parse_effect_method()?);
        }

        self.expect(&TokenKind::RBrace)?;

        Ok(EffectDecl {
            name,
            is_pub,
            methods,
            span: start_span,
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

    fn parse_struct_decl(&mut self, is_pub: bool) -> ParseResult<StructDecl> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Struct)?;
        let name = self.consume_ident()?;
        self.expect(&TokenKind::LBrace)?;

        let fields = self.parse_struct_fields()?;

        self.expect(&TokenKind::RBrace)?;

        Ok(StructDecl {
            name,
            is_pub,
            fields,
            span: start_span,
        })
    }

    fn parse_record_decl(&mut self, is_pub: bool) -> ParseResult<RecordDecl> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Record)?;
        let name = self.consume_ident()?;
        self.expect(&TokenKind::LBrace)?;

        let fields = self.parse_struct_fields()?;

        self.expect(&TokenKind::RBrace)?;

        Ok(RecordDecl {
            name,
            is_pub,
            fields,
            span: start_span,
        })
    }

    fn parse_struct_fields(&mut self) -> ParseResult<Vec<StructField>> {
        let mut fields = Vec::new();

        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            let start_span = self.peek().span;
            let name = self.consume_ident()?;
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

    fn parse_enum_decl(&mut self, is_pub: bool) -> ParseResult<EnumDecl> {
        let start_span = self.peek().span;
        self.expect(&TokenKind::Enum)?;
        let name = self.consume_ident()?;
        self.expect(&TokenKind::LBrace)?;

        let mut variants = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            variants.push(self.parse_enum_variant()?);
            if !self.check(&TokenKind::RBrace) {
                self.expect(&TokenKind::Comma)?;
            }
        }

        self.expect(&TokenKind::RBrace)?;

        Ok(EnumDecl {
            name,
            is_pub,
            variants,
            span: start_span,
        })
    }

    fn parse_enum_variant(&mut self) -> ParseResult<EnumVariant> {
        let start_span = self.peek().span;
        let name = self.consume_ident()?;

        let fields = if self.check(&TokenKind::LParen) {
            self.advance();
            let mut types = vec![self.parse_type()?];
            while self.check(&TokenKind::Comma) {
                self.advance();
                types.push(self.parse_type()?);
            }
            self.expect(&TokenKind::RParen)?;
            Some(types)
        } else {
            None
        };

        Ok(EnumVariant {
            name,
            fields,
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
        let ty = self.parse_type()?;
        self.expect(&TokenKind::LBrace)?;

        let mut methods = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            let is_pub = if self.check(&TokenKind::Pub) {
                self.advance();
                true
            } else {
                false
            };
            methods.push(self.parse_function(is_pub)?);
        }

        self.expect(&TokenKind::RBrace)?;

        Ok(ImplBlock {
            ty,
            methods,
            span: start_span,
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

        self.expect(&TokenKind::RBrace)?;

        Ok(WorldDecl {
            name,
            imports,
            exports,
            span: start_span,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;

    fn parse(source: &str) -> ParseResult<Module> {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().expect("lexer error");
        let mut parser = Parser::new(tokens);
        parser.parse()
    }

    #[test]
    fn test_use_decl() {
        let module = parse("use core::cli::{println, Stdout};").unwrap();
        assert_eq!(module.items.len(), 1);

        if let Item::Use(use_decl) = &module.items[0] {
            assert_eq!(use_decl.path, vec!["core", "cli"]);
            assert_eq!(use_decl.items.len(), 2);
            assert_eq!(use_decl.items[0].name, "println");
            assert_eq!(use_decl.items[0].alias, None);
            assert_eq!(use_decl.items[1].name, "Stdout");
            assert_eq!(use_decl.items[1].alias, None);
            assert!(matches!(use_decl.target, UseTarget::Module));
        } else {
            panic!("expected use declaration");
        }
    }

    #[test]
    fn test_simple_function() {
        let module = parse("fn main() { }").unwrap();
        assert_eq!(module.items.len(), 1);

        if let Item::Function(func) = &module.items[0] {
            assert_eq!(func.name, "main");
            assert!(func.params.is_empty());
            assert!(func.return_type.is_none());
            assert!(func.effects.is_empty());
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_function_with_effects() {
        let module = parse("fn main() with Stdout { }").unwrap();

        if let Item::Function(func) = &module.items[0] {
            assert_eq!(func.effects, vec!["Stdout"]);
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_function_call() {
        let module = parse(r#"fn main() { println("hello"); }"#).unwrap();

        if let Item::Function(func) = &module.items[0] {
            assert_eq!(func.body.stmts.len(), 1);
            if let Stmt::Expr(expr_stmt) = &func.body.stmts[0]
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
                async fn get(url: string) -> Response;
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
                export fn process(input: string, count: i32) -> Result<string, Error>;
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
}
