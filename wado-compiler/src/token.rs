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
    In,
    Pub,
    Effect,
    Handler,
    Reactive,
    Move,
    Unique,
    Struct,
    Enum,
    Type,
    Impl,
    Resource,
    World,
    Async,
    Import,
    Export,
    Assert,

    // Literals
    Ident(String),
    StringLit(String),
    TemplateStringLit(String), // Raw template string content (without backticks)
    CharLit(char),
    IntLit { value: i64, repr: String },
    FloatLit { value: f64, repr: String },
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
