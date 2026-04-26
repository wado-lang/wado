// Token definitions for Wado lexer

#[derive(Debug, Clone, PartialEq)]
pub enum TemplateTokenPart {
    /// Literal text with escape sequences resolved (e.g. `\{` → `{`).
    Literal(String),
    /// Raw source text of an interpolation expression (without enclosing braces).
    Interpolation(String),
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
    Const,
    Matches,
    Stores,
    // Note: "test", "do", "resume" are contextual keywords handled by the parser,
    // not as TokenKinds. `do` is only treated as a keyword inside the trailing
    // position of a `with ... do { ... }` clause; `resume` is only treated as a
    // keyword in expression position inside an effect handler method body.

    // Literals
    Ident(String),
    /// String literal: raw source text between the quotes (escape sequences not interpreted).
    StringLit(String),
    TemplateStringLit(Vec<TemplateTokenPart>), // Structured template string parts
    /// Char literal: raw source text between the quotes (escape sequences not interpreted).
    CharLit(String),
    NumberLit(String), // String representation only, type determined by context in resolver
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
            _ => None,
        }
    }

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
            Self::Const => Some("const"),
            Self::Matches => Some("matches"),
            Self::Stores => Some("stores"),
            // Note: "test", "do", "resume" are contextual keywords, not listed here
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
}

impl Token {
    pub fn new(kind: TokenKind, span: Span) -> Self {
        Self { kind, span }
    }
}

impl Span {
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
        }
    }
}
