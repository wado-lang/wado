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
    Interface,
    Reactive,
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
            Self::Interface => Some("interface"),
            Self::Reactive => Some("reactive"),
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

/// Append a stable byte encoding of `kind` to `out`.
///
/// Concatenating these encodings for every non-comment token in a source
/// file yields a hash input that ignores comments, whitespace, and span
/// positions — used by the Kiln source-hash so a docstring or
/// reformatting edit on a generator's `.wado` files does not churn the
/// `generator_source_hash` field of every consumer's `<primary>.kiln.json`.
///
/// **Stability constraint:** any change to `TokenKind` (rename a variant,
/// add a keyword, tweak a payload) alters this encoding and therefore
/// invalidates every cached `<stable_id>.sources.json` sidecar and the
/// `generator_source_hash` field of every committed `*.kiln.json`. Bump
/// the magic in `wado-cli/src/kiln_provider.rs::combined_sources_hash`
/// (and `SIDECAR_VERSION`) in lockstep, and expect a single noisy diff
/// against committed kiln json after the bump lands.
pub fn canonical_token_bytes(out: &mut Vec<u8>, kind: &TokenKind) {
    use TokenKind::{
        AmpEq, Ampersand, And, Arrow, As, Assert, Async, Break, Caret, CaretEq, CharLit, Colon,
        ColonColon, Comma, Const, Continue, Dot, DotDot, DotDotDot, DotDotEq, DotDotLt, Effect,
        Else, Enum, Eof, Eq, EqEq, Export, False, FatArrow, Flags, Fn, For, From, Global, Gt, GtEq,
        GtGt, Hash, Ident, If, Impl, Import, In, Interface, LBrace, LBracket, LParen, Let, Loop,
        Lt, LtEq, LtLt, Match, Matches, Minus, MinusEq, Mut, Not, NotEq, Null, NumberLit, Of, Or,
        Percent, PercentEq, Pipe, PipeEq, Plus, PlusEq, Pub, Question, RBrace, RBracket, RParen,
        Reactive, Resource, Return, Semicolon, ShlEq, ShrEq, Slash, SlashEq, Star, StarEq, Stores,
        StringLit, Struct, TemplateStringLit, Tilde, Trait, True, Type, Unique, Use, Variant,
        While, With, World,
    };

    fn write_str(out: &mut Vec<u8>, tag: u8, name: &str) {
        out.push(tag);
        out.extend_from_slice(name.as_bytes());
        out.push(0);
    }

    fn write_payload(out: &mut Vec<u8>, tag: u8, name: &str, payload: &[u8]) {
        out.push(tag);
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
    }

    // `b'V'` = unit variant; `b'P'` = variant with string payload;
    // `b'T'` = template-string container.
    match kind {
        // Keywords
        Use => write_str(out, b'V', "Use"),
        From => write_str(out, b'V', "From"),
        As => write_str(out, b'V', "As"),
        Fn => write_str(out, b'V', "Fn"),
        With => write_str(out, b'V', "With"),
        Let => write_str(out, b'V', "Let"),
        Mut => write_str(out, b'V', "Mut"),
        Return => write_str(out, b'V', "Return"),
        If => write_str(out, b'V', "If"),
        Else => write_str(out, b'V', "Else"),
        Match => write_str(out, b'V', "Match"),
        For => write_str(out, b'V', "For"),
        While => write_str(out, b'V', "While"),
        Loop => write_str(out, b'V', "Loop"),
        Break => write_str(out, b'V', "Break"),
        Continue => write_str(out, b'V', "Continue"),
        In => write_str(out, b'V', "In"),
        Of => write_str(out, b'V', "Of"),
        Pub => write_str(out, b'V', "Pub"),
        Effect => write_str(out, b'V', "Effect"),
        Interface => write_str(out, b'V', "Interface"),
        Reactive => write_str(out, b'V', "Reactive"),
        Unique => write_str(out, b'V', "Unique"),
        Struct => write_str(out, b'V', "Struct"),
        Enum => write_str(out, b'V', "Enum"),
        Variant => write_str(out, b'V', "Variant"),
        Flags => write_str(out, b'V', "Flags"),
        Type => write_str(out, b'V', "Type"),
        Impl => write_str(out, b'V', "Impl"),
        Trait => write_str(out, b'V', "Trait"),
        Resource => write_str(out, b'V', "Resource"),
        World => write_str(out, b'V', "World"),
        Async => write_str(out, b'V', "Async"),
        Import => write_str(out, b'V', "Import"),
        Export => write_str(out, b'V', "Export"),
        Assert => write_str(out, b'V', "Assert"),
        Global => write_str(out, b'V', "Global"),
        Const => write_str(out, b'V', "Const"),
        Matches => write_str(out, b'V', "Matches"),
        Stores => write_str(out, b'V', "Stores"),

        // Literals with payload
        Ident(s) => write_payload(out, b'P', "Ident", s.as_bytes()),
        StringLit(s) => write_payload(out, b'P', "StringLit", s.as_bytes()),
        CharLit(s) => write_payload(out, b'P', "CharLit", s.as_bytes()),
        NumberLit(s) => write_payload(out, b'P', "NumberLit", s.as_bytes()),
        TemplateStringLit(parts) => {
            out.push(b'T');
            out.extend_from_slice(&(parts.len() as u32).to_be_bytes());
            for p in parts {
                match p {
                    TemplateTokenPart::Literal(s) => {
                        write_payload(out, b'P', "Literal", s.as_bytes());
                    }
                    TemplateTokenPart::Interpolation(s) => {
                        write_payload(out, b'P', "Interpolation", s.as_bytes());
                    }
                }
            }
        }
        True => write_str(out, b'V', "True"),
        False => write_str(out, b'V', "False"),
        Null => write_str(out, b'V', "Null"),

        // Punctuation
        LParen => write_str(out, b'V', "LParen"),
        RParen => write_str(out, b'V', "RParen"),
        LBrace => write_str(out, b'V', "LBrace"),
        RBrace => write_str(out, b'V', "RBrace"),
        LBracket => write_str(out, b'V', "LBracket"),
        RBracket => write_str(out, b'V', "RBracket"),
        Comma => write_str(out, b'V', "Comma"),
        Colon => write_str(out, b'V', "Colon"),
        Semicolon => write_str(out, b'V', "Semicolon"),
        ColonColon => write_str(out, b'V', "ColonColon"),
        Dot => write_str(out, b'V', "Dot"),
        DotDot => write_str(out, b'V', "DotDot"),
        DotDotLt => write_str(out, b'V', "DotDotLt"),
        DotDotEq => write_str(out, b'V', "DotDotEq"),
        DotDotDot => write_str(out, b'V', "DotDotDot"),
        Arrow => write_str(out, b'V', "Arrow"),
        FatArrow => write_str(out, b'V', "FatArrow"),
        Pipe => write_str(out, b'V', "Pipe"),
        Ampersand => write_str(out, b'V', "Ampersand"),
        Hash => write_str(out, b'V', "Hash"),

        // Operators
        Eq => write_str(out, b'V', "Eq"),
        EqEq => write_str(out, b'V', "EqEq"),
        NotEq => write_str(out, b'V', "NotEq"),
        Lt => write_str(out, b'V', "Lt"),
        LtEq => write_str(out, b'V', "LtEq"),
        Gt => write_str(out, b'V', "Gt"),
        GtEq => write_str(out, b'V', "GtEq"),
        LtLt => write_str(out, b'V', "LtLt"),
        GtGt => write_str(out, b'V', "GtGt"),
        Plus => write_str(out, b'V', "Plus"),
        Minus => write_str(out, b'V', "Minus"),
        Star => write_str(out, b'V', "Star"),
        Slash => write_str(out, b'V', "Slash"),
        Percent => write_str(out, b'V', "Percent"),
        Not => write_str(out, b'V', "Not"),
        And => write_str(out, b'V', "And"),
        Or => write_str(out, b'V', "Or"),
        Caret => write_str(out, b'V', "Caret"),
        Tilde => write_str(out, b'V', "Tilde"),
        PlusEq => write_str(out, b'V', "PlusEq"),
        MinusEq => write_str(out, b'V', "MinusEq"),
        StarEq => write_str(out, b'V', "StarEq"),
        SlashEq => write_str(out, b'V', "SlashEq"),
        PercentEq => write_str(out, b'V', "PercentEq"),
        AmpEq => write_str(out, b'V', "AmpEq"),
        PipeEq => write_str(out, b'V', "PipeEq"),
        CaretEq => write_str(out, b'V', "CaretEq"),
        ShlEq => write_str(out, b'V', "ShlEq"),
        ShrEq => write_str(out, b'V', "ShrEq"),
        Question => write_str(out, b'V', "Question"),

        // Special
        Eof => write_str(out, b'V', "Eof"),
    }
}

#[cfg(test)]
mod canonical_token_bytes_tests {
    use super::{TemplateTokenPart, TokenKind, canonical_token_bytes};

    fn enc(kind: &TokenKind) -> Vec<u8> {
        let mut out = Vec::new();
        canonical_token_bytes(&mut out, kind);
        out
    }

    #[test]
    fn distinct_unit_variants_distinct_encodings() {
        assert_ne!(enc(&TokenKind::Use), enc(&TokenKind::From));
        assert_ne!(enc(&TokenKind::LParen), enc(&TokenKind::RParen));
        assert_ne!(enc(&TokenKind::Eq), enc(&TokenKind::EqEq));
    }

    #[test]
    fn payload_changes_change_encoding() {
        assert_ne!(
            enc(&TokenKind::Ident("foo".into())),
            enc(&TokenKind::Ident("bar".into()))
        );
        assert_ne!(
            enc(&TokenKind::StringLit("x".into())),
            enc(&TokenKind::Ident("x".into())),
        );
    }

    #[test]
    fn template_string_part_kinds_distinguished() {
        let lit = TokenKind::TemplateStringLit(vec![TemplateTokenPart::Literal("hi".into())]);
        let interp =
            TokenKind::TemplateStringLit(vec![TemplateTokenPart::Interpolation("hi".into())]);
        assert_ne!(enc(&lit), enc(&interp));
    }
}
