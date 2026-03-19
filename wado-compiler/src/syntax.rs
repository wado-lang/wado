//! Canonical syntax definition for the Wado language
//!
//! This module provides language-agnostic syntax information that can be
//! transformed into editor-specific formats by wado-cli.

/// Complete syntax definition for the Wado language
#[derive(Debug)]
pub struct SyntaxDefinition {
    pub name: &'static str,
    pub scope_name: &'static str,
    pub file_extensions: Vec<&'static str>,
    pub keywords: KeywordCategories,
    pub operators: OperatorCategories,
    pub builtin_types: Vec<&'static str>,
    pub constants: Vec<&'static str>,
    pub comments: CommentStyles,
}

/// Keywords categorized by their semantic role
#[derive(Debug)]
pub struct KeywordCategories {
    /// Control flow keywords: if, else, while, for, loop, break, continue, return, match
    pub control: Vec<&'static str>,
    /// Declaration keywords: fn, let, struct, enum, impl, type, use, from, pub, import, export
    pub declaration: Vec<&'static str>,
    /// Modifier keywords: as, with, mut, async, move, unique, in, of
    pub modifier: Vec<&'static str>,
    /// Other keywords: assert, effect, handler, reactive, resource, world
    pub other: Vec<&'static str>,
}

/// Operators categorized by type
#[derive(Debug)]
pub struct OperatorCategories {
    pub comparison: Vec<&'static str>,
    pub logical: Vec<&'static str>,
    pub arithmetic: Vec<&'static str>,
    pub bitwise: Vec<&'static str>,
    pub assignment: Vec<&'static str>,
    pub other: Vec<&'static str>,
}

/// Comment style definitions
#[derive(Debug)]
pub struct CommentStyles {
    pub line: &'static str,
    pub block_start: &'static str,
    pub block_end: &'static str,
    /// Shebang prefix (only valid at start of file, e.g., "#!/usr/bin/env wado")
    /// Note: `#![` is an inner attribute, not a shebang
    pub shebang: &'static str,
}

impl SyntaxDefinition {
    /// Create the canonical Wado syntax definition
    pub fn wado() -> Self {
        Self {
            name: "Wado",
            scope_name: "source.wado",
            file_extensions: vec![".wado"],
            keywords: KeywordCategories {
                // Control flow
                control: vec![
                    "if", "else", "while", "for", "loop", "break", "continue", "return", "match",
                    "matches",
                ],
                // Declarations
                declaration: vec![
                    "fn", "let", "global", "const", "struct", "enum", "variant", "impl", "trait",
                    "type", "use", "from", "pub", "import", "export", "test",
                ],
                // Modifiers
                modifier: vec!["as", "with", "mut", "async", "move", "unique", "in", "of", "stores"],
                // Other keywords
                other: vec![
                    "assert", "effect", "handler", "reactive", "resource", "world",
                ],
            },
            operators: OperatorCategories {
                comparison: vec!["==", "!=", "<=", ">=", "<", ">"],
                logical: vec!["&&", "||", "!"],
                arithmetic: vec!["+", "-", "*", "/", "%"],
                bitwise: vec!["&", "|", "^", "~", "<<", ">>"],
                assignment: vec![
                    "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "<<=", ">>=", "=",
                ],
                other: vec!["->", "=>", "::", "?"],
            },
            builtin_types: vec![
                "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "f32", "f64", "bool", "char",
                "String", "Array", "Option", "Result", "Fn",
            ],
            constants: vec!["true", "false", "null", "self"],
            comments: CommentStyles {
                line: "//",
                block_start: "/*",
                block_end: "*/",
                shebang: "#!",
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::token::TokenKind;

    #[test]
    fn test_syntax_definition() {
        let def = SyntaxDefinition::wado();
        assert_eq!(def.name, "Wado");
        assert_eq!(def.scope_name, "source.wado");
        assert!(def.keywords.control.contains(&"if"));
        assert!(def.keywords.declaration.contains(&"fn"));
    }

    /// Verify all keywords in SyntaxDefinition are recognized by the lexer
    #[test]
    fn test_syntax_keywords_match_lexer() {
        let def = SyntaxDefinition::wado();

        // Collect all keywords from SyntaxDefinition
        let all_keywords: Vec<&str> = def
            .keywords
            .control
            .iter()
            .chain(def.keywords.declaration.iter())
            .chain(def.keywords.modifier.iter())
            .chain(def.keywords.other.iter())
            .copied()
            .collect();

        // Contextual keywords: these are in SyntaxDefinition for highlighting
        // but are parsed as identifiers by the lexer and handled specially by the parser
        let contextual_keywords = ["test"];

        // Verify each keyword is lexed as a keyword token (not an identifier)
        // Skip contextual keywords which are intentionally lexed as identifiers
        for keyword in &all_keywords {
            if contextual_keywords.contains(keyword) {
                continue;
            }
            let mut lexer = Lexer::new(keyword);
            let tokens = lexer.tokenize().expect("should lex keyword");
            assert!(!tokens.is_empty(), "'{keyword}' produced no tokens");
            assert!(
                !matches!(tokens[0].kind, TokenKind::Ident(_)),
                "'{keyword}' in SyntaxDefinition is not recognized as a keyword by lexer"
            );
        }

        // Keywords that the lexer recognizes (must be kept in sync with lexer.rs)
        // This is the authoritative list from lexer.rs lex_ident_or_keyword()
        let lexer_keywords = [
            "use", "from", "as", "fn", "with", "let", "mut", "return", "if", "else", "match",
            "matches", "for", "while", "loop", "break", "continue", "in", "of", "pub", "effect",
            "handler", "reactive", "move", "unique", "struct", "enum", "variant", "type", "impl",
            "trait", "resource", "world", "async", "import", "export", "assert", "global", "const",
            "stores",
        ];

        // Contextual keywords: these are in SyntaxDefinition for highlighting
        // but are parsed as identifiers by the lexer and handled specially by the parser
        let contextual_keywords = ["test"];

        // Verify SyntaxDefinition covers all lexer keywords
        for keyword in lexer_keywords {
            assert!(
                all_keywords.contains(&keyword),
                "lexer keyword '{keyword}' is missing from SyntaxDefinition"
            );
        }

        // Verify no extra keywords in SyntaxDefinition (except contextual keywords)
        for keyword in &all_keywords {
            assert!(
                lexer_keywords.contains(keyword) || contextual_keywords.contains(keyword),
                "SyntaxDefinition keyword '{keyword}' is not in lexer or contextual_keywords"
            );
        }
    }

    /// Verify all constants in SyntaxDefinition are recognized by the lexer
    #[test]
    fn test_syntax_constants_match_lexer() {
        let def = SyntaxDefinition::wado();

        // Lexer constants (true, false, null are lexed as specific tokens)
        let lexer_constants = ["true", "false", "null"];

        for constant in lexer_constants {
            assert!(
                def.constants.contains(&constant),
                "lexer constant '{constant}' is missing from SyntaxDefinition.constants"
            );
        }

        // Note: "self" is in constants but handled specially (as identifier in most contexts)
    }
}
