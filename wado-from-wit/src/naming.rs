//! Naming convention utilities for WIT-to-Wado conversion

use heck::{ToSnakeCase, ToUpperCamelCase};

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

/// Escape Wado keywords with % prefix
#[must_use] 
pub fn escape_keyword(name: &str) -> String {
    match name {
        "type" | "fn" | "let" | "mut" | "pub" | "use" | "if" | "else" | "while" | "for"
        | "return" | "match" | "struct" | "enum" | "variant" | "flags" | "effect" | "world"
        | "resource" | "async" | "true" | "false" | "null" => {
            format!("%{name}")
        }
        _ => to_snake_case(name),
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

    #[test]
    fn test_escape_keyword() {
        assert_eq!(escape_keyword("type"), "%type");
        assert_eq!(escape_keyword("flags"), "%flags");
        assert_eq!(escape_keyword("name"), "name");
    }
}
