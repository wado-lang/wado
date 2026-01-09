// Lexer for Wado

use crate::token::{Span, Token, TokenKind};

pub struct Lexer<'a> {
    input: &'a str,
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
    pos: usize,
    line: usize,
    column: usize,
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
        }
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
                if self.peek_char() == Some('=') {
                    self.advance();
                    TokenKind::LtEq
                } else {
                    TokenKind::Lt
                }
            }
            '>' => {
                self.advance();
                if self.peek_char() == Some('=') {
                    self.advance();
                    TokenKind::GtEq
                } else {
                    TokenKind::Gt
                }
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
                TokenKind::Star
            }
            '/' => {
                self.advance();
                TokenKind::Slash
            }
            '%' => {
                self.advance();
                TokenKind::Percent
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
            "record" => TokenKind::Record,
            "enum" => TokenKind::Enum,
            "type" => TokenKind::Type,
            "impl" => TokenKind::Impl,
            "resource" => TokenKind::Resource,
            "world" => TokenKind::World,
            "async" => TokenKind::Async,
            "import" => TokenKind::Import,
            "export" => TokenKind::Export,
            "true" => TokenKind::True,
            "false" => TokenKind::False,
            _ => TokenKind::Ident(text.to_string()),
        }
    }

    fn lex_number(&mut self) -> Result<TokenKind, LexError> {
        let start = self.pos;
        let start_line = self.line;
        let start_column = self.column;

        while let Some((_, ch)) = self.peek() {
            if ch.is_ascii_digit() {
                self.advance();
            } else {
                break;
            }
        }

        // Check for float
        if self.peek_char() == Some('.') {
            let mut chars = self.chars.clone();
            chars.next();
            if let Some((_, ch)) = chars.peek() {
                if ch.is_ascii_digit() {
                    self.advance(); // consume '.'
                    while let Some((_, ch)) = self.peek() {
                        if ch.is_ascii_digit() {
                            self.advance();
                        } else {
                            break;
                        }
                    }
                    let text = &self.input[start..self.pos];
                    let value: f64 = text.parse().map_err(|_| LexError {
                        message: format!("invalid float literal: {text}"),
                        span: Span::new(start, self.pos, start_line, start_column),
                    })?;
                    return Ok(TokenKind::FloatLit(value));
                }
            }
        }

        let text = &self.input[start..self.pos];
        let value: i64 = text.parse().map_err(|_| LexError {
            message: format!("invalid integer literal: {text}"),
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
                    match self.peek_char() {
                        Some('n') => {
                            self.advance();
                            value.push('\n');
                        }
                        Some('t') => {
                            self.advance();
                            value.push('\t');
                        }
                        Some('r') => {
                            self.advance();
                            value.push('\r');
                        }
                        Some('\\') => {
                            self.advance();
                            value.push('\\');
                        }
                        Some('"') => {
                            self.advance();
                            value.push('"');
                        }
                        Some(ch) => {
                            return Err(LexError {
                                message: format!("invalid escape sequence: \\{ch}"),
                                span: Span::new(start, self.pos, start_line, start_column),
                            });
                        }
                        None => {
                            return Err(LexError {
                                message: "unterminated string literal".to_string(),
                                span: Span::new(start, self.pos, start_line, start_column),
                            });
                        }
                    }
                }
                Some((_, ch)) => {
                    self.advance();
                    value.push(ch);
                }
            }
        }

        Ok(TokenKind::StringLit(value))
    }

    fn lex_template_string(&mut self) -> Result<TokenKind, LexError> {
        // For now, treat template strings as regular strings
        // TODO: Handle interpolation properly
        let start = self.pos;
        let start_line = self.line;
        let start_column = self.column;

        self.advance(); // consume opening `

        let mut value = String::new();

        loop {
            match self.peek() {
                None => {
                    return Err(LexError {
                        message: "unterminated template string".to_string(),
                        span: Span::new(start, self.pos, start_line, start_column),
                    });
                }
                Some((_, '`')) => {
                    self.advance();
                    break;
                }
                Some((_, ch)) => {
                    self.advance();
                    value.push(ch);
                }
            }
        }

        Ok(TokenKind::StringLit(value))
    }
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
        let mut lexer = Lexer::new("use core::cli::{println};");
        let tokens = lexer.tokenize().unwrap();

        assert!(matches!(tokens[0].kind, TokenKind::Use));
        assert!(matches!(&tokens[1].kind, TokenKind::Ident(s) if s == "core"));
        assert!(matches!(tokens[2].kind, TokenKind::ColonColon));
        assert!(matches!(&tokens[3].kind, TokenKind::Ident(s) if s == "cli"));
        assert!(matches!(tokens[4].kind, TokenKind::ColonColon));
        assert!(matches!(tokens[5].kind, TokenKind::LBrace));
        assert!(matches!(&tokens[6].kind, TokenKind::Ident(s) if s == "println"));
        assert!(matches!(tokens[7].kind, TokenKind::RBrace));
        assert!(matches!(tokens[8].kind, TokenKind::Semicolon));
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
}
