//! The names a WIT or `WebIDL` identifier takes in Wado and at the CM boundary.

use heck::{ToKebabCase, ToSnakeCase, ToUpperCamelCase};
use wado_compiler::syntax::{CONTEXTUAL_KEYWORDS, KEYWORDS, NAME_KEYWORDS};

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

/// Convert a `WebIDL` identifier to a Wado `snake_case` name the parser takes
/// as a name: a keyword it does not (`match`, `self`) gets a trailing `_`.
#[must_use]
pub fn to_wado_identifier(name: &str) -> String {
    let snake = name.to_snake_case();
    let keyword = KEYWORDS
        .iter()
        .chain(CONTEXTUAL_KEYWORDS)
        .any(|(keyword, _)| *keyword == snake);
    if keyword && !NAME_KEYWORDS.contains(&snake.as_str()) {
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
        assert_eq!(to_wado_identifier("match"), "match_");
        assert_eq!(to_wado_identifier("resume"), "resume_");
        assert_eq!(to_wado_identifier("self"), "self_");
        // The parser takes these as names.
        assert_eq!(to_wado_identifier("type"), "type");
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
