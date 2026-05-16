// Lexer for Wado
// This module must be synchronized with syntax.rs (canonical syntax definition).

use crate::comment::{Comment, CommentKind};
use crate::token::{Span, Token, TokenKind};

/// Check if a string is a valid Wado identifier.
/// Valid identifiers match the pattern /^[a-zA-Z_][a-zA-Z0-9_]*$/
pub fn is_valid_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub struct Lexer<'a> {
    input: &'a str,
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
    pos: usize,
    line: usize,
    column: usize,
    /// Content of the __DATA__ section, if present
    data_section: Option<String>,
    /// Collected comments (not discarded, for formatter use)
    comments: Vec<Comment>,
    /// Shebang line, if present (e.g., "#!/usr/bin/env wado")
    shebang: Option<String>,
}

#[derive(Debug)]
pub struct LexError {
    pub message: String,
    pub span: Span,
}

impl From<LexError> for crate::compiler_host::Diagnostic {
    fn from(e: LexError) -> Self {
        use crate::compiler_host::{Code, DiagnosticSpan, Severity};
        Self {
            severity: Severity::Error,
            code: Code::InvalidSyntax,
            message: format!("lexer error: {}", e.message),
            span: Some(DiagnosticSpan::from_span(&e.span, None)),
        }
    }
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        Self {
            input,
            chars: input.char_indices().peekable(),
            pos: 0,
            line: 1,
            column: 1,
            data_section: None,
            comments: Vec::new(),
            shebang: None,
        }
    }

    /// Create a new lexer with a custom starting line number.
    /// Useful for parsing embedded expressions (e.g., template string interpolations)
    /// where the line number should reflect the original source location.
    pub fn with_line(input: &'a str, line: usize) -> Self {
        Self {
            input,
            chars: input.char_indices().peekable(),
            pos: 0,
            line,
            column: 1,
            data_section: None,
            comments: Vec::new(),
            shebang: None,
        }
    }

    /// Returns the content of the __DATA__ section, if present.
    /// This is available after calling `tokenize()`.
    pub fn data_section(&self) -> Option<&str> {
        self.data_section.as_deref()
    }

    /// Consumes the lexer and returns the data section content.
    pub fn into_data_section(self) -> Option<String> {
        self.data_section
    }

    pub fn comments(&self) -> &[Comment] {
        &self.comments
    }

    pub fn into_comments(self) -> Vec<Comment> {
        self.comments
    }

    pub fn into_parts(self) -> (Option<String>, Vec<Comment>, Option<String>) {
        (self.data_section, self.comments, self.shebang)
    }

    /// Returns the shebang line, if present.
    /// This is available after calling `tokenize()`.
    pub fn shebang(&self) -> Option<&str> {
        self.shebang.as_deref()
    }

    pub fn tokenize(&mut self) -> Result<Vec<Token>, LexError> {
        // Skip shebang if present at the very beginning of the file
        self.skip_shebang();

        let mut tokens = Vec::new();

        loop {
            let token = self.next_token()?;
            let is_eof = token.kind == TokenKind::Eof;
            tokens.push(token);
            if is_eof {
                break;
            }
        }

        Ok(tokens)
    }

    fn next_token(&mut self) -> Result<Token, LexError> {
        self.skip_whitespace_and_comments();

        let start = self.pos;
        let start_line = self.line;
        let start_column = self.column;

        let Some((_, ch)) = self.peek() else {
            return Ok(Token::new(
                TokenKind::Eof,
                Span::new(start, start, start_line, start_column),
            ));
        };

        let kind = match ch {
            // Identifiers and keywords
            'a'..='z' | 'A'..='Z' | '_' => self.lex_ident_or_keyword(),

            // Numbers
            '0'..='9' => self.lex_number()?,

            // Strings
            '"' => self.lex_string()?,

            // Character literals
            '\'' => self.lex_char()?,

            // Template strings
            '`' => self.lex_template_string()?,

            // Punctuation and operators
            '(' => {
                self.advance();
                TokenKind::LParen
            }
            ')' => {
                self.advance();
                TokenKind::RParen
            }
            '{' => {
                self.advance();
                TokenKind::LBrace
            }
            '}' => {
                self.advance();
                TokenKind::RBrace
            }
            '[' => {
                self.advance();
                TokenKind::LBracket
            }
            ']' => {
                self.advance();
                TokenKind::RBracket
            }
            ',' => {
                self.advance();
                TokenKind::Comma
            }
            ';' => {
                self.advance();
                TokenKind::Semicolon
            }
            '.' => {
                self.advance();
                if self.peek_char() == Some('.') {
                    self.advance();
                    match self.peek_char() {
                        Some('.') => {
                            self.advance();
                            TokenKind::DotDotDot
                        }
                        Some('<') => {
                            self.advance();
                            TokenKind::DotDotLt
                        }
                        Some('=') => {
                            self.advance();
                            TokenKind::DotDotEq
                        }
                        _ => TokenKind::DotDot,
                    }
                } else {
                    TokenKind::Dot
                }
            }
            '?' => {
                self.advance();
                TokenKind::Question
            }
            ':' => {
                self.advance();
                if self.peek_char() == Some(':') {
                    self.advance();
                    TokenKind::ColonColon
                } else {
                    TokenKind::Colon
                }
            }
            '-' => {
                self.advance();
                match self.peek_char() {
                    Some('>') => {
                        self.advance();
                        TokenKind::Arrow
                    }
                    Some('=') => {
                        self.advance();
                        TokenKind::MinusEq
                    }
                    _ => TokenKind::Minus,
                }
            }
            '=' => {
                self.advance();
                match self.peek_char() {
                    Some('=') => {
                        self.advance();
                        TokenKind::EqEq
                    }
                    Some('>') => {
                        self.advance();
                        TokenKind::FatArrow
                    }
                    _ => TokenKind::Eq,
                }
            }
            '!' => {
                self.advance();
                if self.peek_char() == Some('=') {
                    self.advance();
                    TokenKind::NotEq
                } else {
                    TokenKind::Not
                }
            }
            '<' => {
                self.advance();
                match self.peek_char() {
                    Some('=') => {
                        self.advance();
                        TokenKind::LtEq
                    }
                    Some('<') => {
                        self.advance();
                        if self.peek_char() == Some('=') {
                            self.advance();
                            TokenKind::ShlEq
                        } else {
                            TokenKind::LtLt
                        }
                    }
                    _ => TokenKind::Lt,
                }
            }
            '>' => {
                self.advance();
                match self.peek_char() {
                    Some('=') => {
                        self.advance();
                        TokenKind::GtEq
                    }
                    Some('>') => {
                        self.advance();
                        if self.peek_char() == Some('=') {
                            self.advance();
                            TokenKind::ShrEq
                        } else {
                            TokenKind::GtGt
                        }
                    }
                    _ => TokenKind::Gt,
                }
            }
            '^' => {
                self.advance();
                if self.peek_char() == Some('=') {
                    self.advance();
                    TokenKind::CaretEq
                } else {
                    TokenKind::Caret
                }
            }
            '~' => {
                self.advance();
                TokenKind::Tilde
            }
            '+' => {
                self.advance();
                if self.peek_char() == Some('=') {
                    self.advance();
                    TokenKind::PlusEq
                } else {
                    TokenKind::Plus
                }
            }
            '*' => {
                self.advance();
                if self.peek_char() == Some('=') {
                    self.advance();
                    TokenKind::StarEq
                } else {
                    TokenKind::Star
                }
            }
            '/' => {
                self.advance();
                if self.peek_char() == Some('=') {
                    self.advance();
                    TokenKind::SlashEq
                } else {
                    TokenKind::Slash
                }
            }
            '%' => {
                self.advance();
                if self.peek_char() == Some('=') {
                    self.advance();
                    TokenKind::PercentEq
                } else {
                    TokenKind::Percent
                }
            }
            '|' => {
                self.advance();
                match self.peek_char() {
                    Some('|') => {
                        self.advance();
                        TokenKind::Or
                    }
                    Some('=') => {
                        self.advance();
                        TokenKind::PipeEq
                    }
                    _ => TokenKind::Pipe,
                }
            }
            '&' => {
                self.advance();
                match self.peek_char() {
                    Some('&') => {
                        self.advance();
                        TokenKind::And
                    }
                    Some('=') => {
                        self.advance();
                        TokenKind::AmpEq
                    }
                    _ => TokenKind::Ampersand,
                }
            }
            '#' => {
                self.advance();
                TokenKind::Hash
            }

            _ => {
                return Err(LexError {
                    message: format!("unexpected character: '{ch}'"),
                    span: Span::new(start, self.pos + 1, start_line, start_column),
                });
            }
        };

        Ok(Token::new(
            kind,
            Span::with_end(
                start,
                self.pos,
                start_line,
                start_column,
                self.line,
                self.column,
            ),
        ))
    }

    fn peek(&mut self) -> Option<(usize, char)> {
        self.chars.peek().copied()
    }

    fn peek_char(&mut self) -> Option<char> {
        self.chars.peek().map(|(_, c)| *c)
    }

    fn advance(&mut self) -> Option<char> {
        if let Some((pos, ch)) = self.chars.next() {
            self.pos = pos + ch.len_utf8();
            if ch == '\n' {
                self.line += 1;
                self.column = 1;
            } else {
                self.column += 1;
            }
            Some(ch)
        } else {
            None
        }
    }

    /// Skip shebang line if present at the very beginning of the file.
    /// Shebang is only valid at position 0 and starts with `#!`.
    /// Note: `#![` is an inner attribute, not a shebang.
    fn skip_shebang(&mut self) {
        // Shebang is only valid at the very beginning of the file
        if self.pos != 0 {
            return;
        }

        // Check for #! but not #![ (inner attribute)
        let remaining = &self.input[self.pos..];
        if !remaining.starts_with("#!") || remaining.starts_with("#![") {
            return;
        }

        // Find the end of the shebang line
        let start = self.pos;
        while let Some((_, ch)) = self.peek() {
            if ch == '\n' {
                break;
            }
            self.advance();
        }

        // Store the shebang line (without trailing newline)
        self.shebang = Some(self.input[start..self.pos].to_string());

        // Skip the newline
        if let Some((_, '\n')) = self.peek() {
            self.advance();
        }
    }

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            // Skip whitespace
            while let Some((_, ch)) = self.peek() {
                if ch.is_whitespace() {
                    self.advance();
                } else {
                    break;
                }
            }

            // Check for __DATA__ at the start of a line
            if self.column == 1 && self.check_data_section() {
                return;
            }

            // Check for comments
            if let Some((_, '/')) = self.peek() {
                let mut chars = self.chars.clone();
                chars.next();
                match chars.peek() {
                    Some((_, '/')) => {
                        // Line comment - collect instead of discard
                        let comment = self.lex_line_comment();
                        self.comments.push(comment);
                        continue;
                    }
                    Some((_, '*')) => {
                        // Block comment - collect instead of discard
                        match self.lex_block_comment() {
                            Ok(comment) => {
                                self.comments.push(comment);
                                continue;
                            }
                            Err(_) => {
                                // Error will be caught in next_token when we
                                // encounter the unterminated block comment again
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }

            break;
        }
    }

    fn lex_line_comment(&mut self) -> Comment {
        let start = self.pos;
        let start_line = self.line;
        let start_column = self.column;

        self.advance(); // first /
        self.advance(); // second /

        // Detect doc comment kinds: `///` or `//!`
        let kind = match self.peek_char() {
            // `///` but not `////` (which is a regular comment)
            Some('/') if self.input.get(self.pos + 1..self.pos + 2) != Some("/") => {
                self.advance(); // skip the third `/`
                CommentKind::DocLine
            }
            Some('!') => {
                self.advance(); // skip the `!`
                CommentKind::ModuleDoc
            }
            _ => CommentKind::Line,
        };

        let text_start = self.pos;
        while let Some((_, ch)) = self.peek() {
            if ch == '\n' {
                break;
            }
            self.advance();
        }

        let text = self.input[text_start..self.pos].to_string();

        Comment {
            text,
            kind,
            span: Span::with_end(
                start,
                self.pos,
                start_line,
                start_column,
                self.line,
                self.column,
            ),
        }
    }

    fn lex_block_comment(&mut self) -> Result<Comment, LexError> {
        let start = self.pos;
        let start_line = self.line;
        let start_column = self.column;

        self.advance(); // /
        self.advance(); // *

        let text_start = self.pos;
        loop {
            match self.peek() {
                None => {
                    return Err(LexError {
                        message: "unterminated block comment".to_string(),
                        span: Span::with_end(
                            start,
                            self.pos,
                            start_line,
                            start_column,
                            self.line,
                            self.column,
                        ),
                    });
                }
                Some((_, '*')) => {
                    self.advance();
                    if self.peek_char() == Some('/') {
                        let text_end = self.pos - 1; // before the *
                        self.advance(); // consume /
                        let text = self.input[text_start..text_end].to_string();
                        return Ok(Comment {
                            text,
                            kind: CommentKind::Block,
                            span: Span::with_end(
                                start,
                                self.pos,
                                start_line,
                                start_column,
                                self.line,
                                self.column,
                            ),
                        });
                    }
                }
                Some(_) => {
                    self.advance();
                }
            }
        }
    }

    /// Check if we're at __DATA__ marker and capture the data section if so.
    /// __DATA__ must be on its own line (^__DATA__$), followed by newline or EOF.
    /// Returns true if __DATA__ was found and processed.
    fn check_data_section(&mut self) -> bool {
        const DATA_MARKER: &str = "__DATA__";

        // Check if we have enough characters and they match __DATA__
        let remaining = &self.input[self.pos..];
        if !remaining.starts_with(DATA_MARKER) {
            return false;
        }

        // Check that __DATA__ is followed by end of input or newline (must be on its own line)
        let after_marker = &remaining[DATA_MARKER.len()..];
        if !after_marker.is_empty() {
            let next_char = after_marker.chars().next().unwrap();
            // Only allow newline (\n or \r) or EOF after __DATA__
            if next_char != '\n' && next_char != '\r' {
                return false;
            }
        }

        // Found __DATA__ marker - consume it
        for _ in 0..DATA_MARKER.len() {
            self.advance();
        }

        // Skip to the end of the __DATA__ line (consume the newline)
        while let Some((_, ch)) = self.peek() {
            self.advance();
            if ch == '\n' {
                break;
            }
        }

        // Capture everything after __DATA__ as the data section
        let data_content = &self.input[self.pos..];
        self.data_section = if data_content.is_empty() {
            Some(String::new())
        } else {
            Some(data_content.to_string())
        };

        // Move position to end of input
        while self.advance().is_some() {}

        true
    }

    fn lex_ident_or_keyword(&mut self) -> TokenKind {
        let start = self.pos;

        while let Some((_, ch)) = self.peek() {
            if ch.is_alphanumeric() || ch == '_' {
                self.advance();
            } else {
                break;
            }
        }

        let text = &self.input[start..self.pos];

        match text {
            "use" => TokenKind::Use,
            "from" => TokenKind::From,
            "as" => TokenKind::As,
            "fn" => TokenKind::Fn,
            "with" => TokenKind::With,
            "let" => TokenKind::Let,
            "mut" => TokenKind::Mut,
            "return" => TokenKind::Return,
            "if" => TokenKind::If,
            "else" => TokenKind::Else,
            "match" => TokenKind::Match,
            "for" => TokenKind::For,
            "while" => TokenKind::While,
            "loop" => TokenKind::Loop,
            "break" => TokenKind::Break,
            "continue" => TokenKind::Continue,
            "in" => TokenKind::In,
            "of" => TokenKind::Of,
            "pub" => TokenKind::Pub,
            "effect" => TokenKind::Effect,
            "interface" => TokenKind::Interface,
            "reactive" => TokenKind::Reactive,
            "unique" => TokenKind::Unique,
            "struct" => TokenKind::Struct,
            "enum" => TokenKind::Enum,
            "variant" => TokenKind::Variant,
            "flags" => TokenKind::Flags,
            "type" => TokenKind::Type,
            "impl" => TokenKind::Impl,
            "trait" => TokenKind::Trait,
            "resource" => TokenKind::Resource,
            "world" => TokenKind::World,
            "async" => TokenKind::Async,
            "import" => TokenKind::Import,
            "export" => TokenKind::Export,
            "assert" => TokenKind::Assert,
            "global" => TokenKind::Global,
            "const" => TokenKind::Const,
            "matches" => TokenKind::Matches,
            "stores" => TokenKind::Stores,
            // Note: "test", "do", "resume" are contextual keywords handled by the parser, not here
            "true" => TokenKind::True,
            "false" => TokenKind::False,
            "null" => TokenKind::Null,
            _ => TokenKind::Ident(text.to_string()),
        }
    }

    fn lex_number(&mut self) -> Result<TokenKind, LexError> {
        let start = self.pos;
        let start_line = self.line;
        let start_column = self.column;

        // Check for hex, binary, or octal prefix
        if self.peek_char() == Some('0') {
            self.advance();
            match self.peek_char() {
                Some('x' | 'X') => {
                    self.advance();
                    return self.lex_hex_number(start, start_line, start_column);
                }
                Some('b' | 'B') => {
                    self.advance();
                    return self.lex_binary_number(start, start_line, start_column);
                }
                Some('o' | 'O') => {
                    self.advance();
                    return self.lex_octal_number(start, start_line, start_column);
                }
                _ => {
                    // Continue with decimal (could be 0, 0.5, etc.)
                }
            }
        }

        // Consume decimal digits and underscores
        while let Some((_, ch)) = self.peek() {
            if ch.is_ascii_digit() || ch == '_' {
                self.advance();
            } else {
                break;
            }
        }

        // Check for float (decimal point followed by digits)
        if self.peek_char() == Some('.') {
            let mut chars = self.chars.clone();
            chars.next();
            if let Some((_, ch)) = chars.peek()
                && ch.is_ascii_digit()
            {
                self.advance(); // consume '.'
                while let Some((_, ch)) = self.peek() {
                    if ch.is_ascii_digit() || ch == '_' {
                        self.advance();
                    } else {
                        break;
                    }
                }
            }
        }

        // Check for scientific notation (e or E)
        if let Some('e' | 'E') = self.peek_char() {
            self.advance();
            // Optional sign
            if let Some('+' | '-') = self.peek_char() {
                self.advance();
            }
            // Must have at least one digit
            if !matches!(self.peek_char(), Some('0'..='9')) {
                return Err(LexError {
                    message: "expected digit after exponent".to_string(),
                    span: Span::with_end(
                        start,
                        self.pos,
                        start_line,
                        start_column,
                        self.line,
                        self.column,
                    ),
                });
            }
            while let Some((_, ch)) = self.peek() {
                if ch.is_ascii_digit() || ch == '_' {
                    self.advance();
                } else {
                    break;
                }
            }
        }

        let text = &self.input[start..self.pos];

        // Return string representation; type is determined by context in resolver
        Ok(TokenKind::NumberLit(text.to_string()))
    }

    fn lex_hex_number(
        &mut self,
        start: usize,
        start_line: usize,
        start_column: usize,
    ) -> Result<TokenKind, LexError> {
        let digit_start = self.pos;

        while let Some((_, ch)) = self.peek() {
            if ch.is_ascii_hexdigit() || ch == '_' {
                self.advance();
            } else {
                break;
            }
        }

        if self.pos == digit_start {
            return Err(LexError {
                message: "expected hex digit after 0x".to_string(),
                span: Span::with_end(
                    start,
                    self.pos,
                    start_line,
                    start_column,
                    self.line,
                    self.column,
                ),
            });
        }

        // Include "0x" prefix in repr; actual parsing happens in resolver
        let repr = self.input[start..self.pos].to_string();
        Ok(TokenKind::NumberLit(repr))
    }

    fn lex_binary_number(
        &mut self,
        start: usize,
        start_line: usize,
        start_column: usize,
    ) -> Result<TokenKind, LexError> {
        let digit_start = self.pos;

        while let Some((_, ch)) = self.peek() {
            if ch == '0' || ch == '1' || ch == '_' {
                self.advance();
            } else {
                break;
            }
        }

        if self.pos == digit_start {
            return Err(LexError {
                message: "expected binary digit after 0b".to_string(),
                span: Span::with_end(
                    start,
                    self.pos,
                    start_line,
                    start_column,
                    self.line,
                    self.column,
                ),
            });
        }

        // Include "0b" prefix in repr; actual parsing happens in resolver
        let repr = self.input[start..self.pos].to_string();
        Ok(TokenKind::NumberLit(repr))
    }

    fn lex_octal_number(
        &mut self,
        start: usize,
        start_line: usize,
        start_column: usize,
    ) -> Result<TokenKind, LexError> {
        let digit_start = self.pos;

        while let Some((_, ch)) = self.peek() {
            if ('0'..='7').contains(&ch) || ch == '_' {
                self.advance();
            } else {
                break;
            }
        }

        if self.pos == digit_start {
            return Err(LexError {
                message: "expected octal digit after 0o".to_string(),
                span: Span::with_end(
                    start,
                    self.pos,
                    start_line,
                    start_column,
                    self.line,
                    self.column,
                ),
            });
        }

        // Include "0o" prefix in repr; actual parsing happens in resolver
        let repr = self.input[start..self.pos].to_string();
        Ok(TokenKind::NumberLit(repr))
    }

    fn lex_string(&mut self) -> Result<TokenKind, LexError> {
        let start = self.pos;
        let start_line = self.line;
        let start_column = self.column;

        self.advance(); // consume opening "
        let content_start = self.pos;

        loop {
            match self.peek() {
                None => {
                    return Err(LexError {
                        message: "unterminated string literal".to_string(),
                        span: Span::with_end(
                            start,
                            self.pos,
                            start_line,
                            start_column,
                            self.line,
                            self.column,
                        ),
                    });
                }
                Some((_, '"')) => {
                    let content_end = self.pos;
                    self.advance();
                    let raw = self.input[content_start..content_end].to_string();
                    return Ok(TokenKind::StringLit(raw));
                }
                Some((_, '\\')) => {
                    self.advance();
                    self.skip_escape();
                }
                Some(_) => {
                    self.advance();
                }
            }
        }
    }

    /// Advance past one escape sequence (after the leading `\` has been consumed).
    /// Only scans — does not validate or interpret escape values.
    fn skip_escape(&mut self) {
        match self.peek_char() {
            Some('u') => {
                self.advance();
                if self.peek_char() == Some('{') {
                    self.advance();
                    while let Some((_, c)) = self.peek() {
                        self.advance();
                        if c == '}' {
                            break;
                        }
                    }
                } else {
                    // \uHHHH — skip up to 4 hex digits
                    for _ in 0..4 {
                        match self.peek_char() {
                            Some(c) if c.is_ascii_hexdigit() => {
                                self.advance();
                            }
                            _ => break,
                        }
                    }
                }
            }
            Some(_) => {
                self.advance();
            }
            None => {}
        }
    }

    fn lex_template_string(&mut self) -> Result<TokenKind, LexError> {
        use crate::token::TemplateTokenPart;

        let start = self.pos;
        let start_line = self.line;
        let start_column = self.column;

        self.advance(); // consume opening `

        let mut parts = Vec::new();
        let mut current_literal = String::new();

        loop {
            match self.peek() {
                None => {
                    return Err(LexError {
                        message: "unterminated template string".to_string(),
                        span: Span::with_end(
                            start,
                            self.pos,
                            start_line,
                            start_column,
                            self.line,
                            self.column,
                        ),
                    });
                }
                Some((_, '\\')) => {
                    current_literal.push('\\');
                    self.advance();
                    match self.peek_char() {
                        Some(
                            ch @ ('{' | '}' | '\\' | '`' | 'n' | 'r' | 't' | '0' | '"' | '\'' | 'b'
                            | 'f'),
                        ) => {
                            current_literal.push(ch);
                            self.advance();
                        }
                        Some('u') => {
                            current_literal.push('u');
                            self.advance();
                            if self.peek_char() == Some('{') {
                                current_literal.push('{');
                                self.advance();
                                while let Some(c) = self.peek_char() {
                                    if c == '}' {
                                        current_literal.push('}');
                                        self.advance();
                                        break;
                                    }
                                    current_literal.push(c);
                                    self.advance();
                                }
                            } else {
                                // \uXXXX form
                                for _ in 0..4 {
                                    if let Some(c) = self.peek_char() {
                                        current_literal.push(c);
                                        self.advance();
                                    }
                                }
                            }
                        }
                        _ => {
                            // Unknown escape — preserve as-is, let resolver report the error
                            if let Some(c) = self.peek_char() {
                                current_literal.push(c);
                                self.advance();
                            }
                        }
                    }
                }
                Some((_, '{')) => {
                    self.advance();
                    if !current_literal.is_empty() {
                        parts.push(TemplateTokenPart::Literal(std::mem::take(
                            &mut current_literal,
                        )));
                    }
                    let interp =
                        self.collect_interpolation_source(start, start_line, start_column)?;
                    parts.push(TemplateTokenPart::Interpolation(interp));
                }
                Some((_, '`')) => {
                    self.advance();
                    break;
                }
                Some((_, ch)) => {
                    self.advance();
                    current_literal.push(ch);
                }
            }
        }

        if !current_literal.is_empty() {
            parts.push(TemplateTokenPart::Literal(current_literal));
        }

        Ok(TokenKind::TemplateStringLit(parts))
    }

    /// Collect the raw source text of an interpolation expression.
    /// Called after consuming the opening `{`. Consumes up to and including the closing `}`.
    fn collect_interpolation_source(
        &mut self,
        start: usize,
        start_line: usize,
        start_column: usize,
    ) -> Result<String, LexError> {
        let mut source = String::new();
        let mut brace_depth = 1u32;
        let mut in_string = false;
        let mut backtick_depth = 0u32;
        let mut escape_next = false;

        loop {
            let Some((_, ch)) = self.peek() else {
                return Err(LexError {
                    message: "unterminated template string".to_string(),
                    span: Span::with_end(
                        start,
                        self.pos,
                        start_line,
                        start_column,
                        self.line,
                        self.column,
                    ),
                });
            };
            self.advance();

            if escape_next {
                source.push(ch);
                escape_next = false;
                continue;
            }

            match ch {
                '\\' => {
                    source.push(ch);
                    if in_string || backtick_depth > 0 {
                        escape_next = true;
                    }
                }
                '"' if backtick_depth == 0 => {
                    source.push(ch);
                    in_string = !in_string;
                }
                '`' if !in_string => {
                    source.push(ch);
                    backtick_depth = u32::from(backtick_depth == 0);
                }
                '{' if !in_string && backtick_depth == 0 => {
                    source.push(ch);
                    brace_depth += 1;
                }
                '}' if !in_string && backtick_depth == 0 => {
                    brace_depth -= 1;
                    if brace_depth == 0 {
                        return Ok(source);
                    }
                    source.push(ch);
                }
                _ => {
                    source.push(ch);
                }
            }
        }
    }

    fn lex_char(&mut self) -> Result<TokenKind, LexError> {
        let start = self.pos;
        let start_line = self.line;
        let start_column = self.column;

        self.advance(); // consume opening '
        let inner_start = self.pos;

        match self.peek() {
            None => {
                return Err(LexError {
                    message: "unterminated character literal".to_string(),
                    span: Span::with_end(
                        start,
                        self.pos,
                        start_line,
                        start_column,
                        self.line,
                        self.column,
                    ),
                });
            }
            Some((_, '\'')) => {
                return Err(LexError {
                    message: "empty character literal".to_string(),
                    span: Span::with_end(
                        start,
                        self.pos,
                        start_line,
                        start_column,
                        self.line,
                        self.column,
                    ),
                });
            }
            Some((_, '\\')) => {
                self.advance();
                self.skip_escape();
            }
            Some(_) => {
                self.advance();
            }
        }

        let raw = self.input[inner_start..self.pos].to_string();

        if self.peek_char() != Some('\'') {
            return Err(LexError {
                message: "unterminated character literal".to_string(),
                span: Span::with_end(
                    start,
                    self.pos,
                    start_line,
                    start_column,
                    self.line,
                    self.column,
                ),
            });
        }
        self.advance(); // consume closing '

        Ok(TokenKind::CharLit(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_valid_ident() {
        // Valid identifiers
        assert!(is_valid_ident("foo"));
        assert!(is_valid_ident("_foo"));
        assert!(is_valid_ident("foo_bar"));
        assert!(is_valid_ident("foo123"));
        assert!(is_valid_ident("_"));
        assert!(is_valid_ident("_123"));

        // Invalid identifiers
        assert!(!is_valid_ident("")); // empty
        assert!(!is_valid_ident("123foo")); // starts with digit
        assert!(!is_valid_ident("foo::bar")); // contains ::
        assert!(!is_valid_ident("Foo^Bar::baz")); // contains ^ and ::
        assert!(!is_valid_ident("Box<i32>")); // contains < and >
        assert!(!is_valid_ident("foo-bar")); // contains -
        assert!(!is_valid_ident("foo bar")); // contains space
    }

    #[test]
    fn test_simple_tokens() {
        let mut lexer = Lexer::new("fn main() { }");
        let tokens = lexer.tokenize().unwrap();

        assert!(matches!(tokens[0].kind, TokenKind::Fn));
        assert!(matches!(&tokens[1].kind, TokenKind::Ident(s) if s == "main"));
        assert!(matches!(tokens[2].kind, TokenKind::LParen));
        assert!(matches!(tokens[3].kind, TokenKind::RParen));
        assert!(matches!(tokens[4].kind, TokenKind::LBrace));
        assert!(matches!(tokens[5].kind, TokenKind::RBrace));
        assert!(matches!(tokens[6].kind, TokenKind::Eof));
    }

    #[test]
    fn test_use_statement() {
        // Test the new ESM-like import syntax
        let mut lexer = Lexer::new(r#"use {println} from "core:cli";"#);
        let tokens = lexer.tokenize().unwrap();

        assert!(matches!(tokens[0].kind, TokenKind::Use));
        assert!(matches!(tokens[1].kind, TokenKind::LBrace));
        assert!(matches!(&tokens[2].kind, TokenKind::Ident(s) if s == "println"));
        assert!(matches!(tokens[3].kind, TokenKind::RBrace));
        assert!(matches!(tokens[4].kind, TokenKind::From));
        assert!(matches!(&tokens[5].kind, TokenKind::StringLit(raw) if raw == "core:cli"));
        assert!(matches!(tokens[6].kind, TokenKind::Semicolon));
    }

    #[test]
    fn test_string_literal() {
        let mut lexer = Lexer::new(r#""Hello, world!""#);
        let tokens = lexer.tokenize().unwrap();

        assert!(matches!(&tokens[0].kind, TokenKind::StringLit(raw) if raw == "Hello, world!"));
    }

    #[test]
    fn test_comments() {
        let mut lexer = Lexer::new("// comment\nfn");
        let tokens = lexer.tokenize().unwrap();

        assert!(matches!(tokens[0].kind, TokenKind::Fn));

        // Comments should be collected, not discarded
        let comments = lexer.comments();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].text, " comment");
        assert_eq!(comments[0].kind, CommentKind::Line);
    }

    #[test]
    fn test_block_comments() {
        let mut lexer = Lexer::new("/* block comment */fn");
        let tokens = lexer.tokenize().unwrap();

        assert!(matches!(tokens[0].kind, TokenKind::Fn));

        let comments = lexer.comments();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].text, " block comment ");
        assert_eq!(comments[0].kind, CommentKind::Block);
    }

    #[test]
    fn test_multiline_block_comment() {
        let mut lexer = Lexer::new("/*\n * multi-line\n * comment\n */\nfn");
        let tokens = lexer.tokenize().unwrap();

        assert!(matches!(tokens[0].kind, TokenKind::Fn));

        let comments = lexer.comments();
        assert_eq!(comments.len(), 1);
        assert!(comments[0].text.contains("multi-line"));
        assert!(comments[0].text.contains("comment"));
        assert_eq!(comments[0].kind, CommentKind::Block);
    }

    #[test]
    fn test_multiple_comments() {
        let mut lexer = Lexer::new("// first\n/* second */\n// third\nfn");
        let tokens = lexer.tokenize().unwrap();

        assert!(matches!(tokens[0].kind, TokenKind::Fn));

        let comments = lexer.comments();
        assert_eq!(comments.len(), 3);
        assert_eq!(comments[0].text, " first");
        assert_eq!(comments[0].kind, CommentKind::Line);
        assert_eq!(comments[1].text, " second ");
        assert_eq!(comments[1].kind, CommentKind::Block);
        assert_eq!(comments[2].text, " third");
        assert_eq!(comments[2].kind, CommentKind::Line);
    }

    #[test]
    fn test_trailing_comment() {
        let mut lexer = Lexer::new("fn main() { } // trailing comment");
        let tokens = lexer.tokenize().unwrap();

        // Find the function tokens
        assert!(matches!(tokens[0].kind, TokenKind::Fn));

        let comments = lexer.comments();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].text, " trailing comment");
        // The comment should be on the same line as the code
        assert_eq!(comments[0].span.line, 1);
    }

    #[test]
    fn test_data_section_basic() {
        let source = r"fn main() { }
__DATA__
hello world";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().unwrap();

        // Should have parsed the function tokens and EOF
        assert!(matches!(tokens[0].kind, TokenKind::Fn));
        assert!(matches!(tokens.last().unwrap().kind, TokenKind::Eof));

        // Should have captured the data section
        assert_eq!(lexer.data_section(), Some("hello world"));
    }

    #[test]
    fn test_data_section_multiline() {
        let source = r"fn main() { }
__DATA__
line 1
line 2
line 3";
        let mut lexer = Lexer::new(source);
        lexer.tokenize().unwrap();

        assert_eq!(lexer.data_section(), Some("line 1\nline 2\nline 3"));
    }

    #[test]
    fn test_data_section_json() {
        let source = r#"fn main() { }
__DATA__
{
  "exit": 0,
  "stdout": "Hello\n"
}"#;
        let mut lexer = Lexer::new(source);
        lexer.tokenize().unwrap();

        let data = lexer.data_section().unwrap();
        assert!(data.contains("\"exit\": 0"));
        assert!(data.contains("\"stdout\": \"Hello\\n\""));
    }

    #[test]
    fn test_data_section_empty() {
        let source = "fn main() { }\n__DATA__\n";
        let mut lexer = Lexer::new(source);
        lexer.tokenize().unwrap();

        // Empty data section should be Some("")
        assert_eq!(lexer.data_section(), Some(""));
    }

    #[test]
    fn test_no_data_section() {
        let source = "fn main() { }";
        let mut lexer = Lexer::new(source);
        lexer.tokenize().unwrap();

        assert_eq!(lexer.data_section(), None);
    }

    #[test]
    fn test_data_section_not_at_start_of_line() {
        // __DATA__ in the middle of a line should not be recognized
        let source = "let x = __DATA__;";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().unwrap();

        // Should parse __DATA__ as an identifier
        assert!(
            tokens
                .iter()
                .any(|t| matches!(&t.kind, TokenKind::Ident(s) if s == "__DATA__"))
        );
        assert_eq!(lexer.data_section(), None);
    }

    #[test]
    fn test_data_section_with_trailing_content() {
        // __DATA__ with content on the same line should not be recognized
        let source = "fn main() { }\n__DATA__ some content";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().unwrap();

        // Should parse __DATA__ as an identifier since it's not on its own line
        assert!(
            tokens
                .iter()
                .any(|t| matches!(&t.kind, TokenKind::Ident(s) if s == "__DATA__"))
        );
        assert_eq!(lexer.data_section(), None);
    }

    #[test]
    fn test_data_section_after_comment() {
        let source = r"// some comment
fn main() { }
__DATA__
test data";
        let mut lexer = Lexer::new(source);
        lexer.tokenize().unwrap();

        assert_eq!(lexer.data_section(), Some("test data"));
    }

    #[test]
    fn test_into_data_section() {
        let source = "fn main() { }\n__DATA__\nowned data";
        let mut lexer = Lexer::new(source);
        lexer.tokenize().unwrap();

        let data = lexer.into_data_section();
        assert_eq!(data, Some("owned data".to_string()));
    }

    #[test]
    fn test_shebang_basic() {
        let source = "#!/usr/bin/env wado\nfn main() { }";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().unwrap();

        // Shebang should be completely skipped
        assert!(matches!(tokens[0].kind, TokenKind::Fn));
        assert!(matches!(&tokens[1].kind, TokenKind::Ident(s) if s == "main"));
    }

    #[test]
    fn test_shebang_with_args() {
        let source = "#!/usr/bin/wado --some-flag\nfn test() { }";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().unwrap();

        assert!(matches!(tokens[0].kind, TokenKind::Fn));
    }

    #[test]
    fn test_no_shebang() {
        // Regular code without shebang
        let source = "fn main() { }";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().unwrap();

        assert!(matches!(tokens[0].kind, TokenKind::Fn));
    }

    #[test]
    fn test_hash_not_shebang() {
        // Hash on first line but not shebang (no !)
        let source = "#[attr]\nfn main() { }";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().unwrap();

        // Should parse # as Hash token, not skip as shebang
        assert!(matches!(tokens[0].kind, TokenKind::Hash));
    }

    #[test]
    fn test_inner_attribute_not_shebang() {
        // #![ is an inner attribute, not a shebang
        let source = "#![no_prelude]\nfn main() { }";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().unwrap();

        // Should parse #, !, [ as tokens, not skip as shebang
        assert!(matches!(tokens[0].kind, TokenKind::Hash));
        assert!(matches!(tokens[1].kind, TokenKind::Not));
        assert!(matches!(tokens[2].kind, TokenKind::LBracket));
    }

    #[test]
    fn test_shebang_not_on_first_line() {
        // #! on second line should be parsed as Hash + Not, not skipped as shebang
        let source = "fn main() { }\n#!/usr/bin/wado";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().unwrap();

        // Should find Hash and Not tokens after the function
        let has_hash = tokens.iter().any(|t| matches!(t.kind, TokenKind::Hash));
        let has_not = tokens.iter().any(|t| matches!(t.kind, TokenKind::Not));
        assert!(has_hash);
        assert!(has_not);
    }

    #[test]
    fn test_shebang_with_data_section() {
        let source = "#!/usr/bin/env wado\nfn main() { }\n__DATA__\ntest data";
        let mut lexer = Lexer::new(source);
        lexer.tokenize().unwrap();

        assert_eq!(lexer.data_section(), Some("test data"));
    }

    #[test]
    fn test_dot_dot_token() {
        let mut lexer = Lexer::new("..x");
        let tokens = lexer.tokenize().unwrap();
        assert!(matches!(tokens[0].kind, TokenKind::DotDot));
        assert!(matches!(&tokens[1].kind, TokenKind::Ident(s) if s == "x"));
    }

    #[test]
    fn test_dot_dot_dot_token() {
        let mut lexer = Lexer::new("...x");
        let tokens = lexer.tokenize().unwrap();
        assert!(matches!(tokens[0].kind, TokenKind::DotDotDot));
        assert!(matches!(&tokens[1].kind, TokenKind::Ident(s) if s == "x"));
    }

    #[test]
    fn test_dot_dot_lt_token() {
        let mut lexer = Lexer::new("0..<10");
        let tokens = lexer.tokenize().unwrap();
        assert!(matches!(&tokens[0].kind, TokenKind::NumberLit(s) if s == "0"));
        assert!(matches!(tokens[1].kind, TokenKind::DotDotLt));
        assert!(matches!(&tokens[2].kind, TokenKind::NumberLit(s) if s == "10"));
    }

    #[test]
    fn test_dot_dot_eq_token() {
        let mut lexer = Lexer::new("1..=10");
        let tokens = lexer.tokenize().unwrap();
        assert!(matches!(&tokens[0].kind, TokenKind::NumberLit(s) if s == "1"));
        assert!(matches!(tokens[1].kind, TokenKind::DotDotEq));
        assert!(matches!(&tokens[2].kind, TokenKind::NumberLit(s) if s == "10"));
    }

    #[test]
    fn test_dot_dot_lt_with_chars() {
        let mut lexer = Lexer::new("'a'..='z'");
        let tokens = lexer.tokenize().unwrap();
        assert!(matches!(&tokens[0].kind, TokenKind::CharLit(s) if s == "a"));
        assert!(matches!(tokens[1].kind, TokenKind::DotDotEq));
        assert!(matches!(&tokens[2].kind, TokenKind::CharLit(s) if s == "z"));
    }

    #[test]
    fn test_single_dot_followed_by_ident() {
        let mut lexer = Lexer::new("a.b");
        let tokens = lexer.tokenize().unwrap();
        assert!(matches!(&tokens[0].kind, TokenKind::Ident(s) if s == "a"));
        assert!(matches!(tokens[1].kind, TokenKind::Dot));
        assert!(matches!(&tokens[2].kind, TokenKind::Ident(s) if s == "b"));
    }
}
