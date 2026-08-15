//! `grammar-corpus`: emit one syntax-only verdict per Wado source, as the
//! language service sees it.
//!
//! The reference side of the `Wado.g4` alignment check. The grammar side is
//! `package-gale-highlight-wado/tools/corpus_check.wado`, which prints the
//! same columns from the Gale-generated parser; `mise run check-grammar`
//! joins the two and reports every file the two parsers disagree about.
//!
//! Verdicts come from [`wado_lsp::Engine::parse_diagnostics`], so a file whose
//! imports do not resolve still reports its own syntax truthfully — necessary
//! for a corpus of compiler fixtures, many of which are deliberately broken
//! somewhere past the parser.

use std::fs;
use std::path::Path;

use lexopt::Arg::{Long, Value};

pub fn run(mut parser: lexopt::Parser) {
    let mut paths: Vec<String> = Vec::new();
    while let Some(arg) = parser.next().expect("failed to parse args") {
        match arg {
            // The corpus runs to thousands of files, past a comfortable argv.
            Long("paths-from") => {
                let file = parser.value().unwrap().to_string_lossy().into_owned();
                let list =
                    fs::read_to_string(&file).unwrap_or_else(|e| panic!("reading '{file}': {e}"));
                paths.extend(list.lines().filter(|l| !l.is_empty()).map(str::to_string));
            }
            Value(v) => paths.push(v.to_string_lossy().into_owned()),
            other => panic!("unexpected argument: {other:?}"),
        }
    }
    assert!(
        !paths.is_empty(),
        "no inputs: pass paths as arguments or via --paths-from <file>"
    );

    let mut engine = wado_lsp::Engine::new();
    let mut bad = 0usize;
    for path in &paths {
        let Ok(source) = fs::read_to_string(path) else {
            bad += 1;
            println!("ng\t{path}\t0\t0\tunreadable\t");
            continue;
        };
        let canonical = Path::new(path)
            .canonicalize()
            .unwrap_or_else(|_| Path::new(path).to_path_buf());
        let uri = wado_lsp::Uri::from_file_path(&canonical)
            .as_str()
            .to_string();
        engine.open_document(&uri, source.clone());
        let diagnostics = engine.parse_diagnostics(&uri);
        engine.close_document(&uri);

        match diagnostics.first() {
            None => println!("ok\t{path}"),
            Some(first) => {
                bad += 1;
                let line = first.range.start.line + 1;
                let snippet = source
                    .lines()
                    .nth(first.range.start.line as usize)
                    .unwrap_or_default()
                    .trim();
                let count = diagnostics.len();
                let message = &first.message;
                println!("ng\t{path}\t{count}\t{line}\t{message}\t{snippet}");
            }
        }
    }
    eprintln!("summary: {} files, {bad} with diagnostics", paths.len());
}
