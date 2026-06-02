//! Tests for resilient lexer recovery.
//!
//! The new lexer API never fails: it returns a `LexResult` bundling the token
//! stream, errors, comments, shebang, and data section. Token output is
//! best-effort even in the presence of malformed input — every byte is
//! accounted for, and downstream consumers (parser, LSP semantic tokens) can
//! continue past errors.

use wado_compiler::lexer::{LexErrorKind, lex};
use wado_compiler::token::TokenKind;

fn token_kinds(source: &str) -> Vec<TokenKind> {
    lex(source).tokens.into_iter().map(|t| t.kind).collect()
}

#[test]
fn lex_never_returns_result() {
    // Pure API smoke check: lex returns LexResult by value.
    let r = lex("fn main() {}");
    assert!(r.errors.is_empty());
    assert!(matches!(
        r.tokens.first().map(|t| &t.kind),
        Some(TokenKind::Fn)
    ));
}

#[test]
fn unexpected_char_emits_error_token_and_continues() {
    let r = lex("fn @ main()");
    assert_eq!(r.errors.len(), 1);
    assert!(matches!(
        r.errors[0].kind,
        LexErrorKind::UnexpectedChar('@')
    ));
    // The lexer must continue past the bad char and recognise `main()`.
    let kinds = r.tokens.iter().map(|t| &t.kind).collect::<Vec<_>>();
    assert!(matches!(kinds[0], TokenKind::Fn));
    assert!(matches!(kinds[1], TokenKind::Error(s) if s == "@"));
    assert!(matches!(kinds[2], TokenKind::Ident(s) if s == "main"));
    assert!(matches!(kinds[3], TokenKind::LParen));
    assert!(matches!(kinds[4], TokenKind::RParen));
}

#[test]
fn multiple_unexpected_chars_each_get_an_error() {
    let r = lex("@ @ @");
    assert_eq!(r.errors.len(), 3);
    for e in &r.errors {
        assert!(matches!(e.kind, LexErrorKind::UnexpectedChar('@')));
    }
}

#[test]
fn unterminated_string_keeps_preceding_tokens() {
    // The unterminated string consumes to EOF (multi-line strings are legal),
    // but the `let x =` tokens before it must survive.
    let r = lex("let x = \"oops");
    assert_eq!(r.errors.len(), 1);
    assert!(matches!(r.errors[0].kind, LexErrorKind::UnterminatedString));
    let kinds = token_kinds("let x = \"oops");
    assert!(matches!(kinds[0], TokenKind::Let));
    assert!(matches!(kinds[1], TokenKind::Ident(ref s) if s == "x"));
    assert!(matches!(kinds[2], TokenKind::Eq));
    assert!(matches!(kinds[3], TokenKind::StringLit(ref s) if s == "oops"));
}

#[test]
fn unterminated_char_emits_charlit_with_content() {
    let r = lex("let c = 'a");
    assert_eq!(r.errors.len(), 1);
    assert!(matches!(r.errors[0].kind, LexErrorKind::UnterminatedChar));
    assert!(
        r.tokens
            .iter()
            .any(|t| matches!(&t.kind, TokenKind::CharLit(s) if s == "a"))
    );
}

#[test]
fn empty_char_literal_emits_charlit_and_continues() {
    // `''` -> error + CharLit("") + subsequent tokens.
    let r = lex("let c = ''; fn main(){}");
    assert_eq!(r.errors.len(), 1);
    assert!(matches!(r.errors[0].kind, LexErrorKind::EmptyCharLiteral));
    // The lexer must keep parsing after `''` so `fn main(){}` is still tokenised.
    assert!(r.tokens.iter().any(|t| matches!(t.kind, TokenKind::Fn)));
    assert!(
        r.tokens
            .iter()
            .any(|t| matches!(&t.kind, TokenKind::Ident(s) if s == "main"))
    );
}

#[test]
fn missing_hex_digits_recovers_to_following_ident() {
    // `0xZ` was previously an error stopping the lexer. Now: NumberLit("0x") + error,
    // then continue and pick up `Z` as an identifier.
    let r = lex("let v = 0xZ;");
    assert_eq!(r.errors.len(), 1);
    assert!(matches!(r.errors[0].kind, LexErrorKind::MissingHexDigits));
    assert!(
        r.tokens
            .iter()
            .any(|t| matches!(&t.kind, TokenKind::Ident(s) if s == "Z"))
    );
}

#[test]
fn missing_binary_digits_recovers() {
    let r = lex("let v = 0b;");
    assert_eq!(r.errors.len(), 1);
    assert!(matches!(
        r.errors[0].kind,
        LexErrorKind::MissingBinaryDigits
    ));
    assert!(
        r.tokens
            .iter()
            .any(|t| matches!(t.kind, TokenKind::Semicolon))
    );
}

#[test]
fn missing_octal_digits_recovers() {
    let r = lex("let v = 0o;");
    assert_eq!(r.errors.len(), 1);
    assert!(matches!(r.errors[0].kind, LexErrorKind::MissingOctalDigits));
    assert!(
        r.tokens
            .iter()
            .any(|t| matches!(t.kind, TokenKind::Semicolon))
    );
}

#[test]
fn missing_exponent_digits_recovers() {
    let r = lex("let v = 1e;");
    assert_eq!(r.errors.len(), 1);
    assert!(matches!(
        r.errors[0].kind,
        LexErrorKind::MissingExponentDigits
    ));
    assert!(
        r.tokens
            .iter()
            .any(|t| matches!(t.kind, TokenKind::Semicolon))
    );
}

#[test]
fn unterminated_block_comment_does_not_lose_preceding_tokens() {
    // Block comment swallowing the rest of the file is unavoidable, but the
    // earlier tokens must survive (this was previously dropped on Err).
    let r = lex("fn main() /* oops");
    assert_eq!(r.errors.len(), 1);
    assert!(matches!(
        r.errors[0].kind,
        LexErrorKind::UnterminatedBlockComment
    ));
    let kinds = r.tokens.iter().map(|t| &t.kind).collect::<Vec<_>>();
    assert!(matches!(kinds[0], TokenKind::Fn));
    assert!(matches!(kinds[1], TokenKind::Ident(s) if s == "main"));
    assert!(matches!(kinds[2], TokenKind::LParen));
    assert!(matches!(kinds[3], TokenKind::RParen));
}

#[test]
fn unterminated_template_string_keeps_preceding_tokens() {
    let r = lex("let s = `hello {name");
    assert_eq!(r.errors.len(), 1);
    assert!(matches!(
        r.errors[0].kind,
        LexErrorKind::UnterminatedTemplateString
    ));
    assert!(matches!(r.tokens[0].kind, TokenKind::Let));
}

#[test]
fn comments_are_in_lex_result() {
    let r = lex("// hi\nfn main() {}");
    assert_eq!(r.comments.len(), 1);
    assert_eq!(r.comments[0].text, " hi");
}

#[test]
fn data_section_is_in_lex_result() {
    let source = "fn main() {}\n__DATA__\nhello";
    let r = lex(source);
    assert_eq!(r.data_section.as_deref(), Some("hello"));
}

#[test]
fn shebang_is_in_lex_result() {
    let r = lex("#!/usr/bin/env wado\nfn main() {}");
    assert_eq!(r.shebang.as_deref(), Some("#!/usr/bin/env wado"));
}

#[test]
fn error_spans_are_correct() {
    let r = lex("fn @ main()");
    let err = &r.errors[0];
    // `@` is at byte 3, line 1, column 4 (1-based).
    assert_eq!(err.span.start, 3);
    assert_eq!(err.span.end, 4);
    assert_eq!(err.span.line, 1);
    assert_eq!(err.span.column, 4);
}
