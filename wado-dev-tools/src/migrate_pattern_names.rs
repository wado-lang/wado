//! `migrate-pattern-names`: rewrites each bare name in a pattern that the
//! compiler resolves to a case or a constant into the form that names it by its
//! shape alone (`null`, `_::Case`, `== NAME`). The edits land where the
//! analysis resolved the name, never where the text matches.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use lexopt::Arg::{Long, Value as Positional};
use wado_cli::discover;
use wado_cli::query_adapter::pattern_name_rewrites;
use wado_compiler::ast::{AstVisitor, Item, Pattern, walk_item, walk_pattern};
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
    let files = collect_files(&roots);
    eprintln!("analyzing {} files", files.len());
    let files = Arc::new(Mutex::new(files));
    let workers = thread::available_parallelism().map_or(1, usize::from);
    let handles: Vec<_> = (0..workers)
        .map(|_| {
            let files = Arc::clone(&files);
            thread::Builder::new()
                .stack_size(wado_cli::COMPILER_STACK_SIZE)
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("a migration runtime");
                    loop {
                        let Some(file) = files.lock().expect("the file queue").pop() else {
                            return;
                        };
                        for line in runtime.block_on(migrate(&file, mode)) {
                            println!("{line}");
                        }
                    }
                })
                .expect("a migration worker")
        })
        .collect();
    for handle in handles {
        handle.join().expect("a migration worker panicked");
    }
}

/// The files to analyze: those whose patterns hold a bare name that some case
/// or global under the roots is named, since no other bare name can resolve to
/// one. A parse is far cheaper than the analysis, which runs Kiln generators.
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
    let mut declared = HashSet::new();
    for root in roots {
        declared_names(root, &mut declared);
    }
    // A golden is regenerated from its fixture, not migrated.
    files.retain(|file| !file.to_string_lossy().contains("/tests/generated/"));
    files.retain(|file| {
        fs::read_to_string(file).is_ok_and(|source| {
            let mut names = PatternNames(Vec::new());
            for item in &wado_compiler::parse(&source).ast.items {
                names.visit_item(item);
            }
            names.0.iter().any(|name| declared.contains(name))
        })
    });
    files
}

/// Every case and global a `.wado` file under `path` declares, gitignored
/// generator output included.
fn declared_names(path: &Path, out: &mut HashSet<String>) {
    if path.is_dir() {
        let skipped = path
            .file_name()
            .is_some_and(|n| n == "target" || n == ".git" || n == "node_modules");
        if skipped {
            return;
        }
        for entry in fs::read_dir(path).into_iter().flatten().flatten() {
            declared_names(&entry.path(), out);
        }
        return;
    }
    if path.extension().is_none_or(|e| e != "wado") {
        return;
    }
    let Ok(source) = fs::read_to_string(path) else {
        return;
    };
    let mut decls = Declared(out);
    for item in &wado_compiler::parse(&source).ast.items {
        decls.visit_item(item);
    }
}

struct Declared<'a>(&'a mut HashSet<String>);

impl AstVisitor<'_> for Declared<'_> {
    fn visit_item(&mut self, item: &Item) {
        match item {
            Item::Enum(e) => self.0.extend(e.cases.iter().map(|c| c.name.clone())),
            Item::Variant(v) => self.0.extend(v.cases.iter().map(|c| c.name.clone())),
            Item::Global(g) => {
                self.0.insert(g.name.clone());
            }
            _ => {}
        }
        walk_item(self, item);
    }
}

/// The bare names a module's patterns hold.
struct PatternNames(Vec<String>);

impl AstVisitor<'_> for PatternNames {
    fn visit_pattern(&mut self, pat: &Pattern) {
        if let Pattern::Ident { name, .. } = pat {
            self.0.push(name.clone());
        }
        walk_pattern(self, pat);
    }
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
