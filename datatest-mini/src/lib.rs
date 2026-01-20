//! Minimal `proc_macro` for generating test functions from fixture files.
//!
//! This crate provides a drop-in replacement for datatest-stable.
//!
//! # Usage
//!
//! ```ignore
//! use std::path::Path;
//!
//! fn run_test(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
//!     // Your test logic here
//!     Ok(())
//! }
//!
//! datatest_mini::harness! {
//!     { test = run_test, root = "tests/fixtures", pattern = r"^[^/]+\.wado$" },
//! }
//! ```

use proc_macro::{Delimiter, Group, Ident, Literal, Punct, Spacing, Span, TokenStream, TokenTree};
use regex::Regex;
use std::path::Path;

/// Generate a test harness for fixture files.
///
/// # Syntax
///
/// ```ignore
/// datatest_mini::harness! {
///     { test = test_fn, root = "path/to/fixtures", pattern = r"pattern" },
/// }
/// ```
///
/// Multiple test sets can be specified by adding more entries.
/// The test function name is used as the module name (e.g., `test_fn::file_name`).
///
/// # Panics
///
/// Panics if:
/// - `CARGO_MANIFEST_DIR` environment variable is not set
/// - The fixture directory does not exist
/// - The pattern regex is invalid
/// - The fixture directory cannot be read
#[proc_macro]
pub fn harness(input: TokenStream) -> TokenStream {
    let entries = parse_harness_entries(input);

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");

    let mut all_tests: Vec<TestEntry> = Vec::new();

    for entry in entries {
        let full_path = Path::new(&manifest_dir).join(&entry.root);
        assert!(
            full_path.exists(),
            "fixture directory does not exist: {}",
            full_path.display()
        );

        let regex = Regex::new(&entry.pattern)
            .unwrap_or_else(|e| panic!("invalid pattern '{}': {}", entry.pattern, e));

        // Use test function name as module name
        collect_matching_files(
            &full_path,
            &full_path,
            &regex,
            &entry.test_fn,
            &mut all_tests,
        );
    }

    generate_test_functions(&all_tests)
}

struct HarnessEntry {
    test_fn: String,
    root: String,
    pattern: String,
}

fn parse_harness_entries(input: TokenStream) -> Vec<HarnessEntry> {
    let mut entries = Vec::new();
    let mut iter = input.into_iter().peekable();

    while let Some(token) = iter.next() {
        if let TokenTree::Group(group) = token
            && group.delimiter() == Delimiter::Brace
            && let Some(entry) = parse_single_entry(group.stream())
        {
            entries.push(entry);
        }
        // Skip commas between entries
        if let Some(TokenTree::Punct(p)) = iter.peek()
            && p.as_char() == ','
        {
            iter.next();
        }
    }

    entries
}

fn parse_single_entry(stream: TokenStream) -> Option<HarnessEntry> {
    let mut test_fn = None;
    let mut root = None;
    let mut pattern = None;

    let mut iter = stream.into_iter().peekable();

    while let Some(token) = iter.next() {
        if let TokenTree::Ident(ident) = token {
            let key = ident.to_string();

            // Skip '='
            if let Some(TokenTree::Punct(p)) = iter.next()
                && p.as_char() != '='
            {
                continue;
            }

            // Get value
            match key.as_str() {
                "test" => {
                    if let Some(TokenTree::Ident(val)) = iter.next() {
                        test_fn = Some(val.to_string());
                    }
                }
                "root" => {
                    if let Some(TokenTree::Literal(lit)) = iter.next() {
                        root = Some(parse_string_literal(&lit));
                    }
                }
                "pattern" => {
                    if let Some(TokenTree::Literal(lit)) = iter.next() {
                        pattern = Some(parse_string_literal(&lit));
                    }
                }
                _ => {}
            }

            // Skip comma
            if let Some(TokenTree::Punct(p)) = iter.peek()
                && p.as_char() == ','
            {
                iter.next();
            }
        }
    }

    Some(HarnessEntry {
        test_fn: test_fn?,
        root: root?,
        pattern: pattern?,
    })
}

fn parse_string_literal(lit: &Literal) -> String {
    let s = lit.to_string();
    // Handle both regular strings "..." and raw strings r"..."
    if s.starts_with("r\"") || s.starts_with("r#") {
        // Raw string: r"..." or r#"..."#
        let s = s.trim_start_matches('r');
        let hash_count = s.chars().take_while(|&c| c == '#').count();
        let start = hash_count + 1; // Skip '#'s and opening '"'
        let end = s.len() - hash_count - 1; // Skip closing '"' and '#'s
        s[start..end].to_string()
    } else {
        // Regular string: "..."
        s.trim_matches('"').to_string()
    }
}

struct TestEntry {
    module_name: String,
    test_name: String,
    path: String,
    test_fn: String,
}

fn collect_matching_files(
    base_path: &Path,
    current_path: &Path,
    pattern: &Regex,
    test_fn: &str,
    tests: &mut Vec<TestEntry>,
) {
    let entries = match std::fs::read_dir(current_path) {
        Ok(entries) => entries,
        Err(e) => panic!("failed to read directory {}: {}", current_path.display(), e),
    };

    let mut entries: Vec<_> = entries.filter_map(std::result::Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::path);

    for entry in entries {
        let path = entry.path();

        if path.is_dir() {
            // Recurse into subdirectories
            collect_matching_files(base_path, &path, pattern, test_fn, tests);
            continue;
        }

        if !path.is_file() {
            continue;
        }

        // Get relative path from base for pattern matching
        let rel_path = path
            .strip_prefix(base_path)
            .unwrap()
            .to_string_lossy()
            .to_string();

        // Check if pattern matches
        if !pattern.is_match(&rel_path) {
            continue;
        }

        // Generate test name from relative path
        let test_name = rel_path.replace([std::path::MAIN_SEPARATOR, '-', '.'], "_");

        tests.push(TestEntry {
            module_name: test_fn.to_string(),
            test_name,
            path: path.display().to_string(),
            test_fn: test_fn.to_string(),
        });
    }
}

fn generate_test_functions(tests: &[TestEntry]) -> TokenStream {
    use std::collections::BTreeMap;

    // Group tests by module name
    let mut modules: BTreeMap<String, Vec<&TestEntry>> = BTreeMap::new();
    for test in tests {
        modules
            .entry(test.module_name.clone())
            .or_default()
            .push(test);
    }

    let mut tokens = Vec::new();

    for (module_name, module_tests) in modules {
        let mut module_tokens = Vec::new();

        for test in module_tests {
            // #[test]
            module_tokens.push(TokenTree::Punct(Punct::new('#', Spacing::Alone)));
            module_tokens.push(TokenTree::Group(Group::new(
                Delimiter::Bracket,
                TokenStream::from_iter([TokenTree::Ident(Ident::new("test", Span::call_site()))]),
            )));

            // fn test_NAME()
            module_tokens.push(TokenTree::Ident(Ident::new("fn", Span::call_site())));
            module_tokens.push(TokenTree::Ident(Ident::new(
                &test.test_name,
                Span::call_site(),
            )));
            module_tokens.push(TokenTree::Group(Group::new(
                Delimiter::Parenthesis,
                TokenStream::new(),
            )));

            // { super::test_fn(std::path::Path::new("PATH")).unwrap(); }
            let body = TokenStream::from_iter([
                TokenTree::Ident(Ident::new("super", Span::call_site())),
                TokenTree::Punct(Punct::new(':', Spacing::Joint)),
                TokenTree::Punct(Punct::new(':', Spacing::Alone)),
                TokenTree::Ident(Ident::new(&test.test_fn, Span::call_site())),
                TokenTree::Group(Group::new(
                    Delimiter::Parenthesis,
                    TokenStream::from_iter([
                        TokenTree::Ident(Ident::new("std", Span::call_site())),
                        TokenTree::Punct(Punct::new(':', Spacing::Joint)),
                        TokenTree::Punct(Punct::new(':', Spacing::Alone)),
                        TokenTree::Ident(Ident::new("path", Span::call_site())),
                        TokenTree::Punct(Punct::new(':', Spacing::Joint)),
                        TokenTree::Punct(Punct::new(':', Spacing::Alone)),
                        TokenTree::Ident(Ident::new("Path", Span::call_site())),
                        TokenTree::Punct(Punct::new(':', Spacing::Joint)),
                        TokenTree::Punct(Punct::new(':', Spacing::Alone)),
                        TokenTree::Ident(Ident::new("new", Span::call_site())),
                        TokenTree::Group(Group::new(
                            Delimiter::Parenthesis,
                            TokenStream::from_iter([TokenTree::Literal(Literal::string(
                                &test.path,
                            ))]),
                        )),
                    ]),
                )),
                TokenTree::Punct(Punct::new('.', Spacing::Alone)),
                TokenTree::Ident(Ident::new("unwrap", Span::call_site())),
                TokenTree::Group(Group::new(Delimiter::Parenthesis, TokenStream::new())),
                TokenTree::Punct(Punct::new(';', Spacing::Alone)),
            ]);

            module_tokens.push(TokenTree::Group(Group::new(Delimiter::Brace, body)));
        }

        if module_name.is_empty() {
            // No module wrapper for empty module name
            tokens.extend(module_tokens);
        } else {
            // mod module_name { ... }
            tokens.push(TokenTree::Ident(Ident::new("mod", Span::call_site())));
            tokens.push(TokenTree::Ident(Ident::new(
                &module_name,
                Span::call_site(),
            )));
            tokens.push(TokenTree::Group(Group::new(
                Delimiter::Brace,
                TokenStream::from_iter(module_tokens),
            )));
        }
    }

    TokenStream::from_iter(tokens)
}
