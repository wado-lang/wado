//! Naming convention utilities for WIT-to-Wado conversion

use heck::{ToSnakeCase, ToUpperCamelCase};

/// Wado reserved keywords that cannot be used as identifiers
/// Note: `self` is valid as a method receiver, and `flags`/`type` are contextual keywords
/// (only keywords at declaration start, allowed as identifiers elsewhere)
const RESERVED_KEYWORDS: &[&str] = &[
    "as", "async", "break", "const", "continue", "effect", "else", "enum", "export", "false", "fn",
    "for", "global", "if", "impl", "import", "in", "let", "loop", "match", "mod", "mut", "null",
    "of", "pub", "reactive", "resource", "return", "struct", "test", "trait", "true", "use",
    "variant", "while", "with", "world",
];

/// Convert WIT kebab-case to Wado `snake_case` for function/field names
#[must_use]
pub fn to_snake_case(name: &str) -> String {
    let result = name.to_snake_case();
    escape_reserved_keyword(&result)
}

/// Convert WIT kebab-case to Wado `UpperCamelCase` for type names
#[must_use]
pub fn to_upper_camel_case(name: &str) -> String {
    name.to_upper_camel_case()
}

/// Escape reserved keywords by appending an underscore
fn escape_reserved_keyword(name: &str) -> String {
    if RESERVED_KEYWORDS.contains(&name) {
        format!("{name}_")
    } else {
        name.to_string()
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
    fn test_to_upper_camel_case() {
        assert_eq!(to_upper_camel_case("error-code"), "ErrorCode");
        assert_eq!(to_upper_camel_case("terminal-input"), "TerminalInput");
        assert_eq!(to_upper_camel_case("stdout"), "Stdout");
    }
}
