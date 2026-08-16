//! `grammar-corpus`: hold `package-gale-highlight-wado/grammar/Wado.g4` to the
//! compiler's own parser over the stdlib + fixture corpus.
//!
//! Three modes, driven by `scripts/check-grammar.sh`: `--emit-corpus <file>`
//! writes the corpus paths the Gale-side tool reads, `--compare <gale.tsv>
//! [--report <file>]` joins both sides' verdicts and checks the invariants
//! below, and anything else prints one verdict per path for triage.
//!
//! Verdicts come from [`wado_lsp::Engine::parse_diagnostics`], so a fixture
//! whose imports are deliberately broken still reports its syntax truthfully.
//!
//! Two invariants, both derived — a committed divergence list rotted between
//! updates and nothing could verify it:
//!
//! 1. Nothing the compiler parses is rejected by the grammar.
//! 2. Everything the grammar accepts but the compiler rejects is a fixture
//!    declaring a `compile_error` — the rules no context-free grammar can
//!    state (a chained `!=`, `internal` beside `pub`). A real source landing
//!    here means the grammar accepts what the language does not.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use lexopt::Arg::{Long, Value};

/// Corpus roots. `tests/generated` is excluded: its `*.wir.wado` files are IR
/// dumps, not Wado source.
const CORPUS_ROOTS: &[&str] = &[
    "wado-compiler/lib",
    "wado-compiler/tests/fixtures",
    "wado-compiler/tests/format.fixtures",
    "example",
];

/// One file's verdict, from either parser.
struct Verdict {
    path: String,
    ok: bool,
    line: u32,
    message: String,
}

pub fn run(mut parser: lexopt::Parser) {
    let mut paths: Vec<String> = Vec::new();
    let mut emit_corpus: Option<String> = None;
    let mut compare: Option<String> = None;
    let mut report: Option<String> = None;
    while let Some(arg) = parser.next().expect("failed to parse args") {
        match arg {
            Long("emit-corpus") => emit_corpus = Some(value(&mut parser)),
            Long("compare") => compare = Some(value(&mut parser)),
            Long("report") => report = Some(value(&mut parser)),
            // The corpus runs to thousands of files, past a comfortable argv.
            Long("paths-from") => {
                let file = value(&mut parser);
                let list =
                    fs::read_to_string(&file).unwrap_or_else(|e| panic!("reading '{file}': {e}"));
                paths.extend(list.lines().filter(|l| !l.is_empty()).map(str::to_string));
            }
            Value(v) => paths.push(v.to_string_lossy().into_owned()),
            other => panic!("unexpected argument: {other:?}"),
        }
    }

    if let Some(out) = emit_corpus {
        let corpus = collect_corpus();
        fs::write(&out, corpus.join("\n") + "\n")
            .unwrap_or_else(|e| panic!("writing '{out}': {e}"));
        eprintln!("corpus: {} files", corpus.len());
        return;
    }

    if let Some(gale_tsv) = compare {
        compare_with_gale(&gale_tsv, report.as_deref());
        return;
    }

    assert!(
        !paths.is_empty(),
        "no inputs: pass paths as arguments, --paths-from <file>, --emit-corpus <file>, or --compare <gale.tsv>"
    );
    let mut bad = 0usize;
    for verdict in verdicts_for(&paths) {
        if verdict.ok {
            println!("ok\t{}", verdict.path);
        } else {
            bad += 1;
            println!(
                "ng\t{}\t{}\t{}",
                verdict.path, verdict.line, verdict.message
            );
        }
    }
    eprintln!("summary: {} files, {bad} with diagnostics", paths.len());
}

fn value(parser: &mut lexopt::Parser) -> String {
    parser.value().unwrap().to_string_lossy().into_owned()
}

/// Every `.wado` file under [`CORPUS_ROOTS`], sorted, so both sides walk the
/// same list in the same order.
fn collect_corpus() -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for root in CORPUS_ROOTS {
        let pattern = format!("{root}/**/*.wado");
        for entry in glob::glob(&pattern).unwrap_or_else(|e| panic!("bad glob '{pattern}': {e}")) {
            let path = entry.unwrap_or_else(|e| panic!("walking '{pattern}': {e}"));
            paths.push(path.to_string_lossy().into_owned());
        }
    }
    paths.sort();
    paths
}

/// The compiler's syntax verdict for each path.
fn verdicts_for(paths: &[String]) -> Vec<Verdict> {
    let mut engine = wado_lsp::Engine::new();
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let Ok(source) = fs::read_to_string(path) else {
            out.push(Verdict {
                path: path.clone(),
                ok: false,
                line: 0,
                message: "unreadable".to_string(),
            });
            continue;
        };
        let canonical = Path::new(path)
            .canonicalize()
            .unwrap_or_else(|_| Path::new(path).to_path_buf());
        let uri = wado_lsp::Uri::from_file_path(&canonical)
            .as_str()
            .to_string();
        engine.open_document(&uri, source);
        let diagnostics = engine.parse_diagnostics(&uri);
        engine.close_document(&uri);

        out.push(match diagnostics.first() {
            None => Verdict {
                path: path.clone(),
                ok: true,
                line: 0,
                message: String::new(),
            },
            Some(first) => Verdict {
                path: path.clone(),
                ok: false,
                line: first.range.start.line + 1,
                message: first.message.clone(),
            },
        });
    }
    out
}

/// Read the Gale side's `ok` / `ng <path> <count> <line> <message> <snippet>`
/// lines into a path → verdict lookup.
fn read_gale_verdicts(path: &str) -> Vec<Verdict> {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("reading '{path}': {e}"));
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let kind = fields.next()?;
            let file = fields.next()?.to_string();
            match kind {
                "ok" => Some(Verdict {
                    path: file,
                    ok: true,
                    line: 0,
                    message: String::new(),
                }),
                "ng" => {
                    let _count = fields.next();
                    let line_no = fields.next().and_then(|f| f.parse().ok()).unwrap_or(0);
                    Some(Verdict {
                        path: file,
                        ok: false,
                        line: line_no,
                        message: fields.next().unwrap_or_default().to_string(),
                    })
                }
                _ => None,
            }
        })
        .collect()
}

/// A fixture declaring a `compile_error` — the only place the grammar may be
/// more permissive than the parser.
fn declares_compile_error(path: &str) -> bool {
    let Ok(source) = fs::read_to_string(path) else {
        return false;
    };
    crate::data_section::should_skip_file(&source)
}

fn compare_with_gale(gale_tsv: &str, report_path: Option<&str>) {
    let corpus = collect_corpus();
    let compiler = verdicts_for(&corpus);
    let gale = read_gale_verdicts(gale_tsv);

    let gale_by_path: wado_compiler::hashmap::IndexMap<&str, &Verdict> =
        gale.iter().map(|v| (v.path.as_str(), v)).collect();

    let mut rejects: Vec<&Verdict> = Vec::new();
    let mut accepts: Vec<(&Verdict, &Verdict)> = Vec::new();
    for own in &compiler {
        let Some(theirs) = gale_by_path.get(own.path.as_str()) else {
            panic!(
                "the Gale side reported no verdict for '{}' — the two corpora differ",
                own.path
            );
        };
        if theirs.ok == own.ok {
            continue;
        }
        if theirs.ok {
            accepts.push((own, theirs));
        } else {
            rejects.push(theirs);
        }
    }

    let unexplained: Vec<&Verdict> = accepts
        .iter()
        .map(|(own, _)| *own)
        .filter(|own| !declares_compile_error(&own.path))
        .collect();

    if let Some(path) = report_path {
        fs::write(path, render_report(&rejects, &accepts))
            .unwrap_or_else(|e| panic!("writing '{path}': {e}"));
    }
    println!(
        "corpus: {} files | grammar gaps: {} | parser-only rules: {}",
        corpus.len(),
        rejects.len(),
        accepts.len()
    );

    let mut failed = false;
    if !rejects.is_empty() {
        eprintln!(
            "\nerror: Wado.g4 rejects {} file(s) the compiler parses. Every one is a grammar gap:",
            rejects.len()
        );
        for verdict in &rejects {
            eprintln!("  {}:{}: {}", verdict.path, verdict.line, verdict.message);
        }
        failed = true;
    }
    if !unexplained.is_empty() {
        eprintln!(
            "\nerror: the compiler's parser rejects {} file(s) Wado.g4 accepts, and they are not \
             fixtures that expect a compile error:",
            unexplained.len()
        );
        for verdict in &unexplained {
            eprintln!("  {}:{}: {}", verdict.path, verdict.line, verdict.message);
        }
        eprintln!(
            "\n  Either the grammar accepts something the language does not, or the file needs a\n  \
             `__DATA__` section declaring the compile error it expects."
        );
        failed = true;
    }
    if failed {
        // A check verdict, not a programming error: no backtrace.
        std::process::exit(1);
    }
}

fn render_report(rejects: &[&Verdict], accepts: &[(&Verdict, &Verdict)]) -> String {
    let mut out = String::from(
        "# Where the two Wado parsers disagree. Generated, not committed.\n#\n\
         #   gap    Wado.g4 rejects what the compiler parses.\n\
         #   rule   the compiler rejects what Wado.g4 accepts; each is a fixture\n\
         #          declaring the compile error it expects.\n#\n\
         # kind<TAB>path<TAB>line<TAB>message\n",
    );
    for verdict in rejects {
        writeln!(
            out,
            "gap\t{}\t{}\t{}",
            verdict.path, verdict.line, verdict.message
        )
        .unwrap();
    }
    for (own, _) in accepts {
        writeln!(out, "rule\t{}\t{}\t{}", own.path, own.line, own.message).unwrap();
    }
    out
}
