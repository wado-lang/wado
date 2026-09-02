//! `highlight-vocab`: hold the Gale highlight query to the compiler's canonical
//! syntax registries.
//!
//! `wado-compiler/src/syntax.rs` is the single source of truth for which words
//! are keywords and how each is categorized; the lexer, the token↔text
//! mapping, and the `TextMate` grammar all generate from it, while `Wado.g4`
//! and `Wado.highlights.scm` are hand-written and wired to nothing.
//!
//! Five invariants, over vocabularies rather than files, so no corpus:
//!
//! 1. Every keyword in the registries is a literal in `Wado.g4`. A keyword the
//!    grammar cannot match is a parse gap, not just a colour gap.
//! 2. Every keyword in the registries carries the capture its category implies
//!    (see [`expected_capture`]).
//! 3. Every keyword-shaped literal captured by the query is a registry keyword.
//!    A capture for a word the compiler does not know means the query invented
//!    a keyword, or the registry is missing a contextual one.
//! 4. The query captures `@operator` on exactly the operators the compiler
//!    highlights as one — the spellings that double as punctuation (`&`, `|`,
//!    `::`, `?`, `..`, `...`) stay uncoloured on both sides.
//! 5. Every keyword the parser accepts as a name is a name in `Wado.g4` too:
//!    `NAME_KEYWORDS` under `identifier`, and every keyword under `memberName`,
//!    which is where a `.name` goes. One-directional — the grammar accepts
//!    more words as names than the parser does, and `check-grammar` owns that
//!    half.

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use wado_compiler::syntax::{
    CONTEXTUAL_KEYWORDS, KEYWORDS, KeywordCategory, NAME_KEYWORDS, OPERATORS,
};

const GRAMMAR: &str = "../package-gale-highlight-wado/grammar/Wado.g4";
const QUERY: &str = "../package-gale-highlight-wado/grammar/Wado.highlights.scm";

/// Highlight operators `Wado.g4` cannot spell as a literal, with the spelling
/// it uses instead. `'>' '>'` is what lets `List<Box<i32>>` close, so `>>`
/// reaches the grammar as two tokens; the two lexers then disagree on the
/// boundary, which is `highlight-corpus`'s business, not this check's. The
/// alternative spelling is verified, so the exception cannot outlive its
/// reason.
const SPLIT_OPERATORS: &[(&str, &str)] = &[(">>", "'>' '>'")];

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
    /// An operator the compiler deliberately leaves uncoloured, captured
    /// anyway.
    PunctuationCaptured { text: String, found: String },
    /// A keyword the parser accepts as a name that a grammar name rule does
    /// not, so that program parses on one side only.
    MissingFromNameRule { text: String, rule: String },
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
            Self::PunctuationCaptured { text, found } => format!(
                "{text}\tcaptured @{found}; the compiler highlights it as punctuation, not an operator"
            ),
            Self::MissingFromNameRule { text, rule } => {
                format!("{text}\tWado.g4's `{rule}` does not accept it as a name")
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

fn read(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading '{}': {e}", path.display()))
}

/// The grammar with `formatSpecAtom` cut out.
///
/// That rule spells `. < > ^ + - # ? _ *` as literals to describe a format
/// specifier's punctuation, which is not the language using them as operators.
/// Left in, it satisfies invariant 1 for six of the operators on its own, and
/// deleting `'+'` from every expression rule would still pass.
fn grammar_without_format_spec_atoms(grammar: &str) -> String {
    const TERMINATOR: &str = "\n    ;";
    let start = grammar
        .find("\nformatSpecAtom")
        .unwrap_or_else(|| panic!("Wado.g4 no longer declares `formatSpecAtom`"));
    let end = grammar[start..]
        .find(TERMINATOR)
        .map(|at| start + at + TERMINATOR.len())
        .expect("a grammar rule ends with `;`");
    let mut out = String::with_capacity(grammar.len());
    out.push_str(&grammar[..start]);
    out.push_str(&grammar[end..]);
    out
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

/// The `'literal'` alternatives of one grammar rule.
fn rule_literals(grammar: &str, rule: &str) -> Vec<String> {
    let head = format!("\n{rule}\n");
    let start = grammar
        .find(&head)
        .unwrap_or_else(|| panic!("Wado.g4 no longer declares `{rule}`"));
    let end = grammar[start..]
        .find("\n    ;")
        .map(|at| start + at)
        .expect("a grammar rule ends with `;`");
    grammar[start..end]
        .split('\'')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
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
/// Punctuation captures are not vocabulary this check owns.
fn is_word(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'_' || b.is_ascii_digit())
}

/// The operators the compiler colours as operators, and those it leaves as
/// punctuation. Derived by asking the lexer, so the split cannot drift from
/// `is_highlight_operator`.
fn operators_by_highlighting() -> (Vec<&'static str>, Vec<&'static str>) {
    let mut highlighted = Vec::new();
    let mut punctuation = Vec::new();
    for (text, _) in OPERATORS {
        if wado_compiler::lexer::lex(text).tokens[0]
            .kind
            .is_highlight_operator()
        {
            highlighted.push(*text);
        } else {
            punctuation.push(*text);
        }
    }
    (highlighted, punctuation)
}

pub fn check() -> Vec<Drift> {
    let grammar = grammar_without_format_spec_atoms(&read(GRAMMAR));
    let captures = literal_captures(&read(QUERY));
    let keywords = registry_keywords();
    let (highlighted, punctuation) = operators_by_highlighting();

    let mut drift = Vec::new();
    let mut expected: Vec<(&str, &str)> = keywords
        .iter()
        .map(|(text, category)| (*text, expected_capture(*category)))
        .collect();
    for text in &highlighted {
        // The grammar spells a few operators out of literal form; the boundary
        // that creates is `highlight-corpus`'s business.
        match SPLIT_OPERATORS.iter().find(|(op, _)| op == text) {
            Some((_, spelling)) => assert!(
                grammar.contains(spelling),
                "'{text}' is exempt because Wado.g4 spells it {spelling}, which it no longer does"
            ),
            None => expected.push((text, "operator")),
        }
    }

    for (text, capture) in &expected {
        if !grammar.contains(&format!("'{text}'")) {
            drift.push(Drift::MissingFromGrammar {
                text: (*text).to_string(),
            });
        }
        match captures.iter().find(|(t, _)| t == text) {
            None => drift.push(Drift::MissingFromQuery {
                text: (*text).to_string(),
                expected: (*capture).to_string(),
            }),
            Some((_, found)) if found != capture => drift.push(Drift::WrongCapture {
                text: (*text).to_string(),
                expected: (*capture).to_string(),
                found: found.clone(),
            }),
            Some(_) => {}
        }
    }

    // A name position on one side and a keyword on the other is a parse gap,
    // and the corpus cannot report it: the file the grammar rejects is dropped
    // from the comparison rather than compared.
    let names: [(&str, Vec<&str>); 2] = [
        ("identifier", NAME_KEYWORDS.to_vec()),
        (
            "memberName",
            KEYWORDS.iter().map(|(text, _)| *text).collect(),
        ),
    ];
    for (rule, words) in &names {
        let accepted = rule_literals(&grammar, rule);
        for text in words {
            if !accepted.iter().any(|literal| literal == text) {
                drift.push(Drift::MissingFromNameRule {
                    text: (*text).to_string(),
                    rule: (*rule).to_string(),
                });
            }
        }
    }

    for (text, found) in &captures {
        if expected.iter().any(|(t, _)| t == text) {
            continue;
        }
        if punctuation.contains(&text.as_str()) {
            drift.push(Drift::PunctuationCaptured {
                text: text.clone(),
                found: found.clone(),
            });
        } else if is_word(text) {
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
