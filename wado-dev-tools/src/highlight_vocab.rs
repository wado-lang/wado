//! `highlight-vocab`: hold the Gale highlight query to the compiler's canonical
//! syntax registries.
//!
//! `wado-compiler/src/syntax.rs` is the single source of truth for which words
//! are keywords and how each is categorized: the lexer, the token↔text mapping,
//! and the `TextMate` grammar are all generated from it. `Wado.g4` and
//! `Wado.highlights.scm` are hand-written and wired to nothing, so they drift.
//! This check closes that gap without a corpus — it compares vocabularies, not
//! files, and runs in milliseconds.
//!
//! Three invariants:
//!
//! 1. Every keyword in the registries is a literal in `Wado.g4`. A keyword the
//!    grammar cannot match is a parse gap, not just a colour gap.
//! 2. Every keyword in the registries carries the capture its category implies
//!    (see [`expected_capture`]).
//! 3. Every keyword-shaped literal captured by the query is a registry keyword.
//!    A capture for a word the compiler does not know means the query invented
//!    a keyword, or the registry is missing a contextual one.

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use wado_compiler::syntax::{CONTEXTUAL_KEYWORDS, KEYWORDS, KeywordCategory};

const GRAMMAR: &str = "../package-gale-highlight-wado/grammar/Wado.g4";
const QUERY: &str = "../package-gale-highlight-wado/grammar/Wado.highlights.scm";

/// One vocabulary disagreement, in the order they are reported.
#[derive(Debug, PartialEq, Eq)]
pub enum Drift {
    /// A registry keyword `Wado.g4` has no literal for.
    MissingFromGrammar { text: String },
    /// A registry keyword the query captures as nothing.
    MissingFromQuery { text: String, expected: String },
    /// A registry keyword the query captures as the wrong class.
    WrongCapture {
        text: String,
        expected: String,
        found: String,
    },
    /// A keyword-shaped literal the query captures that no registry knows.
    UnknownKeyword { text: String, found: String },
}

impl Drift {
    fn render(&self) -> String {
        match self {
            Self::MissingFromGrammar { text } => {
                format!("{text}\tWado.g4 has no '{text}' literal")
            }
            Self::MissingFromQuery { text, expected } => {
                format!("{text}\tWado.highlights.scm captures nothing; expected @{expected}")
            }
            Self::WrongCapture {
                text,
                expected,
                found,
            } => format!("{text}\tcaptured @{found}; the registry implies @{expected}"),
            Self::UnknownKeyword { text, found } => {
                format!("{text}\tcaptured @{found}, but no keyword registry knows it")
            }
        }
    }
}

/// The capture a keyword's editorial category implies. `Constant` and
/// `Operator` exist precisely because those words are *not* coloured as
/// keywords, so the query must not flatten them into `@keyword`.
fn expected_capture(category: KeywordCategory) -> &'static str {
    match category {
        KeywordCategory::Control
        | KeywordCategory::StorageType
        | KeywordCategory::StorageModifier
        | KeywordCategory::Other => "keyword",
        KeywordCategory::Constant => "constant.builtin",
        KeywordCategory::Operator => "operator",
    }
}

fn repo_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn read(relative: &str) -> String {
    let path = repo_path(relative);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading '{}': {e}", path.display()))
}

/// Keyword-shaped literal capture rules from the query: `"text" @capture`.
/// Rules keyed on a lexer or parser rule (`(STRING_LITERAL) @string`) are not
/// vocabulary and are skipped.
fn literal_captures(query: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in query.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix('"') else {
            continue;
        };
        let Some((text, rest)) = rest.split_once('"') else {
            continue;
        };
        let Some(capture) = rest.trim_start().strip_prefix('@') else {
            continue;
        };
        out.push((text.to_string(), capture.trim().to_string()));
    }
    out
}

/// Every keyword the compiler knows, real and contextual, with its category.
fn registry_keywords() -> Vec<(&'static str, KeywordCategory)> {
    KEYWORDS
        .iter()
        .chain(CONTEXTUAL_KEYWORDS.iter())
        .copied()
        .collect()
}

/// Whether a captured literal is keyword-shaped — a bare lowercase word.
/// Punctuation captures (`"(" @punctuation.bracket`) are not vocabulary this
/// check owns.
fn is_word(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'_' || b.is_ascii_digit())
}

pub fn check() -> Vec<Drift> {
    let grammar = read(GRAMMAR);
    let captures = literal_captures(&read(QUERY));
    let keywords = registry_keywords();

    let mut drift = Vec::new();
    for (text, category) in &keywords {
        if !grammar.contains(&format!("'{text}'")) {
            drift.push(Drift::MissingFromGrammar {
                text: (*text).to_string(),
            });
        }
        let expected = expected_capture(*category);
        match captures.iter().find(|(t, _)| t == text) {
            None => drift.push(Drift::MissingFromQuery {
                text: (*text).to_string(),
                expected: expected.to_string(),
            }),
            Some((_, found)) if found != expected => drift.push(Drift::WrongCapture {
                text: (*text).to_string(),
                expected: expected.to_string(),
                found: found.clone(),
            }),
            Some(_) => {}
        }
    }

    for (text, found) in &captures {
        if is_word(text) && !keywords.iter().any(|(k, _)| k == text) {
            drift.push(Drift::UnknownKeyword {
                text: text.clone(),
                found: found.clone(),
            });
        }
    }
    drift
}

fn render(drift: &[Drift]) -> String {
    let mut out = String::new();
    for item in drift {
        writeln!(out, "{}", item.render()).unwrap();
    }
    out
}

pub fn run(mut parser: lexopt::Parser) {
    if let Some(arg) = parser.next().expect("failed to parse args") {
        panic!("unexpected argument: {arg:?}");
    }
    let drift = check();
    if drift.is_empty() {
        println!("highlight vocabulary: in sync");
        return;
    }
    eprint!(
        "error: the Gale highlight query has drifted from the compiler's syntax registries:\n\n{}",
        render(&drift)
    );
    // A check verdict, not a programming error: no backtrace.
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Wado.highlights.scm` colours exactly the vocabulary the compiler
    /// defines. See the module docs for the three invariants.
    #[test]
    fn query_matches_the_syntax_registries() {
        let drift = check();
        assert!(drift.is_empty(), "\n{}", render(&drift));
    }
}
