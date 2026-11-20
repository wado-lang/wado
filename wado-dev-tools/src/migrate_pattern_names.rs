//! `migrate-pattern-names`: rewrites each bare name in a pattern that the
//! compiler resolves to a case or a constant into the form that names it by its
//! shape alone (`null`, `_::Case`, `== NAME`). The edits land where the
//! analysis resolved the name, never where the text matches.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use lexopt::Arg::{Long, Value as Positional};
use wado_cli::discover;
use wado_cli::query_adapter::pattern_name_rewrites;
use wado_lsp::pattern_names::PatternNameRewrite;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Apply,
    DryRun,
}

pub fn run(mut parser: lexopt::Parser) {
    let mut mode = Mode::Apply;
    let mut roots = Vec::new();
    while let Some(arg) = parser.next().expect("failed to parse args") {
        match arg {
            Long("dry-run") => mode = Mode::DryRun,
            Positional(value) => roots.push(PathBuf::from(value)),
            _ => panic!("unexpected argument: {arg:?}"),
        }
    }
    assert!(
        !roots.is_empty(),
        "usage: migrate-pattern-names [--dry-run] PATH..."
    );
    let files = Arc::new(Mutex::new(collect_files(&roots)));
    let workers = thread::available_parallelism().map_or(1, usize::from);
    let handles: Vec<_> = (0..workers)
        .map(|_| {
            let files = Arc::clone(&files);
            thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a migration runtime");
                let mut lines = Vec::new();
                loop {
                    let Some(file) = files.lock().expect("the file queue").pop() else {
                        return lines;
                    };
                    lines.extend(runtime.block_on(migrate(&file, mode)));
                }
            })
        })
        .collect();
    let mut lines: Vec<String> = handles
        .into_iter()
        .flat_map(|h| h.join().expect("a migration worker panicked"))
        .collect();
    lines.sort();
    for line in lines {
        println!("{line}");
    }
}

fn collect_files(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for root in roots {
        if root.is_file() {
            files.push(root.clone());
            continue;
        }
        let packages = discover::discover_tree(root, &discover::no_filters)
            .unwrap_or_else(|_| panic!("{}: discovery failed", root.display()));
        files.extend(packages.into_iter().flat_map(|pkg| pkg.files));
    }
    files.sort();
    files.dedup();
    files
}

/// One report line per rewrite, or per file the analysis could not load.
async fn migrate(file: &Path, mode: Mode) -> Vec<String> {
    let shown = file.display();
    let rewrites = match pattern_name_rewrites(file).await {
        Ok(rewrites) => rewrites,
        Err(reason) => return vec![format!("{shown}: skipped: {reason}")],
    };
    if rewrites.is_empty() {
        return Vec::new();
    }
    let mut text = fs::read_to_string(file).unwrap_or_else(|e| panic!("{shown}: {e}"));
    let mut lines = Vec::new();
    for PatternNameRewrite {
        start,
        end,
        name,
        replacement,
    } in rewrites.iter().rev()
    {
        let line = text[..*start].matches('\n').count() + 1;
        if text.get(*start..*end) != Some(name.as_str()) {
            lines.push(format!("{shown}:{line}: skipped: `{name}` is not at its span"));
            continue;
        }
        lines.push(format!("{shown}:{line}: {name} -> {replacement}"));
        text.replace_range(*start..*end, replacement);
    }
    if mode == Mode::Apply {
        fs::write(file, text).unwrap_or_else(|e| panic!("{shown}: {e}"));
    }
    lines
}
