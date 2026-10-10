// Token definitions for Wado lexer

use std::fmt;

use crate::ast::AstIdSpace;
#[derive(Debug, Clone, PartialEq)]
pub enum TemplateTokenPart {
    /// Literal text with escape sequences resolved (e.g. `\$` → `$`). Only
    /// `${` opens an interpolation, so bare `{` / `}` stay literal.
    Literal(String),
    /// An interpolation. The lexer splits the content at the top-level `:`, so
    /// `expr` is the raw expression source (without enclosing braces) and
    /// `format` is the specifier text after the `:` (if any). Splitting here —
    /// where the same scan already tracks strings/backticks/braces — keeps a
    /// `:` inside parens, brackets, or a char literal out of the specifier.
    Interpolation {
        expr: String,
        format: Option<String>,
        /// Where `expr`'s first byte sits in the source file. The parser lexes
        /// `expr` on its own, which would leave every node inside it carrying a
        /// position relative to the fragment; this is what puts those spans
        /// back on the file they came from.
        origin: Position,
    },
}

impl TemplateTokenPart {
    /// Where an interpolation's expression source starts and ends in the file.
    /// `expr` is verbatim source anchored at `origin`, so the end follows from
    /// the text. `None` for a literal chunk, which records no position.
    #[must_use]
    pub fn expr_bounds(&self) -> Option<(Position, Position)> {
        match self {
            Self::Literal(_) => None,
            Self::Interpolation { expr, origin, .. } => Some((*origin, origin.advance(expr))),
        }
    }

    /// Where an interpolation's `:spec` tail sits: from the `:` through the
    /// last byte of the specifier. `None` when there is no specifier.
    #[must_use]
    pub fn format_bounds(&self) -> Option<(Position, Position)> {
        let Self::Interpolation {
            expr,
            format: Some(spec),
            origin,
        } = self
        else {
            return None;
        };
        let colon = origin.advance(expr);
        Some((colon, colon.advance(":").advance(spec)))
    }
}

/// A byte offset paired with the 1-based line and column it falls on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Position {
    pub offset: usize,
    pub line: usize,
    pub column: usize,
}

impl Position {
    /// Move past `text`, counting the lines and columns it covers.
    #[must_use]
    pub fn advance(self, text: &str) -> Self {
        let lines = text.matches('\n').count();
        Self {
            offset: self.offset + text.len(),
            line: self.line + lines,
            column: match text.rsplit_once('\n') {
                Some((_, tail)) => 1 + tail.chars().count(),
                None => self.column + text.chars().count(),
            },
        }
    }

    /// The span running from here to `end`.
    #[must_use]
    pub fn span_to(self, end: Self) -> Span {
        Span::with_end(
            self.offset,
            end.offset,
            self.line,
            self.column,
            end.line,
            end.column,
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Keywords
    Use,
    From,
    As,
    Fn,
    With,
    Let,
    Mut,
    Return,
    If,
    Else,
    Match,
    For,
    While,
    Loop,
    Break,
    Continue,
    In,
    Of,
    Pub,
    Internal,
    Effect,
    Interface,
    Reactive,
    Struct,
    Enum,
    Variant,
    Flags,
    Type,
    Impl,
    Trait,
    Resource,
    Extends,
    World,
    Async,
    Import,
    Export,
    Assert,
    Global,
    Const,
    Matches,
    // Note: "test", "do", "resume" are contextual keywords handled by the parser,
    // not as TokenKinds. `do` is only treated as a keyword inside the trailing
    // position of a `with ... do { ... }` clause; `resume` is only treated as a
    // keyword in expression position inside an effect handler method body.

    /// `_` alone: the wildcard, which no identifier can spell.
    Underscore,

    // Literals
    Ident(String),
    /// String literal: raw source text between the quotes (escape sequences not interpreted).
    StringLit(String),
    /// Byte-string literal `b"..."`: raw source text between the quotes (escape
    /// sequences not interpreted). Lowers to a constant `List<u8>`.
    ByteStringLit(String),
    /// Byte literal `b'x'`: raw source between the quotes. Lowers to a `u8`.
    ByteCharLit(String),
    TemplateStringLit(Vec<TemplateTokenPart>), // Structured template string parts
    /// Char literal: raw source text between the quotes (escape sequences not interpreted).
    CharLit(String),
    NumberLit(String), // String representation only, type determined by context in elaborator
    True,
    False,
    Null,

    // Punctuation
    LParen,     // (
    RParen,     // )
    LBrace,     // {
    RBrace,     // }
    LBracket,   // [
    RBracket,   // ]
    Comma,      // ,
    Colon,      // :
    Semicolon,  // ;
    ColonColon, // ::
    Dot,        // .
    DotDot,     // ..
    DotDotLt,   // ..<
    DotDotEq,   // ..=
    DotDotDot,  // ... (error token: "did you mean `..`?")
    Arrow,      // ->
    FatArrow,   // =>
    Pipe,       // |
    Ampersand,  // &
    Hash,       // #

    // Operators
    Eq,        // =
    EqEq,      // ==
    NotEq,     // !=
    Lt,        // <
    LtEq,      // <=
    Gt,        // >
    GtEq,      // >=
    LtLt,      // <<
    GtGt,      // >>
    Plus,      // +
    Minus,     // -
    Star,      // *
    Slash,     // /
    Percent,   // %
    Not,       // !
    And,       // &&
    Or,        // ||
    Caret,     // ^
    Tilde,     // ~
    PlusEq,    // +=
    MinusEq,   // -=
    StarEq,    // *=
    SlashEq,   // /=
    PercentEq, // %=
    AmpEq,     // &=
    PipeEq,    // |=
    CaretEq,   // ^=
    ShlEq,     // <<=
    ShrEq,     // >>=
    Question,  // ?

    // Special
    /// Lexer-emitted recovery token for an unrecognised character. Healthy
    /// source never produces this variant; it appears only when
    /// [`crate::lexer::lex`] also pushes a
    /// [`crate::lexer::LexErrorKind::UnexpectedChar`].
    /// [`crate::parser::Parser::from_lex`] filters it out at construction;
    /// LSP semantic tokens and other consumers that read the raw token
    /// stream see it directly.
    Error(String),

    Eof,
}

impl TokenKind {
    /// Returns the identifier name for tokens that can act as identifiers.
    /// This includes regular identifiers and contextual keywords (`flags`, `type`, `of`)
    /// which are only keywords at declaration start but can be used as identifiers elsewhere.
    #[must_use]
    pub fn as_ident_name(&self) -> Option<&str> {
        match self {
            Self::Ident(name) => Some(name),
            // Contextual keywords: only keywords at declaration start
            Self::Flags => Some("flags"),
            Self::Type => Some("type"),
            // `of` is only a keyword after `for let <pattern>`, valid as identifier elsewhere
            Self::Of => Some("of"),
            // `from` appears in `use { x } from "mod"` but is also valid as a type/trait name
            Self::From => Some("from"),
            // `extends` is a keyword only between a resource name and its parent
            Self::Extends => Some("extends"),
            _ => None,
        }
    }

    // `as_keyword_str` (token → keyword text), `from_keyword`, `operator_str`,
    // and `operator_category` are generated from the canonical registries in
    // `crate::syntax`, so the keyword/operator sets cannot drift from the
    // lexer or the editor grammar.
}

/// A token as a diagnostic names it: as written, or by its kind where the text
/// can run to any length.
impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let spelling = match self {
            Self::Ident(name) => return write!(f, "identifier `{name}`"),
            Self::NumberLit(text) => return write!(f, "number `{text}`"),
            Self::Error(text) => return write!(f, "`{text}`"),
            Self::StringLit(_) => return f.write_str("a string literal"),
            Self::ByteStringLit(_) => return f.write_str("a byte string literal"),
            Self::ByteCharLit(_) => return f.write_str("a byte literal"),
            Self::CharLit(_) => return f.write_str("a character literal"),
            Self::TemplateStringLit(_) => return f.write_str("a template literal"),
            Self::Eof => return f.write_str("end of file"),
            Self::LParen => "(",
            Self::RParen => ")",
            Self::LBrace => "{",
            Self::RBrace => "}",
            Self::LBracket => "[",
            Self::RBracket => "]",
            Self::Comma => ",",
            Self::Colon => ":",
            Self::Semicolon => ";",
            Self::Dot => ".",
            Self::Hash => "#",
            Self::Underscore => "_",
            Self::Use
            | Self::From
            | Self::As
            | Self::Fn
            | Self::With
            | Self::Let
            | Self::Mut
            | Self::Return
            | Self::If
            | Self::Else
            | Self::Match
            | Self::For
            | Self::While
            | Self::Loop
            | Self::Break
            | Self::Continue
            | Self::In
            | Self::Of
            | Self::Pub
            | Self::Internal
            | Self::Effect
            | Self::Interface
            | Self::Reactive
            | Self::Struct
            | Self::Enum
            | Self::Variant
            | Self::Flags
            | Self::Type
            | Self::Impl
            | Self::Trait
            | Self::Resource
            | Self::Extends
            | Self::World
            | Self::Async
            | Self::Import
            | Self::Export
            | Self::Assert
            | Self::Global
            | Self::Const
            | Self::Matches
            | Self::True
            | Self::False
            | Self::Null => self
                .as_keyword_str()
                .expect("a keyword token has a spelling"),
            Self::ColonColon
            | Self::DotDot
            | Self::DotDotLt
            | Self::DotDotEq
            | Self::DotDotDot
            | Self::Arrow
            | Self::FatArrow
            | Self::Pipe
            | Self::Ampersand
            | Self::Eq
            | Self::EqEq
            | Self::NotEq
            | Self::Lt
            | Self::LtEq
            | Self::Gt
            | Self::GtEq
            | Self::LtLt
            | Self::GtGt
            | Self::Plus
            | Self::Minus
            | Self::Star
            | Self::Slash
            | Self::Percent
            | Self::Not
            | Self::And
            | Self::Or
            | Self::Caret
            | Self::Tilde
            | Self::PlusEq
            | Self::MinusEq
            | Self::StarEq
            | Self::SlashEq
            | Self::PercentEq
            | Self::AmpEq
            | Self::PipeEq
            | Self::CaretEq
            | Self::ShlEq
            | Self::ShrEq
            | Self::Question => self
                .operator_str()
                .expect("an operator token has a spelling"),
        };
        write!(f, "`{spelling}`")
    }
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub line: usize,
    pub column: usize,
    pub end_line: usize,
    /// 1-based column of the first position past the end of the token (exclusive end).
    /// For a single-line ASCII token, this equals `column + (end - start)`.
    /// For multi-line or multi-byte content, callers must supply the value explicitly
    /// via `Span::with_end`; lexer tracks this via its own cursor state.
    pub end_column: usize,
    /// The parse whose text these offsets index, so a location stays whole
    /// however far it travels from the walk that read it.
    /// [`crate::ast::AstIdSpace::FRESH`] for a synthesized span, which indexes
    /// no text.
    pub space: AstIdSpace,
}

impl Token {
    pub fn new(kind: TokenKind, span: Span) -> Self {
        Self { kind, span }
    }
}

impl Span {
    /// Where this span starts, as `line:column`, for a message that has no
    /// `Diagnostic` to carry it — a compiler-internal invariant report.
    #[must_use]
    pub fn location(&self) -> String {
        format!("{}:{}", self.line, self.column)
    }

    /// The positions this span runs between — the inverse of
    /// [`Position::span_to`].
    #[must_use]
    pub fn bounds(&self) -> (Position, Position) {
        (
            Position {
                offset: self.start,
                line: self.line,
                column: self.column,
            },
            Position {
                offset: self.end,
                line: self.end_line,
                column: self.end_column,
            },
        )
    }

    /// Create a span assuming single-line ASCII content.
    /// `end_column` defaults to `column + (end - start)`, which is correct for
    /// single-line tokens whose byte length equals their column width.
    pub fn new(start: usize, end: usize, line: usize, column: usize) -> Self {
        Self {
            start,
            end,
            line,
            column,
            end_line: line,
            end_column: column + (end - start),
            space: AstIdSpace::FRESH,
        }
    }

    /// Create a span with explicit end line and end column (1-based, exclusive).
    /// Prefer this when the token may span multiple lines or contain multi-byte characters.
    pub fn with_end(
        start: usize,
        end: usize,
        line: usize,
        column: usize,
        end_line: usize,
        end_column: usize,
    ) -> Self {
        Self {
            start,
            end,
            line,
            column,
            end_line,
            end_column,
            space: AstIdSpace::FRESH,
        }
    }

    pub fn end_line(&self) -> usize {
        self.end_line
    }

    pub fn merge(&self, other: &Span) -> Self {
        Self {
            start: self.start,
            end: other.end,
            line: self.line,
            column: self.column,
            end_line: other.end_line,
            end_column: other.end_column,
            space: self.space,
        }
    }

    /// Say which parse's text this span indexes.
    pub fn in_space(mut self, space: AstIdSpace) -> Self {
        self.space = space;
        self
    }
}
