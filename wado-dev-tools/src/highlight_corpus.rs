//! `highlight-corpus`: hold the Gale highlighter to the compiler's own
//! classification over the stdlib + fixture corpus.
//!
//! The sibling of [`crate::grammar_corpus`], which compares parse verdicts on
//! the same corpus and through the same split — the compiler in-process, Gale
//! under `wado run`, `scripts/check-highlight.sh` driving both.
//!
//! Both vocabularies project onto [`Class`], and the projection is where the
//! comparison stays honest about what a context-free grammar can know: every
//! class but `Ident` is decidable without name resolution and is gated, while
//! `Ident` covers the dozen resolved kinds Gale cannot tell apart and is only
//! reported. Files Gale reports diagnostics for are skipped and counted:
//! `check-grammar` owns that failure. The two recoveries agree closely — what
//! comparing them anyway adds is a divergence on the broken token itself.

use std::fmt::Write as _;
use std::fs;

use lexopt::Arg::{Long, Value};
use wado_compiler::hashmap::IndexMap;
use wado_lsp::semantic_tokens::{
    ClassifiedToken, TOKEN_TYPES, classify_all, token_modifier, token_type,
};

/// The vocabulary both sides are projected onto. Everything above `Ident` is
/// decidable from syntax alone; `Ident` is where semantics starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Class {
    Comment,
    String,
    Number,
    Keyword,
    Constant,
    Operator,
    Ident,
}

impl Class {
    /// Whether disagreement on this class is a defect rather than a capability
    /// gap. See the module docs.
    fn is_gated(self) -> bool {
        self != Self::Ident
    }

    fn name(self) -> &'static str {
        match self {
            Self::Comment => "comment",
            Self::String => "string",
            Self::Number => "number",
            Self::Keyword => "keyword",
            Self::Constant => "constant",
            Self::Operator => "operator",
            Self::Ident => "ident",
        }
    }
}

/// Project one LSP semantic token onto [`Class`]. Every resolved identifier
/// kind collapses to `Ident`: the distinctions below it are exactly the ones
/// Gale cannot make.
fn class_of_token(token: &ClassifiedToken) -> Class {
    const CONSTANT_MODIFIERS: u32 = token_modifier::READONLY | token_modifier::DEFAULT_LIBRARY;
    match token.token_type {
        token_type::COMMENT => Class::Comment,
        token_type::STRING => Class::String,
        token_type::NUMBER => Class::Number,
        token_type::KEYWORD => Class::Keyword,
        token_type::OPERATOR => Class::Operator,
        token_type::VARIABLE if token.modifiers & CONSTANT_MODIFIERS == CONSTANT_MODIFIERS => {
            Class::Constant
        }
        _ => Class::Ident,
    }
}

/// Project a tree-sitter capture name onto [`Class`]. An unknown capture is a
/// programming error, not corpus data: `highlight-vocab` pins the vocabulary.
fn class_of_capture(capture: &str) -> Class {
    match capture {
        "comment" => Class::Comment,
        "string" => Class::String,
        "number" => Class::Number,
        "keyword" => Class::Keyword,
        "constant.builtin" => Class::Constant,
        "operator" => Class::Operator,
        "variable" | "type" | "property" => Class::Ident,
        other => panic!(
            "the highlight query emits capture '{other}', which this comparison has no class for"
        ),
    }
}

/// One classified byte span, from either side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Piece {
    start: usize,
    end: usize,
    class: Class,
    /// What its own side called the span before [`Class`] flattened it: an LSP
    /// legend name, or — Gale having nothing finer — the class name.
    kind: &'static str,
}

/// How one of the compiler's spans lines up with the Gale side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Relation {
    /// Same span, different class.
    ClassDiffers,
    /// Same start, different end.
    BoundaryDiffers,
    /// The query colours nothing there.
    Uncovered,
    /// A Gale span with no counterpart, and not inside one either.
    Unexpected,
}

impl Relation {
    fn name(self) -> &'static str {
        match self {
            Self::ClassDiffers => "class",
            Self::BoundaryDiffers => "boundary",
            Self::Uncovered => "uncovered",
            Self::Unexpected => "unexpected",
        }
    }
}

/// One disagreement, before grouping.
#[derive(Debug, Clone)]
struct Divergence {
    path: String,
    line: u32,
    relation: Relation,
    compiler: Option<Class>,
    gale: Option<Class>,
    /// The unflattened name from whichever side has a span here — the one that
    /// says whether a gap is closable. See [`Piece::kind`].
    kind: &'static str,
    text: String,
}

impl Divergence {
    /// The pattern this divergence is an instance of. Thousands of
    /// divergences collapse onto a handful of these, which is what makes the
    /// report triageable.
    fn pattern(&self) -> (Relation, Option<Class>, Option<Class>) {
        (self.relation, self.compiler, self.gale)
    }

    fn is_gated(&self) -> bool {
        self.compiler.is_some_and(Class::is_gated) || self.gale.is_some_and(Class::is_gated)
    }
}

/// The Gale side's dump for one file. See `tools/highlight_dump.wado`.
struct GaleFile {
    diagnostics: i32,
    pieces: Vec<Piece>,
}

pub fn run(mut parser: lexopt::Parser) {
    let mut emit_corpus: Option<String> = None;
    let mut compare: Option<String> = None;
    let mut report: Option<String> = None;
    while let Some(arg) = parser.next().expect("failed to parse args") {
        match arg {
            Long("emit-corpus") => emit_corpus = Some(value(&mut parser)),
            Long("compare") => compare = Some(value(&mut parser)),
            Long("report") => report = Some(value(&mut parser)),
            Value(v) => panic!("unexpected argument: {}", v.to_string_lossy()),
            other => panic!("unexpected argument: {other:?}"),
        }
    }

    if let Some(out) = emit_corpus {
        crate::grammar_corpus::emit_corpus_to(&out);
        return;
    }

    let gale_tsv = compare.expect("pass --emit-corpus <file> or --compare <gale.tsv>");
    compare_with_gale(&gale_tsv, report.as_deref());
}

fn value(parser: &mut lexopt::Parser) -> String {
    parser.value().unwrap().to_string_lossy().into_owned()
}

/// Parse `tools/highlight_dump.wado`'s output: a capture table, then one
/// header per file followed by its spans.
fn read_gale_dump(path: &str) -> IndexMap<String, GaleFile> {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("reading '{path}': {e}"));
    let mut captures: Vec<Class> = Vec::new();
    let mut files: IndexMap<String, GaleFile> = IndexMap::default();
    let mut current: Option<String> = None;

    for line in text.lines() {
        let mut fields = line.split('\t');
        let head = fields.next().unwrap_or_default();
        match head {
            "V" => captures = fields.map(class_of_capture).collect(),
            "F" => {
                let file = fields.next().expect("F line needs a path").to_string();
                let diagnostics = fields
                    .next()
                    .and_then(|f| f.parse().ok())
                    .expect("F line needs a diagnostic count");
                files.insert(
                    file.clone(),
                    GaleFile {
                        diagnostics,
                        pieces: Vec::new(),
                    },
                );
                current = Some(file);
            }
            first => {
                let file = current.as_ref().expect("a span line before any F line");
                let start: usize = first.parse().unwrap_or_else(|e| panic!("bad start: {e}"));
                let end: usize = fields
                    .next()
                    .and_then(|f| f.parse().ok())
                    .expect("span line needs an end");
                let id: usize = fields
                    .next()
                    .and_then(|f| f.parse().ok())
                    .expect("span line needs a capture id");
                files
                    .get_mut(file)
                    .expect("current file is in the map")
                    .pieces
                    .push(Piece {
                        start,
                        end,
                        class: captures[id],
                        kind: captures[id].name(),
                    });
            }
        }
    }
    files
}

/// The compiler's classification of `source`.
///
/// Semantics are deliberately not loaded: every gated class is decidable from
/// the token stream and the AST alone, so resolving the whole corpus would buy
/// only the `Ident` sub-kinds — which are never gated.
fn compiler_pieces(source: &str) -> Vec<Piece> {
    classify_all(source, None)
        .iter()
        .map(|token| Piece {
            start: token.span.start,
            end: token.span.end,
            class: class_of_token(token),
            kind: TOKEN_TYPES[token.token_type as usize],
        })
        .collect()
}

/// Move the Gale side onto byte offsets.
///
/// Its runtime scans a `List<char>`, so every span it reports is a codepoint
/// index. The two agree on any ASCII file and drift from the first multi-byte
/// character onward, which reads as wholesale disagreement rather than as an
/// encoding mismatch — `if` lining up with the `->` ten characters later.
fn to_byte_offsets(source: &str, pieces: &[Piece]) -> Vec<Piece> {
    let mut byte_of_char: Vec<usize> = source.char_indices().map(|(at, _)| at).collect();
    byte_of_char.push(source.len());
    let at = |index: usize| byte_of_char.get(index).copied().unwrap_or(source.len());
    pieces
        .iter()
        .map(|piece| Piece {
            start: at(piece.start),
            end: at(piece.end),
            ..*piece
        })
        .collect()
}

/// 1-based line number of byte offset `at`.
fn line_of(source: &str, at: usize) -> u32 {
    source.as_bytes()[..at.min(source.len())]
        .iter()
        .filter(|b| **b == b'\n')
        .count() as u32
        + 1
}

/// The source text a span covers, on one line and bounded, for the report.
fn text_of(source: &str, piece: &Piece) -> String {
    let text = source
        .get(piece.start..piece.end.min(source.len()))
        .unwrap_or("<not a char boundary>");
    let first = text.lines().next().unwrap_or("");
    if first.chars().count() > 40 {
        format!("{}…", first.chars().take(40).collect::<String>())
    } else {
        first.to_string()
    }
}

/// Join one file's two classifications, returning the disagreements and a
/// count of the splits that are not one.
///
/// A Gale span inside a compiler span of the *same* class is a refinement: the
/// compiler paints a template's leading `` `text${ `` in one run, the grammar
/// in three. Of a different class it is a disagreement, and one the container
/// would otherwise swallow — that is how a mis-captured format specifier hides.
fn diverge(path: &str, source: &str, mine: &[Piece], theirs: &[Piece]) -> (Vec<Divergence>, usize) {
    assert!(
        mine.windows(2).all(|pair| pair[0].end <= pair[1].start),
        "each compiler span must end no later than the next begins: {path}"
    );
    let record = |piece: &Piece, relation, compiler, gale| Divergence {
        path: path.to_string(),
        line: line_of(source, piece.start),
        relation,
        compiler,
        gale,
        kind: piece.kind,
        text: text_of(source, piece),
    };

    let by_start: IndexMap<usize, &Piece> = theirs.iter().map(|p| (p.start, p)).collect();
    let mut out = Vec::new();
    let mut refinements = 0usize;

    for piece in mine {
        let Some(other) = by_start.get(&piece.start) else {
            out.push(record(piece, Relation::Uncovered, Some(piece.class), None));
            continue;
        };
        if other.class == piece.class && other.end == piece.end {
            continue;
        }
        if other.class == piece.class && other.end < piece.end {
            refinements += 1;
            continue;
        }
        let relation = if other.end == piece.end {
            Relation::ClassDiffers
        } else {
            Relation::BoundaryDiffers
        };
        out.push(record(
            piece,
            relation,
            Some(piece.class),
            Some(other.class),
        ));
    }

    for piece in theirs {
        // Sorted and disjoint, so the only span that can start at or contain
        // this one is the last starting no later than it.
        let after = mine.partition_point(|m| m.start <= piece.start);
        let Some(counterpart) = after.checked_sub(1).map(|at| mine[at]) else {
            out.push(record(piece, Relation::Unexpected, None, Some(piece.class)));
            continue;
        };
        if counterpart.start == piece.start {
            continue; // the loop above already judged this pair
        }
        if piece.end > counterpart.end {
            out.push(record(piece, Relation::Unexpected, None, Some(piece.class)));
        } else if counterpart.class == piece.class {
            refinements += 1;
        } else {
            out.push(record(
                piece,
                Relation::ClassDiffers,
                Some(counterpart.class),
                Some(piece.class),
            ));
        }
    }
    (out, refinements)
}

fn compare_with_gale(gale_tsv: &str, report_path: Option<&str>) {
    let gale = read_gale_dump(gale_tsv);
    let corpus = crate::grammar_corpus::collect_corpus();

    let mut divergences: Vec<Divergence> = Vec::new();
    let mut compared = 0usize;
    let mut skipped = 0usize;
    let mut refinements = 0usize;
    for path in &corpus {
        let Some(theirs) = gale.get(path) else {
            panic!("the Gale side dumped nothing for '{path}' — the two corpora differ");
        };
        if theirs.diagnostics != 0 {
            skipped += 1;
            continue;
        }
        let source = fs::read_to_string(path).unwrap_or_else(|e| panic!("reading '{path}': {e}"));
        compared += 1;
        let gale_pieces = to_byte_offsets(&source, &theirs.pieces);
        let (found, refined) = diverge(path, &source, &compiler_pieces(&source), &gale_pieces);
        refinements += refined;
        divergences.extend(found);
    }

    let patterns = group(&divergences);
    if let Some(path) = report_path {
        fs::write(path, render_report(&patterns))
            .unwrap_or_else(|e| panic!("writing '{path}': {e}"));
    }
    println!(
        "corpus: {} files | compared: {compared} | skipped (Gale diagnostics): {skipped} | \
         divergences: {} in {} patterns | template refinements: {refinements}",
        corpus.len(),
        divergences.len(),
        patterns.len(),
    );
    print!("{}", render_capability_gap(&divergences));

    let gated: Vec<&(usize, Divergence)> = patterns.iter().filter(|(_, d)| d.is_gated()).collect();
    if gated.is_empty() {
        return;
    }
    eprintln!(
        "\nerror: the grammar and the compiler colour {} pattern(s) differently on a class \
         neither needs semantics for:\n",
        gated.len()
    );
    for (count, example) in &gated {
        eprintln!("  {}", render_pattern(*count, example));
    }
    // A check verdict, not a programming error: no backtrace.
    std::process::exit(1);
}

/// What the grammar leaves uncoloured, by the kind the compiler resolved it
/// to — the only view that says which gaps are worth closing.
///
/// A `type` or `typeParameter` sits in a syntactic position the query could
/// name (`(typeRef (IDENTIFIER) @type)`). A `function` or `parameter` does
/// not: telling one from a plain variable takes name resolution, which is
/// exactly what a context-free grammar cannot do. Reported, never gated.
fn render_capability_gap(divergences: &[Divergence]) -> String {
    let mut counts: IndexMap<&'static str, usize> = IndexMap::default();
    for divergence in divergences {
        if divergence.relation == Relation::Uncovered && divergence.compiler == Some(Class::Ident) {
            *counts.entry(divergence.kind).or_insert(0) += 1;
        }
    }
    if counts.is_empty() {
        return String::new();
    }
    let mut rows: Vec<(&str, usize)> = counts.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));

    let mut out = String::from("uncoloured by the grammar, by resolved kind:\n");
    for (kind, count) in rows {
        writeln!(out, "  {count:>7}  {kind}").unwrap();
    }
    out
}

/// Collapse divergences onto their patterns, most frequent first, keeping one
/// example each.
fn group(divergences: &[Divergence]) -> Vec<(usize, Divergence)> {
    let mut grouped: IndexMap<(Relation, Option<Class>, Option<Class>), (usize, Divergence)> =
        IndexMap::default();
    for divergence in divergences {
        grouped
            .entry(divergence.pattern())
            .and_modify(|(count, _)| *count += 1)
            .or_insert((1, divergence.clone()));
    }
    let mut out: Vec<(usize, Divergence)> = grouped.into_values().collect();
    out.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.pattern().cmp(&b.1.pattern())));
    out
}

fn class_name(class: Option<Class>) -> &'static str {
    match class {
        Some(class) => class.name(),
        None => "-",
    }
}

fn render_pattern(count: usize, example: &Divergence) -> String {
    format!(
        "{count}x {}: compiler={} gale={} e.g. {}:{} {:?}",
        example.relation.name(),
        class_name(example.compiler),
        class_name(example.gale),
        example.path,
        example.line,
        example.text,
    )
}

fn render_report(patterns: &[(usize, Divergence)]) -> String {
    let mut out = String::from(
        "# Where the two Wado highlighters disagree. Generated, not committed.\n#\n\
         # Grouped by pattern, most frequent first, one example each. A pattern\n\
         # naming `ident` on either side is a capability gap, not a defect: Gale\n\
         # is context-free and cannot resolve a name.\n#\n\
         # count<TAB>relation<TAB>compiler<TAB>gale<TAB>path<TAB>line<TAB>text\n",
    );
    for (count, example) in patterns {
        writeln!(
            out,
            "{count}\t{}\t{}\t{}\t{}\t{}\t{}",
            example.relation.name(),
            class_name(example.compiler),
            class_name(example.gale),
            example.path,
            example.line,
            example.text,
        )
        .unwrap();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn piece(start: usize, end: usize, class: Class) -> Piece {
        Piece {
            start,
            end,
            class,
            kind: class.name(),
        }
    }

    /// The grammar splitting one compiler run into several of the same class is
    /// how a template literal always looks: one `String` token against a
    /// backtick, a text chunk, and a `${`.
    #[test]
    fn a_finer_split_of_the_same_class_is_a_refinement() {
        let mine = [piece(0, 6, Class::String)];
        let theirs = [
            piece(0, 1, Class::String),
            piece(1, 4, Class::String),
            piece(4, 6, Class::String),
        ];
        let (found, refinements) = diverge("t.wado", "`ab${", &mine, &theirs);
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(refinements, 3);
    }

    /// A span of a *different* class inside a compiler run is a disagreement,
    /// not a refinement — the shape a mis-captured format specifier takes, and
    /// the one a containment-only rule swallows. Here the compiler mutes
    /// `:>8.2` as one comment while the grammar leaves the `>` an operator.
    #[test]
    fn a_nested_span_of_another_class_is_a_divergence() {
        let mine = [piece(0, 5, Class::Comment)];
        let theirs = [piece(0, 1, Class::Comment), piece(1, 2, Class::Operator)];
        let (found, refinements) = diverge("t.wado", ":>8.2", &mine, &theirs);
        assert_eq!(refinements, 1, "the leading `:` still refines");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].relation, Relation::ClassDiffers);
        assert_eq!(found[0].compiler, Some(Class::Comment));
        assert_eq!(found[0].gale, Some(Class::Operator));
    }

    /// Gale's spans are codepoint offsets; the compiler's are bytes. They agree
    /// on ASCII and drift from the first multi-byte character onward.
    #[test]
    fn gale_offsets_convert_from_codepoints_to_bytes() {
        let source = "//あ\nlet x = 1;";
        // `let` is codepoints 4..7; `あ` is three bytes, so it is bytes 6..9.
        let converted = to_byte_offsets(source, &[piece(4, 7, Class::Keyword)]);
        assert_eq!(
            source.get(converted[0].start..converted[0].end),
            Some("let")
        );
    }
}
