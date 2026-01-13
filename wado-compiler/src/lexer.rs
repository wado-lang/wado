// Lexer for Wado

use crate::token::{Span, Token, TokenKind};

pub struct Lexer<'a> {
    input: &'a str,
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
    pos: usize,
    line: usize,
    column: usize,
    /// Content of the __DATA__ section, if present
    data_section: Option<String>,
}

#[derive(Debug)]
pub struct LexError {
    pub message: String,
    pub span: Span,
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

    pub fn tokenize(&mut self) -> Result<Vec<Token>, LexError> {
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
                TokenKind::Dot
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
                        TokenKind::LtLt
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
                        TokenKind::GtGt
                    }
                    _ => TokenKind::Gt,
                }
            }
            '^' => {
                self.advance();
                TokenKind::Caret
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
                if self.peek_char() == Some('|') {
                    self.advance();
                    TokenKind::Or
                } else {
                    TokenKind::Pipe
                }
            }
            '&' => {
                self.advance();
                if self.peek_char() == Some('&') {
                    self.advance();
                    TokenKind::And
                } else {
                    TokenKind::Ampersand
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
            Span::new(start, self.pos, start_line, start_column),
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

            // Skip line comments
            if let Some((_, '/')) = self.peek() {
                let mut chars = self.chars.clone();
                chars.next();
                if let Some((_, '/')) = chars.peek() {
                    // Line comment
                    self.advance(); // first /
                    self.advance(); // second /
                    while let Some((_, ch)) = self.peek() {
                        if ch == '\n' {
                            break;
                        }
                        self.advance();
                    }
                    continue;
                }
            }

            break;
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
            "in" => TokenKind::In,
            "pub" => TokenKind::Pub,
            "effect" => TokenKind::Effect,
            "handler" => TokenKind::Handler,
            "reactive" => TokenKind::Reactive,
            "move" => TokenKind::Move,
            "unique" => TokenKind::Unique,
            "struct" => TokenKind::Struct,
            "enum" => TokenKind::Enum,
            "type" => TokenKind::Type,
            "impl" => TokenKind::Impl,
            "resource" => TokenKind::Resource,
            "world" => TokenKind::World,
            "async" => TokenKind::Async,
            "import" => TokenKind::Import,
            "export" => TokenKind::Export,
            "assert" => TokenKind::Assert,
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
                Some('x') | Some('X') => {
                    self.advance();
                    return self.lex_hex_number(start, start_line, start_column);
                }
                Some('b') | Some('B') => {
                    self.advance();
                    return self.lex_binary_number(start, start_line, start_column);
                }
                Some('o') | Some('O') => {
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

        // Check for float (decimal point)
        let is_float = if self.peek_char() == Some('.') {
            let mut chars = self.chars.clone();
            chars.next();
            if let Some((_, ch)) = chars.peek() {
                if ch.is_ascii_digit() {
                    self.advance(); // consume '.'
                    while let Some((_, ch)) = self.peek() {
                        if ch.is_ascii_digit() || ch == '_' {
                            self.advance();
                        } else {
                            break;
                        }
                    }
                    true
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };

        // Check for scientific notation (e or E)
        let has_exponent = if let Some('e') | Some('E') = self.peek_char() {
            self.advance();
            // Optional sign
            if let Some('+') | Some('-') = self.peek_char() {
                self.advance();
            }
            // Must have at least one digit
            if !matches!(self.peek_char(), Some('0'..='9')) {
                return Err(LexError {
                    message: "expected digit after exponent".to_string(),
                    span: Span::new(start, self.pos, start_line, start_column),
                });
            }
            while let Some((_, ch)) = self.peek() {
                if ch.is_ascii_digit() || ch == '_' {
                    self.advance();
                } else {
                    break;
                }
            }
            true
        } else {
            false
        };

        let text = &self.input[start..self.pos];
        // Remove underscores for parsing
        let clean_text: String = text.chars().filter(|&c| c != '_').collect();

        if is_float || has_exponent {
            let value: f64 = clean_text.parse().map_err(|_| LexError {
                message: format!("invalid float literal: {text}"),
                span: Span::new(start, self.pos, start_line, start_column),
            })?;
            Ok(TokenKind::FloatLit(value))
        } else {
            let value: i64 = clean_text.parse().map_err(|_| LexError {
                message: format!("invalid integer literal: {text}"),
                span: Span::new(start, self.pos, start_line, start_column),
            })?;
            Ok(TokenKind::IntLit(value))
        }
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
                span: Span::new(start, self.pos, start_line, start_column),
            });
        }

        let text = &self.input[digit_start..self.pos];
        let clean_text: String = text.chars().filter(|&c| c != '_').collect();

        let value = i64::from_str_radix(&clean_text, 16).map_err(|_| LexError {
            message: format!("invalid hex literal: 0x{text}"),
            span: Span::new(start, self.pos, start_line, start_column),
        })?;

        Ok(TokenKind::IntLit(value))
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
                span: Span::new(start, self.pos, start_line, start_column),
            });
        }

        let text = &self.input[digit_start..self.pos];
        let clean_text: String = text.chars().filter(|&c| c != '_').collect();

        let value = i64::from_str_radix(&clean_text, 2).map_err(|_| LexError {
            message: format!("invalid binary literal: 0b{text}"),
            span: Span::new(start, self.pos, start_line, start_column),
        })?;

        Ok(TokenKind::IntLit(value))
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
                span: Span::new(start, self.pos, start_line, start_column),
            });
        }

        let text = &self.input[digit_start..self.pos];
        let clean_text: String = text.chars().filter(|&c| c != '_').collect();

        let value = i64::from_str_radix(&clean_text, 8).map_err(|_| LexError {
            message: format!("invalid octal literal: 0o{text}"),
            span: Span::new(start, self.pos, start_line, start_column),
        })?;

        Ok(TokenKind::IntLit(value))
    }

    fn lex_string(&mut self) -> Result<TokenKind, LexError> {
        let start = self.pos;
        let start_line = self.line;
        let start_column = self.column;

        self.advance(); // consume opening "

        let mut value = String::new();
        let mut pending_high_surrogate: Option<u16> = None;

        loop {
            match self.peek() {
                None => {
                    return Err(LexError {
                        message: "unterminated string literal".to_string(),
                        span: Span::new(start, self.pos, start_line, start_column),
                    });
                }
                Some((_, '"')) => {
                    self.advance();
                    break;
                }
                Some((_, '\\')) => {
                    self.advance();
                    let ch = self.parse_escape_sequence(start, start_line, start_column)?;

                    // Handle surrogate pairs
                    if let Some(code_unit) = char_to_surrogate(ch) {
                        if is_high_surrogate(code_unit) {
                            if pending_high_surrogate.is_some() {
                                return Err(LexError {
                                    message: "invalid surrogate pair: high surrogate not followed by low surrogate".to_string(),
                                    span: Span::new(start, self.pos, start_line, start_column),
                                });
                            }
                            pending_high_surrogate = Some(code_unit);
                            continue;
                        } else if is_low_surrogate(code_unit)
                            && let Some(high) = pending_high_surrogate.take()
                        {
                            let combined = decode_surrogate_pair(high, code_unit);
                            if let Some(c) = char::from_u32(combined) {
                                value.push(c);
                                continue;
                            } else {
                                return Err(LexError {
                                    message: "invalid surrogate pair".to_string(),
                                    span: Span::new(start, self.pos, start_line, start_column),
                                });
                            }
                        }
                    }

                    // If we had a pending high surrogate but didn't get a low surrogate, error
                    if pending_high_surrogate.is_some() {
                        return Err(LexError {
                            message: "invalid surrogate pair: high surrogate not followed by low surrogate".to_string(),
                            span: Span::new(start, self.pos, start_line, start_column),
                        });
                    }

                    value.push(ch);
                }
                Some((_, ch)) => {
                    if pending_high_surrogate.is_some() {
                        return Err(LexError {
                            message: "invalid surrogate pair: high surrogate not followed by low surrogate".to_string(),
                            span: Span::new(start, self.pos, start_line, start_column),
                        });
                    }
                    self.advance();
                    value.push(ch);
                }
            }
        }

        if pending_high_surrogate.is_some() {
            return Err(LexError {
                message: "invalid surrogate pair: high surrogate at end of string".to_string(),
                span: Span::new(start, self.pos, start_line, start_column),
            });
        }

        Ok(TokenKind::StringLit(value))
    }

    fn parse_escape_sequence(
        &mut self,
        start: usize,
        start_line: usize,
        start_column: usize,
    ) -> Result<char, LexError> {
        match self.peek_char() {
            Some('n') => {
                self.advance();
                Ok('\n')
            }
            Some('t') => {
                self.advance();
                Ok('\t')
            }
            Some('r') => {
                self.advance();
                Ok('\r')
            }
            Some('\\') => {
                self.advance();
                Ok('\\')
            }
            Some('"') => {
                self.advance();
                Ok('"')
            }
            Some('\'') => {
                self.advance();
                Ok('\'')
            }
            Some('/') => {
                self.advance();
                Ok('/')
            }
            Some('b') => {
                self.advance();
                Ok('\x08') // backspace
            }
            Some('f') => {
                self.advance();
                Ok('\x0C') // form feed
            }
            Some('0') => {
                self.advance();
                Ok('\0') // null
            }
            Some('u') => {
                self.advance();
                self.parse_unicode_escape(start, start_line, start_column)
            }
            Some(ch) => Err(LexError {
                message: format!("invalid escape sequence: \\{ch}"),
                span: Span::new(start, self.pos, start_line, start_column),
            }),
            None => Err(LexError {
                message: "unterminated string literal".to_string(),
                span: Span::new(start, self.pos, start_line, start_column),
            }),
        }
    }

    fn parse_unicode_escape(
        &mut self,
        start: usize,
        start_line: usize,
        start_column: usize,
    ) -> Result<char, LexError> {
        // Check for \u{...} (variable length) or \uHHHH (4 hex digits)
        if self.peek_char() == Some('{') {
            self.advance(); // consume '{'

            let mut hex = String::new();
            while let Some((_, ch)) = self.peek() {
                if ch == '}' {
                    self.advance();
                    break;
                }
                if ch.is_ascii_hexdigit() {
                    hex.push(ch);
                    self.advance();
                } else {
                    return Err(LexError {
                        message: format!("invalid character in unicode escape: {ch}"),
                        span: Span::new(start, self.pos, start_line, start_column),
                    });
                }
            }

            if hex.is_empty() {
                return Err(LexError {
                    message: "empty unicode escape".to_string(),
                    span: Span::new(start, self.pos, start_line, start_column),
                });
            }

            let code_point = u32::from_str_radix(&hex, 16).map_err(|_| LexError {
                message: format!("invalid unicode escape: \\u{{{hex}}}"),
                span: Span::new(start, self.pos, start_line, start_column),
            })?;

            char::from_u32(code_point).ok_or_else(|| LexError {
                message: format!("invalid unicode code point: U+{code_point:04X}"),
                span: Span::new(start, self.pos, start_line, start_column),
            })
        } else {
            // \uHHHH - exactly 4 hex digits
            let mut hex = String::new();
            for _ in 0..4 {
                match self.peek_char() {
                    Some(ch) if ch.is_ascii_hexdigit() => {
                        hex.push(ch);
                        self.advance();
                    }
                    _ => {
                        return Err(LexError {
                            message: "expected 4 hex digits after \\u".to_string(),
                            span: Span::new(start, self.pos, start_line, start_column),
                        });
                    }
                }
            }

            let code_unit = u16::from_str_radix(&hex, 16).map_err(|_| LexError {
                message: format!("invalid unicode escape: \\u{hex}"),
                span: Span::new(start, self.pos, start_line, start_column),
            })?;

            // This might be a surrogate, which we handle specially
            if is_high_surrogate(code_unit) || is_low_surrogate(code_unit) {
                // Rust's char cannot hold surrogate values (0xD800-0xDFFF), so we encode
                // them using Private Use Area characters that can be detected later:
                // High surrogates (0xD800-0xDBFF) -> PUA (0xE000-0xE7FF)
                // Low surrogates (0xDC00-0xDFFF) -> PUA (0xE800-0xEBFF)
                let pua_code = if is_high_surrogate(code_unit) {
                    (code_unit - 0xD800) as u32 + 0xE000
                } else {
                    (code_unit - 0xDC00) as u32 + 0xE800
                };
                Ok(char::from_u32(pua_code).unwrap())
            } else {
                char::from_u32(code_unit as u32).ok_or_else(|| LexError {
                    message: format!("invalid unicode code point: U+{code_unit:04X}"),
                    span: Span::new(start, self.pos, start_line, start_column),
                })
            }
        }
    }

    fn lex_template_string(&mut self) -> Result<TokenKind, LexError> {
        let start = self.pos;
        let start_line = self.line;
        let start_column = self.column;

        self.advance(); // consume opening `

        let mut value = String::new();
        let mut brace_depth = 0; // Track { } nesting to handle nested template strings
        let mut in_string = false;

        loop {
            match self.peek() {
                None => {
                    return Err(LexError {
                        message: "unterminated template string".to_string(),
                        span: Span::new(start, self.pos, start_line, start_column),
                    });
                }
                Some((_, '\\')) => {
                    // Handle escape sequences in template strings
                    self.advance();
                    let ch = self.parse_escape_sequence(start, start_line, start_column)?;
                    value.push(ch);
                }
                Some((_, '"')) if !in_string || brace_depth > 0 => {
                    // Track string literals inside interpolations
                    self.advance();
                    value.push('"');
                    in_string = !in_string;
                }
                Some((_, '{')) if !in_string => {
                    // Entering an interpolation
                    self.advance();
                    value.push('{');
                    brace_depth += 1;
                }
                Some((_, '}')) if !in_string && brace_depth > 0 => {
                    // Exiting an interpolation
                    self.advance();
                    value.push('}');
                    brace_depth -= 1;
                }
                Some((_, '`')) if brace_depth == 0 => {
                    // Only end template if we're not inside an interpolation
                    self.advance();
                    break;
                }
                Some((_, ch)) => {
                    self.advance();
                    value.push(ch);
                }
            }
        }

        Ok(TokenKind::TemplateStringLit(value))
    }

    fn lex_char(&mut self) -> Result<TokenKind, LexError> {
        let start = self.pos;
        let start_line = self.line;
        let start_column = self.column;

        self.advance(); // consume opening '

        let ch = match self.peek() {
            None => {
                return Err(LexError {
                    message: "unterminated character literal".to_string(),
                    span: Span::new(start, self.pos, start_line, start_column),
                });
            }
            Some((_, '\'')) => {
                return Err(LexError {
                    message: "empty character literal".to_string(),
                    span: Span::new(start, self.pos, start_line, start_column),
                });
            }
            Some((_, '\\')) => {
                self.advance();
                self.parse_escape_sequence(start, start_line, start_column)?
            }
            Some((_, c)) => {
                self.advance();
                c
            }
        };

        // Expect closing quote
        if self.peek_char() != Some('\'') {
            return Err(LexError {
                message: "unterminated character literal".to_string(),
                span: Span::new(start, self.pos, start_line, start_column),
            });
        }
        self.advance(); // consume closing '

        Ok(TokenKind::CharLit(ch))
    }
}

// Helper functions for surrogate pair handling

fn is_high_surrogate(code_unit: u16) -> bool {
    (0xD800..=0xDBFF).contains(&code_unit)
}

fn is_low_surrogate(code_unit: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&code_unit)
}

fn char_to_surrogate(c: char) -> Option<u16> {
    let code = c as u32;
    // Check for PUA-encoded surrogates:
    // High surrogates were encoded as PUA 0xE000-0xE7FF
    // Low surrogates were encoded as PUA 0xE800-0xEBFF
    if (0xE000..=0xE7FF).contains(&code) {
        // Decode high surrogate
        Some((code - 0xE000 + 0xD800) as u16)
    } else if (0xE800..=0xEBFF).contains(&code) {
        // Decode low surrogate
        Some((code - 0xE800 + 0xDC00) as u16)
    } else {
        None
    }
}

fn decode_surrogate_pair(high: u16, low: u16) -> u32 {
    let high = (high - 0xD800) as u32;
    let low = (low - 0xDC00) as u32;
    0x10000 + (high << 10) + low
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(matches!(&tokens[5].kind, TokenKind::StringLit(s) if s == "core:cli"));
        assert!(matches!(tokens[6].kind, TokenKind::Semicolon));
    }

    #[test]
    fn test_string_literal() {
        let mut lexer = Lexer::new(r#""Hello, world!""#);
        let tokens = lexer.tokenize().unwrap();

        assert!(matches!(&tokens[0].kind, TokenKind::StringLit(s) if s == "Hello, world!"));
    }

    #[test]
    fn test_comments() {
        let mut lexer = Lexer::new("// comment\nfn");
        let tokens = lexer.tokenize().unwrap();

        assert!(matches!(tokens[0].kind, TokenKind::Fn));
    }

    #[test]
    fn test_data_section_basic() {
        let source = r#"fn main() { }
__DATA__
hello world"#;
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
        let source = r#"fn main() { }
__DATA__
line 1
line 2
line 3"#;
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
        let source = r#"// some comment
fn main() { }
__DATA__
test data"#;
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
}
