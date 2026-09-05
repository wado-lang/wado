//! Naming convention utilities for WIT-to-Wado conversion

use heck::{ToKebabCase, ToSnakeCase, ToUpperCamelCase};

/// Convert WIT kebab-case to Wado `snake_case` for function/field names
#[must_use]
pub fn to_snake_case(name: &str) -> String {
    name.to_snake_case()
}

/// Convert WIT kebab-case to Wado `UpperCamelCase` for type names
#[must_use]
pub fn to_upper_camel_case(name: &str) -> String {
    name.to_upper_camel_case()
}

/// Convert a `WebIDL` identifier to the kebab-case a CM member name takes
#[must_use]
pub fn to_kebab_case(name: &str) -> String {
    name.to_kebab_case()
}

/// The Wado keywords, plus `self`, which no parameter or method may be named.
const RESERVED: &[&str] = &[
    "if",
    "else",
    "while",
    "for",
    "loop",
    "break",
    "continue",
    "return",
    "match",
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
    "extends",
    "world",
    "effect",
    "interface",
    "pub",
    "internal",
    "export",
    "mut",
    "async",
    "unique",
    "stores",
    "reactive",
    "use",
    "from",
    "import",
    "as",
    "with",
    "in",
    "of",
    "assert",
    "true",
    "false",
    "null",
    "matches",
    "self",
];

/// Convert a `WebIDL` identifier to a Wado `snake_case` name that is not a
/// keyword: `type` becomes `type_`.
#[must_use]
pub fn to_wado_identifier(name: &str) -> String {
    let snake = name.to_snake_case();
    if RESERVED.contains(&snake.as_str()) {
        format!("{snake}_")
    } else {
        snake
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_snake_case() {
        assert_eq!(to_snake_case("write-via-stream"), "write_via_stream");
        assert_eq!(to_snake_case("get-environment"), "get_environment");
        assert_eq!(to_snake_case("getArguments"), "get_arguments");
    }

    #[test]
    fn a_webidl_identifier_avoids_the_keywords() {
        assert_eq!(to_wado_identifier("type"), "type_");
        assert_eq!(to_wado_identifier("self"), "self_");
        assert_eq!(to_wado_identifier("innerHTML"), "inner_html");
        assert_eq!(to_kebab_case("HTMLInputElement"), "html-input-element");
        assert_eq!(to_kebab_case("setHTMLUnsafe"), "set-html-unsafe");
    }

    #[test]
    fn test_to_upper_camel_case() {
        assert_eq!(to_upper_camel_case("error-code"), "ErrorCode");
        assert_eq!(to_upper_camel_case("terminal-input"), "TerminalInput");
        assert_eq!(to_upper_camel_case("stdout"), "Stdout");
    }
}
