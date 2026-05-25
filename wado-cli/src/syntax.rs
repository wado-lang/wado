//! Syntax definition generator subcommand
//!
//! Transforms canonical syntax definitions from wado-compiler into
//! editor-specific formats (`TextMate` grammar, VS Code language configuration).

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write};

use lexopt::Arg::Value;
use lexopt::Parser;
use serde_json::json;

use wado_compiler::syntax::SyntaxDefinition;

use crate::args::{self, CliExit};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SyntaxFormat {
    TmLanguage,
    LanguageConfig,
}

impl SyntaxFormat {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "tmLanguage" | "tmlanguage" | "textmate" => Some(Self::TmLanguage),
            "language-config" | "languageConfig" => Some(Self::LanguageConfig),
            _ => None,
        }
    }
}

pub struct SyntaxOptions {
    pub format: SyntaxFormat,
    pub output: Option<String>,
}

#[derive(Clone, Copy)]
enum Opt {
    Format,
    Output,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[Self::Format, Self::Output, Self::Help];

    const fn spec(self) -> args::OptSpec {
        match self {
            Self::Format => args::OptSpec {
                long: Some("format"),
                short: None,
                value: Some("<fmt>"),
                desc: "Output format: tmLanguage, language-config (default: tmLanguage)",
            },
            Self::Output => args::OptSpec {
                long: Some("output"),
                short: Some('o'),
                value: Some("<file>"),
                desc: "Output file (default: stdout)",
            },
            Self::Help => args::HELP_SPEC,
        }
    }
}

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado syntax [options]").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Generate syntax definition files for editor integrations."
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    write!(buf, "{}", args::format_opts_help(Opt::ALL, |o| o.spec())).unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Examples:").unwrap();
    writeln!(
        buf,
        "  wado syntax --format tmLanguage -o wado.tmLanguage.json"
    )
    .unwrap();
    writeln!(
        buf,
        "  wado syntax --format language-config -o language-configuration.json"
    )
    .unwrap();
    buf
}

/// Parse command-line arguments for the `syntax` subcommand.
///
/// # Errors
///
/// Returns an error if the arguments are invalid or required arguments are missing.
pub fn parse_args(mut parser: Parser) -> Result<SyntaxOptions, CliExit> {
    let usage = format_usage();
    let mut format = SyntaxFormat::TmLanguage;
    let mut output = None;

    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Format => {
                    let fmt_str = args::require_string(&mut parser)?;
                    format = SyntaxFormat::from_str(&fmt_str)
                        .ok_or_else(|| CliExit::error(format!("unknown format: {fmt_str}")))?;
                }
                Opt::Output => {
                    output = Some(args::require_string(&mut parser)?);
                }
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(_) = arg {
            // No positional arguments expected
            return Err(CliExit::error("unexpected argument"));
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    Ok(SyntaxOptions { format, output })
}

pub fn run(opts: SyntaxOptions) -> Result<(), CliExit> {
    let def = SyntaxDefinition::wado();

    let output_str = match opts.format {
        SyntaxFormat::TmLanguage => {
            let grammar = generate_textmate_grammar(&def);
            serde_json::to_string_pretty(&grammar).expect("failed to serialize grammar")
        }
        SyntaxFormat::LanguageConfig => {
            let config = generate_language_configuration(&def);
            serde_json::to_string_pretty(&config).expect("failed to serialize language config")
        }
    };

    if let Some(path) = opts.output {
        fs::write(&path, &output_str)
            .map_err(|e| CliExit::error(format!("failed to write to {path}: {e}")))?;
        eprintln!("Generated: {path}");
    } else {
        io::stdout()
            .write_all(output_str.as_bytes())
            .expect("failed to write to stdout");
        println!();
    }
    Ok(())
}

/// Generate `TextMate` grammar JSON for VS Code
fn generate_textmate_grammar(def: &SyntaxDefinition) -> serde_json::Value {
    // Helper to create keyword regex pattern
    let keyword_pattern = |words: &[&str]| -> String { format!("\\b({})\\b", words.join("|")) };

    // Helper to escape regex special characters
    let escape_regex = |s: &str| -> String {
        let mut result = String::new();
        for c in s.chars() {
            match c {
                '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^'
                | '$' => {
                    result.push('\\');
                    result.push(c);
                }
                _ => result.push(c),
            }
        }
        result
    };

    // Build an alternation for a single operator category.
    // Operators are sorted by length descending so multi-char tokens win over their prefixes
    // (e.g. `..=` matches before `..` before `.`). Word-shaped operators (like `matches`) are
    // wrapped in `\b…\b` so they don't bleed into surrounding identifiers.
    let operator_alternation = |ops: &[&str]| -> String {
        let mut sorted: Vec<&str> = ops.to_vec();
        sorted.sort_by_key(|op| std::cmp::Reverse(op.len()));
        sorted
            .iter()
            .map(|op| {
                if op.chars().all(|c| c.is_ascii_alphabetic() || c == '_') {
                    format!("\\b{op}\\b")
                } else {
                    escape_regex(op)
                }
            })
            .collect::<Vec<_>>()
            .join("|")
    };

    json!({
        "$schema": "https://raw.githubusercontent.com/martinring/tmlanguage/master/tmlanguage.json",
        "name": def.name,
        "scopeName": def.scope_name,
        "patterns": [
            { "include": "#shebang" },
            { "include": "#data-section" },
            { "include": "#comments" },
            { "include": "#attributes" },
            { "include": "#compile-time-literals" },
            { "include": "#strings" },
            { "include": "#characters" },
            { "include": "#numbers" },
            { "include": "#keywords" },
            { "include": "#types" },
            { "include": "#functions" },
            { "include": "#operators" },
            { "include": "#punctuation" }
        ],
        "repository": {
            "shebang": {
                "name": "comment.line.shebang.wado",
                "match": "\\A#!(?!\\[).*$"
            },
            "data-section": {
                "begin": "^__DATA__\\s*$",
                "end": "\\z",
                "beginCaptures": {
                    "0": { "name": "keyword.control.data-section.wado" }
                },
                "patterns": [
                    {
                        "comment": "JSON content - triggered when line starts with { or [",
                        "begin": "^\\s*(?=[\\{\\[])",
                        "end": "\\z",
                        "contentName": "meta.embedded.block.json",
                        "patterns": [
                            { "include": "source.json" }
                        ]
                    },
                    {
                        "comment": "Plain text as comment",
                        "name": "comment.block.data-section.wado",
                        "match": ".+"
                    }
                ]
            },
            "comments": {
                "patterns": [
                    {
                        "name": "comment.line.double-slash.wado",
                        "match": "//.*$"
                    },
                    {
                        "name": "comment.block.wado",
                        "begin": "/\\*",
                        "end": "\\*/",
                        "patterns": [
                            { "include": "#comments" }
                        ]
                    }
                ]
            },
            "attributes": {
                "patterns": [
                    {
                        "name": "meta.attribute.inner.wado",
                        "begin": "#!\\[",
                        "end": "\\]",
                        "beginCaptures": {
                            "0": { "name": "punctuation.definition.attribute.wado" }
                        },
                        "endCaptures": {
                            "0": { "name": "punctuation.definition.attribute.wado" }
                        },
                        "patterns": [
                            {
                                "name": "entity.name.attribute.wado",
                                "match": "\\b(no_prelude)\\b"
                            },
                            { "include": "#strings" }
                        ]
                    },
                    {
                        "name": "meta.attribute.outer.wado",
                        "begin": "#\\[",
                        "end": "\\]",
                        "beginCaptures": {
                            "0": { "name": "punctuation.definition.attribute.wado" }
                        },
                        "endCaptures": {
                            "0": { "name": "punctuation.definition.attribute.wado" }
                        },
                        "patterns": [
                            {
                                "name": "entity.name.attribute.wado",
                                "match": "\\b(hidden)\\b"
                            },
                            { "include": "#strings" }
                        ]
                    }
                ]
            },
            "strings": {
                "patterns": [
                    {
                        "name": "string.quoted.double.wado",
                        "begin": "\"",
                        "end": "\"",
                        "patterns": [
                            { "include": "#string-escapes" }
                        ]
                    },
                    {
                        "name": "string.interpolated.wado",
                        "begin": "`",
                        "end": "`",
                        "patterns": [
                            { "include": "#string-escapes" },
                            {
                                "name": "meta.interpolation.wado",
                                "begin": "\\{",
                                "end": "\\}",
                                "beginCaptures": {
                                    "0": { "name": "punctuation.definition.interpolation.begin.wado" }
                                },
                                "endCaptures": {
                                    "0": { "name": "punctuation.definition.interpolation.end.wado" }
                                },
                                "patterns": [
                                    { "include": "$self" }
                                ]
                            }
                        ]
                    }
                ]
            },
            "string-escapes": {
                "patterns": [
                    {
                        "name": "constant.character.escape.wado",
                        "match": "\\\\[nrt\\\\\"'0`{}]"
                    },
                    {
                        "name": "constant.character.escape.unicode.wado",
                        "match": "\\\\u[0-9a-fA-F]{4}"
                    },
                    {
                        "name": "constant.character.escape.unicode.wado",
                        "match": "\\\\u\\{[0-9a-fA-F]+\\}"
                    }
                ]
            },
            "characters": {
                "patterns": [
                    {
                        "name": "string.quoted.single.wado",
                        "match": "'(?:[^'\\\\]|\\\\[nrt\\\\\"'0]|\\\\u[0-9a-fA-F]{4}|\\\\u\\{[0-9a-fA-F]+\\})'"
                    }
                ]
            },
            "numbers": {
                "patterns": [
                    {
                        "name": "constant.numeric.hex.wado",
                        "match": "\\b0x[0-9a-fA-F][0-9a-fA-F_]*\\b"
                    },
                    {
                        "name": "constant.numeric.binary.wado",
                        "match": "\\b0b[01][01_]*\\b"
                    },
                    {
                        "name": "constant.numeric.octal.wado",
                        "match": "\\b0o[0-7][0-7_]*\\b"
                    },
                    {
                        "name": "constant.numeric.float.wado",
                        "match": "\\b[0-9][0-9_]*\\.[0-9][0-9_]*(?:[eE][+-]?[0-9][0-9_]*)?\\b"
                    },
                    {
                        "name": "constant.numeric.integer.wado",
                        "match": "\\b[0-9][0-9_]*\\b"
                    }
                ]
            },
            "keywords": {
                "patterns": [
                    {
                        "name": "keyword.control.wado",
                        "match": keyword_pattern(&def.keywords.control)
                    },
                    // Dual-scope via capture 1: the outer `name` gives the semantic scope
                    // (`storage.type`) while the capture layers a `keyword.control.*` scope
                    // so themes that only style `keyword.control` (minimalist themes) still
                    // color declaration keywords.
                    {
                        "name": "storage.type.wado",
                        "match": keyword_pattern(&def.keywords.storage_type),
                        "captures": {
                            "1": { "name": "keyword.control.declaration.wado" }
                        }
                    },
                    {
                        "name": "storage.modifier.wado",
                        "match": keyword_pattern(&def.keywords.storage_modifier),
                        "captures": {
                            "1": { "name": "keyword.control.modifier.wado" }
                        }
                    },
                    {
                        "name": "keyword.other.wado",
                        "match": keyword_pattern(&def.keywords.other),
                        "captures": {
                            "1": { "name": "keyword.control.other.wado" }
                        }
                    },
                    {
                        "name": "constant.language.wado",
                        "match": keyword_pattern(&def.constants)
                    }
                ]
            },
            "types": {
                "patterns": [
                    {
                        "name": "storage.type.primitive.wado",
                        "match": "\\b(i8|i16|i32|i64|i128|u8|u16|u32|u64|u128|f32|f64|bool|char)\\b"
                    },
                    {
                        "name": "storage.type.builtin.wado",
                        "match": "\\b(String|Array|Option|Result|Dict|Fn)\\b"
                    },
                    {
                        "name": "entity.name.type.wado",
                        "match": "\\b[A-Z][a-zA-Z0-9_]*\\b"
                    }
                ]
            },
            "functions": {
                "patterns": [
                    {
                        "name": "meta.function.definition.wado",
                        "match": "\\b(fn)\\s+([a-zA-Z_][a-zA-Z0-9_]*)\\s*(?:<|\\()",
                        "captures": {
                            "1": { "name": "storage.type.wado" },
                            "2": { "name": "entity.name.function.wado" }
                        }
                    },
                    {
                        "name": "meta.function.call.wado",
                        "match": "\\b([a-zA-Z_][a-zA-Z0-9_]*)\\s*(?:::<[^>]*>)?\\s*\\(",
                        "captures": {
                            "1": { "name": "entity.name.function.call.wado" }
                        }
                    },
                    {
                        "name": "meta.method.call.wado",
                        "match": "\\.\\s*([a-zA-Z_][a-zA-Z0-9_]*)\\s*(?:::<[^>]*>)?\\s*\\(",
                        "captures": {
                            "1": { "name": "entity.name.function.call.wado" }
                        }
                    }
                ]
            },
            "operators": {
                "patterns": [
                    // `other` is emitted first so multi-char tokens like `..<`, `..=`, `->` win
                    // before the single-char comparison/assignment patterns match their prefix.
                    {
                        "name": "keyword.operator.other.wado",
                        "match": operator_alternation(&def.operators.other)
                    },
                    {
                        "name": "keyword.operator.comparison.wado",
                        "match": operator_alternation(&def.operators.comparison)
                    },
                    {
                        "name": "keyword.operator.logical.wado",
                        "match": operator_alternation(&def.operators.logical)
                    },
                    {
                        "name": "keyword.operator.arithmetic.wado",
                        "match": operator_alternation(&def.operators.arithmetic)
                    },
                    {
                        "name": "keyword.operator.bitwise.wado",
                        "match": operator_alternation(&def.operators.bitwise)
                    },
                    {
                        "name": "keyword.operator.assignment.wado",
                        "match": operator_alternation(&def.operators.assignment)
                    }
                ]
            },
            "compile-time-literals": {
                "patterns": [
                    {
                        "name": "constant.language.compile-time.wado",
                        "match": format!("#({})\\b", def.compile_time_literals.join("|"))
                    }
                ]
            },
            "punctuation": {
                "patterns": [
                    {
                        "name": "punctuation.separator.comma.wado",
                        "match": ","
                    },
                    {
                        "name": "punctuation.terminator.semicolon.wado",
                        "match": ";"
                    },
                    {
                        "name": "punctuation.separator.colon.wado",
                        "match": ":"
                    },
                    {
                        "name": "punctuation.separator.period.wado",
                        "match": "\\."
                    },
                    {
                        "name": "punctuation.section.braces.begin.wado",
                        "match": "\\{"
                    },
                    {
                        "name": "punctuation.section.braces.end.wado",
                        "match": "\\}"
                    },
                    {
                        "name": "punctuation.section.brackets.begin.wado",
                        "match": "\\["
                    },
                    {
                        "name": "punctuation.section.brackets.end.wado",
                        "match": "\\]"
                    },
                    {
                        "name": "punctuation.section.parens.begin.wado",
                        "match": "\\("
                    },
                    {
                        "name": "punctuation.section.parens.end.wado",
                        "match": "\\)"
                    },
                    {
                        "name": "punctuation.section.angles.begin.wado",
                        "match": "<"
                    },
                    {
                        "name": "punctuation.section.angles.end.wado",
                        "match": ">"
                    },
                    {
                        "name": "punctuation.definition.closure.wado",
                        "match": "\\|"
                    }
                ]
            }
        }
    })
}

/// Generate VS Code language-configuration.json
fn generate_language_configuration(def: &SyntaxDefinition) -> serde_json::Value {
    json!({
        "$schema": "https://raw.githubusercontent.com/SchemaStore/schemastore/master/src/schemas/json/language-configuration.json",
        "comments": {
            "lineComment": def.comments.line,
            "blockComment": [def.comments.block_start, def.comments.block_end]
        },
        "brackets": [
            ["{", "}"],
            ["[", "]"],
            ["(", ")"],
            ["<", ">"]
        ],
        "autoClosingPairs": [
            { "open": "{", "close": "}" },
            { "open": "[", "close": "]" },
            { "open": "(", "close": ")" },
            { "open": "<", "close": ">", "notIn": ["string", "comment"] },
            { "open": "\"", "close": "\"", "notIn": ["string", "comment"] },
            { "open": "'", "close": "'", "notIn": ["string", "comment"] },
            { "open": "`", "close": "`", "notIn": ["string", "comment"] },
            { "open": "/*", "close": " */", "notIn": ["string"] }
        ],
        "surroundingPairs": [
            ["{", "}"],
            ["[", "]"],
            ["(", ")"],
            ["<", ">"],
            ["\"", "\""],
            ["'", "'"],
            ["`", "`"]
        ],
        "folding": {
            "markers": {
                "start": "^\\s*//\\s*#?region\\b",
                "end": "^\\s*//\\s*#?endregion\\b"
            }
        },
        "wordPattern": "[a-zA-Z_][a-zA-Z0-9_]*",
        "indentationRules": {
            "increaseIndentPattern": "^.*\\{[^}\"']*$|^.*\\([^)\"']*$|^.*\\[[^\\]\"']*$",
            "decreaseIndentPattern": "^\\s*[}\\])]"
        },
        "onEnterRules": [
            {
                "beforeText": "^\\s*///.*$",
                "action": { "indent": "none", "appendText": "/// " }
            },
            {
                "beforeText": "\\{\\s*$",
                "afterText": "^\\s*\\}",
                "action": { "indent": "indentOutdent" }
            },
            {
                "beforeText": "\\{\\s*$",
                "action": { "indent": "indent" }
            },
            {
                "beforeText": "\\[\\s*$",
                "afterText": "^\\s*\\]",
                "action": { "indent": "indentOutdent" }
            },
            {
                "beforeText": "\\(\\s*$",
                "afterText": "^\\s*\\)",
                "action": { "indent": "indentOutdent" }
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonschema::validator_for;

    /// `TextMate` grammar JSON schema
    /// From: <https://raw.githubusercontent.com/martinring/tmlanguage/master/tmlanguage.json>
    fn textmate_grammar_schema() -> serde_json::Value {
        serde_json::from_str(include_str!("../schemas/tmlanguage.schema.json"))
            .expect("failed to parse tmlanguage schema")
    }

    /// VS Code language-configuration.json schema
    /// From: <https://raw.githubusercontent.com/SchemaStore/schemastore/master/src/schemas/json/language-configuration.json>
    fn language_configuration_schema() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../schemas/language-configuration.schema.json"
        ))
        .expect("failed to parse language-configuration schema")
    }

    #[test]
    fn test_generate_textmate_grammar_schema() {
        let def = SyntaxDefinition::wado();
        let grammar = generate_textmate_grammar(&def);

        let schema = textmate_grammar_schema();
        let validator = validator_for(&schema).expect("failed to compile schema");
        let errors: Vec<String> = validator
            .iter_errors(&grammar)
            .map(|e| e.to_string())
            .collect();

        assert!(
            errors.is_empty(),
            "TextMate grammar failed schema validation:\n{}",
            errors.join("\n")
        );
    }

    #[test]
    fn test_generate_language_configuration_schema() {
        let def = SyntaxDefinition::wado();
        let config = generate_language_configuration(&def);

        let schema = language_configuration_schema();
        let validator = validator_for(&schema).expect("failed to compile schema");
        let errors: Vec<String> = validator
            .iter_errors(&config)
            .map(|e| e.to_string())
            .collect();

        assert!(
            errors.is_empty(),
            "Language configuration failed schema validation:\n{}",
            errors.join("\n")
        );
    }
}
