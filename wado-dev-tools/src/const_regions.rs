//! `const-regions`: census of the compile-time regions that reach the final
//! IR, over the benchmark and wasm-size corpora and the packages written in
//! Wado, grouped by cause. Prints Markdown.
//!
//! Each file compiles under the dev `wado`, a process apiece: the packages
//! resolve their dependencies through the CLI's manifest loader, which this
//! crate does not link.

use std::cmp::Reverse;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::thread;

use wado_compiler::hashmap::IndexMap;

const CORPUS_DIRS: &[&str] = &["benchmark", "wasm-size"];

/// The remark `remarks.rs` emits for a region left to run time.
const REGION_REMARK: &str = "computes a constant at run time: ";

/// The remarks a compile emits, or the first error line when it did not build.
type Outcome = Result<Vec<String>, String>;

pub fn run(mut parser: lexopt::Parser) {
    if let Some(arg) = parser.next().expect("failed to parse args") {
        panic!("unexpected argument: {arg:?}");
    }
    let wado = dev_wado();
    let files = corpus_files();
    assert!(!files.is_empty(), "the corpus must hold a file");
    let outcomes = compile_all(&wado, &files);

    let mut by_cause: IndexMap<String, usize> = IndexMap::default();
    let mut by_file: Vec<(&str, usize)> = Vec::new();
    let mut skipped: Vec<(&str, &str)> = Vec::new();
    for (file, outcome) in files.iter().zip(&outcomes) {
        let remarks = match outcome {
            Ok(remarks) => remarks,
            Err(error) => {
                skipped.push((file, error));
                continue;
            }
        };
        let causes: Vec<&str> = remarks.iter().filter_map(|r| region_cause(r)).collect();
        if !causes.is_empty() {
            by_file.push((file, causes.len()));
        }
        for name in causes.into_iter().flat_map(blamed) {
            *by_cause.entry(name.to_string()).or_default() += 1;
        }
    }

    let total: usize = by_file.iter().map(|(_, n)| n).sum();
    println!("# Compile-time regions surviving to the final IR\n");
    println!(
        "{total} across {} of {} files.\n",
        by_file.len(),
        files.len() - skipped.len()
    );
    let mut by_cause: Vec<(String, usize)> = by_cause.into_iter().collect();
    print_table("Cause", &mut by_cause);
    println!();
    print_table("File", &mut by_file);
    if !skipped.is_empty() {
        println!("\n## Not compiled\n");
        for (file, error) in skipped {
            println!("- `{file}` — {error}");
        }
    }
}

/// The dev `wado`, built first so the census never reads an older compiler.
fn dev_wado() -> String {
    let output = Command::new("scripts/dev-bin.sh")
        .args(["--path", "wado"])
        .output()
        .unwrap_or_else(|e| panic!("running 'scripts/dev-bin.sh': {e}"));
    assert!(
        output.status.success(),
        "building wado failed: {}",
        output.status
    );
    String::from_utf8(output.stdout)
        .expect("dev-bin.sh prints the path as UTF-8")
        .trim_end()
        .to_string()
}

/// Every `.wado` directly inside each directory under [`CORPUS_DIRS`], plus
/// `package-*/src/*.wado`, sorted.
fn corpus_files() -> Vec<String> {
    let mut dirs: Vec<String> = CORPUS_DIRS
        .iter()
        .flat_map(|root| entries(root))
        .filter(|path| Path::new(path).is_dir())
        .collect();
    dirs.extend(
        entries(".")
            .into_iter()
            .filter_map(|path| {
                let name = path.strip_prefix("./")?;
                name.starts_with("package-").then(|| format!("{name}/src"))
            })
            .filter(|src| Path::new(src).is_dir()),
    );
    let mut files: Vec<String> = dirs
        .iter()
        .flat_map(|dir| entries(dir))
        .filter(|path| path.ends_with(".wado") && Path::new(path).is_file())
        .collect();
    files.sort();
    files
}

/// Each entry of `dir`, as a path that starts with it.
fn entries(dir: &str) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading '{dir}': {e}"))
        .map(|entry| {
            let name = entry.expect("reading a directory entry").file_name();
            format!("{dir}/{}", name.to_string_lossy())
        })
        .collect()
}

/// Compile every file, one per CPU at a time, keeping the corpus order.
fn compile_all(wado: &str, files: &[String]) -> Vec<Outcome> {
    let next = Mutex::new(files.iter().enumerate());
    let outcomes: Mutex<Vec<Option<Outcome>>> = Mutex::new(vec![None; files.len()]);
    let workers = thread::available_parallelism().map_or(1, usize::from);
    let (next, outcomes_ref) = (&next, &outcomes);
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(move || {
                while let Some((i, file)) = next.lock().unwrap().next() {
                    let outcome = compile(wado, file);
                    outcomes_ref.lock().unwrap()[i] = Some(outcome);
                }
            });
        }
    });
    outcomes
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|o| o.expect("every file compiles once"))
        .collect()
}

fn compile(wado: &str, file: &str) -> Outcome {
    let output = Command::new(wado)
        .args(["compile", "-O2", "--log-level", "info", "-o", "/dev/null"])
        .arg(file)
        .output()
        .unwrap_or_else(|e| panic!("running '{wado}': {e}"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() {
        return Ok(stderr
            .lines()
            .filter(|l| l.contains("remark:"))
            .map(str::to_string)
            .collect());
    }
    // A file that is not an entry point and one the compiler rejects both land
    // here, and only the first is expected: the line tells them apart.
    Err(stderr
        .lines()
        .find(|l| l.contains("error:"))
        .map_or_else(|| output.status.to_string(), |l| l.trim().to_string()))
}

fn region_cause(remark: &str) -> Option<&str> {
    remark
        .find(REGION_REMARK)
        .map(|at| remark[at + REGION_REMARK.len()..].trim())
}

/// What a region's cause blames: the calls that still run, each by its
/// backticked name, or else the refusal as a whole.
fn blamed(cause: &str) -> Vec<&str> {
    if !cause.ends_with("still runs here") {
        return vec![cause];
    }
    cause.split('`').skip(1).step_by(2).collect()
}

/// A `## By <noun>` table, its rows sorted by count, descending.
fn print_table<K: AsRef<str>>(noun: &str, rows: &mut [(K, usize)]) {
    rows.sort_by_key(|row| Reverse(row.1));
    println!("## By {}\n", noun.to_lowercase());
    println!("| {noun} | Regions |");
    println!("| {} | ------- |", "-".repeat(noun.len()));
    for (key, n) in rows.iter() {
        println!("| `{}` | {n} |", key.as_ref());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_still_running_call_blames_each_backticked_name() {
        assert_eq!(blamed("`f`, `g` still runs here"), ["f", "g"]);
    }

    #[test]
    fn a_refusal_blames_itself() {
        assert_eq!(
            blamed("no call on its path explains it"),
            ["no call on its path explains it"]
        );
    }

    #[test]
    fn only_a_region_remark_has_a_cause() {
        assert_eq!(
            region_cause("a.wado:1:2: remark: this block computes a constant at run time: x "),
            Some("x"),
        );
        assert_eq!(region_cause("a.wado:1:2: remark: something else"), None);
    }
}
