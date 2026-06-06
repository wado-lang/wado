//! Canonical syntax definition for the Wado language
//!
//! This module is the **single source of truth** for which words are
//! keywords, which symbols are operators, and how each is categorized. The
//! lexer (`TokenKind::from_keyword`), the token→text mapping
//! (`TokenKind::as_keyword_str` / `operator_str`), the LSP highlighter
//! (`TokenKind::operator_category`), and the editor-grammar generator
//! (`SyntaxDefinition::wado`, consumed by wado-cli) all derive from the
//! [`KEYWORDS`] / [`OPERATORS`] tables below, so they cannot drift apart.

use crate::token::TokenKind;

/// Editorial role of a keyword, chosen so the `TextMate` generator can map
/// each onto a widely-themed scope. See [`KeywordCategories`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeywordCategory {
    Control,
    StorageType,
    StorageModifier,
    Other,
    /// `true` / `false` / `null` / `self` — rendered as constants, not keywords.
    Constant,
    /// Keyword-shaped binary operator (`matches`): lexes as a keyword token
    /// but highlights as an operator.
    Operator,
}

/// Category of an operator/punctuation token. See [`OperatorCategories`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorCategory {
    Comparison,
    Logical,
    Arithmetic,
    Bitwise,
    Assignment,
    Other,
}

/// Generate the keyword registry and the `TokenKind` ↔ text mappings from one
/// table, so the lexer, the reverse mapping, and the grammar stay in sync.
macro_rules! keyword_registry {
    ( $( $text:literal => $variant:ident : $cat:ident ),+ $(,)? ) => {
        /// Every real keyword token paired with its editorial category. The
        /// authoritative list — adding a keyword here wires the lexer, the
        /// reverse text mapping, and the grammar in one edit.
        pub const KEYWORDS: &[(&str, KeywordCategory)] = &[
            $( ($text, KeywordCategory::$cat) ),+
        ];

        impl TokenKind {
            /// Map source text to its keyword token, if it is one. Used by the
            /// lexer's identifier/keyword disambiguation.
            #[must_use]
            pub(crate) fn from_keyword(text: &str) -> Option<TokenKind> {
                match text {
                    $( $text => Some(TokenKind::$variant), )+
                    _ => None,
                }
            }

            /// The keyword's source spelling, if this token is a keyword
            /// (including `true` / `false` / `null` and the `matches` operator
            /// keyword). Used to allow keywords as field names and to highlight.
            #[must_use]
            pub fn as_keyword_str(&self) -> Option<&'static str> {
                match self {
                    $( TokenKind::$variant => Some($text), )+
                    _ => None,
                }
            }
        }
    };
}

/// Generate the operator registry and the `TokenKind` → text/category
/// mappings from one table.
macro_rules! operator_registry {
    ( $( $text:literal => $variant:ident : $cat:ident ),+ $(,)? ) => {
        /// Every operator/punctuation token paired with its category. The
        /// lexer's maximal-munch scanner produces these tokens; a test pins
        /// that each spelling lexes back to the listed variant.
        pub const OPERATORS: &[(&str, OperatorCategory)] = &[
            $( ($text, OperatorCategory::$cat) ),+
        ];

        impl TokenKind {
            /// The operator's source spelling, if this token is an operator.
            #[must_use]
            pub fn operator_str(&self) -> Option<&'static str> {
                match self {
                    $( TokenKind::$variant => Some($text), )+
                    _ => None,
                }
            }

            /// The operator's category, if this token is an operator.
            #[must_use]
            pub fn operator_category(&self) -> Option<OperatorCategory> {
                match self {
                    $( TokenKind::$variant => Some(OperatorCategory::$cat), )+
                    _ => None,
                }
            }
        }
    };
}

keyword_registry! {
    // Control flow.
    "if" => If : Control,
    "else" => Else : Control,
    "while" => While : Control,
    "for" => For : Control,
    "loop" => Loop : Control,
    "break" => Break : Control,
    "continue" => Continue : Control,
    "return" => Return : Control,
    "match" => Match : Control,
    // Storage types: items and bindings.
    "fn" => Fn : StorageType,
    "let" => Let : StorageType,
    "global" => Global : StorageType,
    "const" => Const : StorageType,
    "struct" => Struct : StorageType,
    "enum" => Enum : StorageType,
    "variant" => Variant : StorageType,
    "flags" => Flags : StorageType,
    "impl" => Impl : StorageType,
    "trait" => Trait : StorageType,
    "type" => Type : StorageType,
    "resource" => Resource : StorageType,
    "world" => World : StorageType,
    "effect" => Effect : StorageType,
    "interface" => Interface : StorageType,
    // Storage modifiers: visibility and qualifiers.
    "pub" => Pub : StorageModifier,
    "export" => Export : StorageModifier,
    "mut" => Mut : StorageModifier,
    "async" => Async : StorageModifier,
    "unique" => Unique : StorageModifier,
    "stores" => Stores : StorageModifier,
    "reactive" => Reactive : StorageModifier,
    // Other keywords.
    "use" => Use : Other,
    "from" => From : Other,
    "import" => Import : Other,
    "as" => As : Other,
    "with" => With : Other,
    "in" => In : Other,
    "of" => Of : Other,
    "assert" => Assert : Other,
    // Constants.
    "true" => True : Constant,
    "false" => False : Constant,
    "null" => Null : Constant,
    // Operator keyword.
    "matches" => Matches : Operator,
}

operator_registry! {
    // Comparison.
    "==" => EqEq : Comparison,
    "!=" => NotEq : Comparison,
    "<=" => LtEq : Comparison,
    ">=" => GtEq : Comparison,
    "<" => Lt : Comparison,
    ">" => Gt : Comparison,
    // Logical.
    "&&" => And : Logical,
    "||" => Or : Logical,
    "!" => Not : Logical,
    // Arithmetic.
    "+" => Plus : Arithmetic,
    "-" => Minus : Arithmetic,
    "*" => Star : Arithmetic,
    "/" => Slash : Arithmetic,
    "%" => Percent : Arithmetic,
    // Bitwise.
    "&" => Ampersand : Bitwise,
    "|" => Pipe : Bitwise,
    "^" => Caret : Bitwise,
    "~" => Tilde : Bitwise,
    "<<" => LtLt : Bitwise,
    ">>" => GtGt : Bitwise,
    // Assignment.
    "+=" => PlusEq : Assignment,
    "-=" => MinusEq : Assignment,
    "*=" => StarEq : Assignment,
    "/=" => SlashEq : Assignment,
    "%=" => PercentEq : Assignment,
    "&=" => AmpEq : Assignment,
    "|=" => PipeEq : Assignment,
    "^=" => CaretEq : Assignment,
    "<<=" => ShlEq : Assignment,
    ">>=" => ShrEq : Assignment,
    "=" => Eq : Assignment,
    // Other.
    "->" => Arrow : Other,
    "=>" => FatArrow : Other,
    "::" => ColonColon : Other,
    "?" => Question : Other,
    "..<" => DotDotLt : Other,
    "..=" => DotDotEq : Other,
    ".." => DotDot : Other,
    "..." => DotDotDot : Other,
}

/// Contextual keywords: lexed as identifiers (the parser recognises them in
/// position) but highlighted as keywords. They have no `TokenKind`, so they
/// live outside [`KEYWORDS`] and feed only the grammar.
pub const CONTEXTUAL_KEYWORDS: &[(&str, KeywordCategory)] = &[
    ("task", KeywordCategory::Control),
    ("do", KeywordCategory::Control),
    ("resume", KeywordCategory::Control),
    ("test", KeywordCategory::Other),
    ("self", KeywordCategory::Constant),
];

/// Spellings of every keyword (real + contextual) in the given category.
fn keywords_in(category: KeywordCategory) -> Vec<&'static str> {
    KEYWORDS
        .iter()
        .chain(CONTEXTUAL_KEYWORDS.iter())
        .filter(|(_, c)| *c == category)
        .map(|(text, _)| *text)
        .collect()
}

/// Spellings of every operator in the given category.
fn operators_in(category: OperatorCategory) -> Vec<&'static str> {
    OPERATORS
        .iter()
        .filter(|(_, c)| *c == category)
        .map(|(text, _)| *text)
        .collect()
}

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
                control: keywords_in(KeywordCategory::Control),
                storage_type: keywords_in(KeywordCategory::StorageType),
                storage_modifier: keywords_in(KeywordCategory::StorageModifier),
                other: keywords_in(KeywordCategory::Other),
            },
            operators: OperatorCategories {
                comparison: operators_in(OperatorCategory::Comparison),
                logical: operators_in(OperatorCategory::Logical),
                arithmetic: operators_in(OperatorCategory::Arithmetic),
                bitwise: operators_in(OperatorCategory::Bitwise),
                assignment: operators_in(OperatorCategory::Assignment),
                // `matches` is a keyword token but highlights as an operator.
                other: {
                    let mut ops = operators_in(OperatorCategory::Other);
                    ops.extend(keywords_in(KeywordCategory::Operator));
                    ops
                },
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
                "List",
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
            constants: keywords_in(KeywordCategory::Constant),
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

    /// Every real keyword round-trips text → token → text and lexes as a
    /// keyword token (not an identifier). Pins the generated `from_keyword` /
    /// `as_keyword_str` against the actual lexer.
    #[test]
    fn keywords_round_trip_through_lexer() {
        for (text, _cat) in KEYWORDS {
            let token = TokenKind::from_keyword(text)
                .unwrap_or_else(|| panic!("'{text}' is missing from from_keyword"));
            assert_eq!(
                token.as_keyword_str(),
                Some(*text),
                "'{text}' does not round-trip through as_keyword_str"
            );
            let r = crate::lexer::lex(text);
            assert!(
                r.errors.is_empty(),
                "lexer error on '{text}': {:?}",
                r.errors
            );
            assert!(
                !matches!(r.tokens[0].kind, TokenKind::Ident(_)),
                "'{text}' should lex as a keyword token, not an identifier"
            );
        }
    }

    /// Contextual keywords are deliberately not lexer keywords: they lex as
    /// identifiers and the parser recognises them positionally.
    #[test]
    fn contextual_keywords_lex_as_identifiers() {
        for (text, _cat) in CONTEXTUAL_KEYWORDS {
            assert!(
                TokenKind::from_keyword(text).is_none(),
                "contextual keyword '{text}' must not be a lexer keyword"
            );
            let r = crate::lexer::lex(text);
            assert!(
                matches!(r.tokens[0].kind, TokenKind::Ident(_)),
                "contextual keyword '{text}' should lex as an identifier"
            );
        }
    }

    /// Every operator round-trips: the lexer emits a token whose
    /// `operator_str` / `operator_category` agree with the registry.
    #[test]
    fn operators_round_trip_through_lexer() {
        for (text, cat) in OPERATORS {
            let r = crate::lexer::lex(text);
            let kind = &r.tokens[0].kind;
            assert_eq!(
                kind.operator_str(),
                Some(*text),
                "'{text}' does not round-trip through operator_str"
            );
            assert_eq!(
                kind.operator_category(),
                Some(*cat),
                "'{text}' operator_category mismatch"
            );
        }
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
