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
    /// Compile-time literals introduced with `#`, e.g. `#file`, `#line`, `#include_str`.
    pub compile_time_literals: Vec<&'static str>,
    pub comments: CommentStyles,
}

/// Keywords categorized by their semantic role.
///
/// Categories are chosen so that the `TextMate` generator can map each directly
/// onto a widely-themed scope (`keyword.control`, `storage.type`, `storage.modifier`,
/// `keyword.other`). Themes that do not ship specific rules for obscure scopes
/// like `keyword.declaration` still color these correctly.
#[derive(Debug)]
pub struct KeywordCategories {
    /// Control flow keywords: if, else, while, for, loop, break, continue, return, match
    pub control: Vec<&'static str>,
    // Note: `matches` is exposed via `OperatorCategories::other` since it is a
    // binary pattern-test operator, not a control-flow construct.
    /// Storage-type keywords: items and bindings — fn, let, global, const, struct,
    /// enum, variant, flags, impl, trait, type.
    pub storage_type: Vec<&'static str>,
    /// Storage-modifier keywords: visibility and qualifiers on declarations —
    /// pub, export, mut, async, move, unique, stores.
    pub storage_modifier: Vec<&'static str>,
    /// Other keywords: everything else that lexes as a keyword but isn't a
    /// control-flow, storage-type, or storage-modifier keyword.
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
                control: vec![
                    "if", "else", "while", "for", "loop", "break", "continue", "return", "match",
                    "task", "handler", "do", "resume",
                ],
                storage_type: vec![
                    "fn",
                    "let",
                    "global",
                    "const",
                    "struct",
                    "enum",
                    "variant",
                    "flags",
                    "impl",
                    "trait",
                    "type",
                    "resource",
                    "world",
                    "effect",
                    "interface",
                ],
                storage_modifier: vec![
                    "pub", "export", "mut", "async", "move", "unique", "stores", "reactive",
                ],
                other: vec![
                    "use", "from", "import", "test", "as", "with", "in", "of", "assert",
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
                other: vec!["->", "=>", "::", "?", "..<", "..=", "..", "...", "matches"],
            },
            builtin_types: vec![
                "i8",
                "i16",
                "i32",
                "i64",
                "i128",
                "u8",
                "u16",
                "u32",
                "u64",
                "u128",
                "f32",
                "f64",
                "bool",
                "char",
                "String",
                "Array",
                "Option",
                "Result",
                "Default",
                "Eq",
                "Ord",
                "Ordering",
                "Default",
                "Display",
                "DisplayAlt",
                "Inspect",
                "InspectAlt",
                "Iterator",
                "IntoIterator",
                "Index",
                "IndexValue",
                "IndexAssign",
            ],
            constants: vec!["true", "false", "null", "self"],
            compile_time_literals: vec![
                "file",
                "line",
                "function",
                "data",
                "include_str",
                "include_bytes",
            ],
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
        assert!(def.keywords.storage_type.contains(&"fn"));
        assert!(def.keywords.storage_modifier.contains(&"pub"));
    }

    /// Verify all keywords in `SyntaxDefinition` are recognized by the lexer
    #[test]
    fn test_syntax_keywords_match_lexer() {
        let def = SyntaxDefinition::wado();

        // Collect all keywords from SyntaxDefinition
        let all_keywords: Vec<&str> = def
            .keywords
            .control
            .iter()
            .chain(def.keywords.storage_type.iter())
            .chain(def.keywords.storage_modifier.iter())
            .chain(def.keywords.other.iter())
            .copied()
            .collect();

        // Contextual keywords: these are in SyntaxDefinition for highlighting
        // but are parsed as identifiers by the lexer and handled specially by the parser
        // (e.g. `test` for test blocks, `task` for `task return` statements)
        let contextual_keywords = ["test", "task", "do", "resume"];
        // Keyword-shaped operators: lexer keywords exposed via OperatorCategories
        // rather than KeywordCategories (e.g. `matches`, the binary pattern-test op)
        let operator_keywords = ["matches"];

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
            "use",
            "from",
            "as",
            "fn",
            "with",
            "let",
            "mut",
            "return",
            "if",
            "else",
            "match",
            "matches",
            "for",
            "while",
            "loop",
            "break",
            "continue",
            "in",
            "of",
            "pub",
            "effect",
            "interface",
            "handler",
            "reactive",
            "move",
            "unique",
            "struct",
            "enum",
            "variant",
            "flags",
            "type",
            "impl",
            "trait",
            "resource",
            "world",
            "async",
            "import",
            "export",
            "assert",
            "global",
            "const",
            "stores",
        ];

        // Verify SyntaxDefinition covers all lexer keywords
        // (operator keywords like `matches` live in OperatorCategories instead)
        let def_operators = &def.operators;
        let all_op_tokens: Vec<&str> = def_operators
            .comparison
            .iter()
            .chain(def_operators.logical.iter())
            .chain(def_operators.arithmetic.iter())
            .chain(def_operators.bitwise.iter())
            .chain(def_operators.assignment.iter())
            .chain(def_operators.other.iter())
            .copied()
            .collect();
        for keyword in lexer_keywords {
            let in_keywords = all_keywords.contains(&keyword);
            let in_operators =
                operator_keywords.contains(&keyword) && all_op_tokens.contains(&keyword);
            assert!(
                in_keywords || in_operators,
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

    /// Verify all constants in `SyntaxDefinition` are recognized by the lexer
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

    /// Verify compile-time literals in `SyntaxDefinition` match the names parsed by `parser.rs`.
    #[test]
    fn test_syntax_compile_time_literals_match_parser() {
        let def = SyntaxDefinition::wado();

        // Authoritative list from parser.rs `parse_expr` hash-literal branch.
        let parser_literals = [
            "file",
            "line",
            "function",
            "data",
            "include_str",
            "include_bytes",
        ];

        for lit in parser_literals {
            assert!(
                def.compile_time_literals.contains(&lit),
                "parser compile-time literal '#{lit}' is missing from SyntaxDefinition.compile_time_literals"
            );
        }

        for lit in &def.compile_time_literals {
            assert!(
                parser_literals.contains(lit),
                "SyntaxDefinition compile-time literal '#{lit}' is not in parser_literals"
            );
        }
    }
}
