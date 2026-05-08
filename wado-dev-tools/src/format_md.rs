use std::path::{Path, PathBuf};

use dprint_plugin_markdown::configuration::ConfigurationBuilder;
use dprint_plugin_markdown::format_text;
use lexopt::Arg::{Long, Value};

const DEFAULT_EXCLUDED_DIRS: &[&str] =
    &[".vscode-test", "vendor", "target", "node_modules", ".git"];

pub fn run(mut parser: lexopt::Parser) {
    let mut check = false;
    let mut paths: Vec<String> = Vec::new();
    while let Some(arg) = parser.next().expect("failed to parse args") {
        match arg {
            Long("check") => check = true,
            Value(v) => paths.push(v.to_string_lossy().into_owned()),
            _ => panic!("unexpected argument: {arg:?}"),
        }
    }
    if !format(&paths, check) {
        std::process::exit(1);
    }
}

fn format(paths: &[String], check: bool) -> bool {
    let files = if paths.is_empty() {
        collect_md_files(Path::new("."))
    } else {
        let mut all = Vec::new();
        for p in paths {
            let path = Path::new(p);
            if path.is_dir() {
                all.extend(collect_md_files(path));
            } else if path.extension().and_then(|s| s.to_str()) == Some("md") {
                all.push(path.to_path_buf());
            } else {
                eprintln!(
                    "error: refusing to format non-Markdown file {} (only *.md files are supported)",
                    path.display()
                );
                return false;
            }
        }
        all
    };

    let config = ConfigurationBuilder::new().build();
    let mut had_changes = false;
    let mut had_errors = false;

    for path in &files {
        let original = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error reading {}: {e}", path.display());
                had_errors = true;
                continue;
            }
        };

        let result = format_text(&original, &config, |_, _, _| Ok(None));
        match result {
            Ok(None) => {}
            Ok(Some(formatted)) => {
                had_changes = true;
                if check {
                    println!("would reformat: {}", path.display());
                } else if let Err(e) = std::fs::write(path, &formatted) {
                    eprintln!("error writing {}: {e}", path.display());
                    had_errors = true;
                } else {
                    println!("formatted: {}", path.display());
                }
            }
            Err(e) => {
                eprintln!("error formatting {}: {e}", path.display());
                had_errors = true;
            }
        }
    }

    !had_errors && (!check || !had_changes)
}

fn collect_md_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if DEFAULT_EXCLUDED_DIRS.contains(&name) {
                continue;
            }
            walk(&path, out);
        } else if file_type.is_file() && path.extension().and_then(|s| s.to_str()) == Some("md") {
            out.push(path);
        }
    }
}
