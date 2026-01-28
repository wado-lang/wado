// Token definitions for Wado lexer

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
    Effect,
    Handler,
    Reactive,
    Move,
    Unique,
    Struct,
    Enum,
    Variant,
    Flags,
    Type,
    Impl,
    Trait,
    Resource,
    World,
    Async,
    Import,
    Export,
    Assert,
    Global,
    Matches,
    // Note: "test" is handled as a contextual keyword in the parser, not as a TokenKind

    // Literals
    Ident(String),
    StringLit(String),
    TemplateStringLit(String), // Raw template string content (without backticks)
    CharLit(char),
    IntLit(String),   // String representation only, parsed in resolver
    FloatLit(String), // String representation only, parsed in resolver
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
    Question,  // ?

    // Special
    Eof,
}

impl TokenKind {
    /// Returns the keyword as a string if this token is a keyword.
    /// Used for allowing keywords as field names.
    #[must_use]
    pub fn as_keyword_str(&self) -> Option<&'static str> {
        match self {
            Self::Use => Some("use"),
            Self::From => Some("from"),
            Self::As => Some("as"),
            Self::Fn => Some("fn"),
            Self::With => Some("with"),
            Self::Let => Some("let"),
            Self::Mut => Some("mut"),
            Self::Return => Some("return"),
            Self::If => Some("if"),
            Self::Else => Some("else"),
            Self::Match => Some("match"),
            Self::For => Some("for"),
            Self::While => Some("while"),
            Self::Loop => Some("loop"),
            Self::Break => Some("break"),
            Self::Continue => Some("continue"),
            Self::In => Some("in"),
            Self::Of => Some("of"),
            Self::Pub => Some("pub"),
            Self::Effect => Some("effect"),
            Self::Handler => Some("handler"),
            Self::Reactive => Some("reactive"),
            Self::Move => Some("move"),
            Self::Unique => Some("unique"),
            Self::Struct => Some("struct"),
            Self::Enum => Some("enum"),
            Self::Variant => Some("variant"),
            Self::Flags => Some("flags"),
            Self::Type => Some("type"),
            Self::Impl => Some("impl"),
            Self::Trait => Some("trait"),
            Self::Resource => Some("resource"),
            Self::World => Some("world"),
            Self::Async => Some("async"),
            Self::Import => Some("import"),
            Self::Export => Some("export"),
            Self::Assert => Some("assert"),
            Self::Global => Some("global"),
            Self::Matches => Some("matches"),
            // Note: "test" is a contextual keyword, not listed here
            Self::True => Some("true"),
            Self::False => Some("false"),
            Self::Null => Some("null"),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub line: usize,
    pub column: usize,
    pub end_line: usize,
}

impl Token {
    pub fn new(kind: TokenKind, span: Span) -> Self {
        Self { kind, span }
    }
}

impl Span {
    pub fn new(start: usize, end: usize, line: usize, column: usize) -> Self {
        Self {
            start,
            end,
            line,
            column,
            end_line: line,
        }
    }

    pub fn with_end_line(
        start: usize,
        end: usize,
        line: usize,
        column: usize,
        end_line: usize,
    ) -> Self {
        Self {
            start,
            end,
            line,
            column,
            end_line,
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
        }
    }
}
