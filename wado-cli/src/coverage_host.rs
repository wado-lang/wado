//! `wado test --coverage`: the `core:coverage` host each test store records its
//! hits through, and the run-wide report those hits and the components' plans
//! make. See WEP 2026-09-28 (Test Coverage).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use wado_compiler::coverage::{
    self, Coverage, CoverageScope, DecodedPlans, FileCoverage, PlanMismatch, Region, Registered,
};
#[cfg(debug_assertions)]
use wado_compiler::stdlib::DEV_STDLIB_ROOT;
use wado_compiler::stdlib::core_module_file;
use wasmtime::component::{HasSelf, Linker};

use crate::args::CliExit;
use crate::compile::build_dir;
use crate::sync::lock;
use crate::test_report::TestReporter;

wasmtime::component::bindgen!({
    inline: "package wado:coverage-runner; world runner { import core:coverage/coverage-host@0.1.0; }",
    path: "../wado-compiler/lib/core/coverage",
    world: "wado:coverage-runner/runner",
});

use self::core::coverage::coverage_host;

/// The regions one test store reported, by global id.
#[derive(Debug, Default)]
pub struct CoverageHits(pub BTreeSet<u32>);

impl coverage_host::Host for CoverageHits {
    fn hit(&mut self, id: u32) {
        self.0.insert(id);
    }
}

/// Link `core:coverage` into a test linker, reaching each store's hits through
/// `hits`.
///
/// # Errors
///
/// Returns an error if the linker already defines the interface.
pub fn add_to_linker<T: Send + 'static>(
    linker: &mut Linker<T>,
    hits: fn(&mut T) -> &mut CoverageHits,
) -> wasmtime::Result<()> {
    coverage_host::add_to_linker::<T, HasSelf<CoverageHits>>(linker, hits)
}

/// The reports `--coverage` writes to `build/coverage/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoverageFormats {
    pub lcov: bool,
    pub json: bool,
    /// `baseline.json`, in the form `--coverage-baseline` reads.
    pub baseline: bool,
}

impl CoverageFormats {
    /// Parse `--coverage=lcov,json,baseline`.
    ///
    /// # Errors
    ///
    /// Returns the format no report is written as.
    pub fn parse(list: &str) -> Result<Self, CliExit> {
        let mut formats = Self {
            lcov: false,
            json: false,
            baseline: false,
        };
        for format in list.split(',') {
            match format {
                "lcov" => formats.lcov = true,
                "json" => formats.json = true,
                "baseline" => formats.baseline = true,
                other => {
                    return Err(CliExit::error(format!(
                        "--coverage takes lcov, json and baseline, got '{other}'"
                    )));
                }
            }
        }
        Ok(formats)
    }
}

impl Default for CoverageFormats {
    fn default() -> Self {
        Self {
            lcov: true,
            json: false,
            baseline: false,
        }
    }
}

/// Parse `--coverage-include=deps,stdlib`.
///
/// # Errors
///
/// Returns the word that names nothing to measure.
pub fn parse_coverage_include(list: &str) -> Result<CoverageScope, CliExit> {
    let mut scope = CoverageScope::default();
    for word in list.split(',') {
        match word {
            "deps" => scope.include_deps = true,
            "stdlib" => scope.stdlib = true,
            other => {
                return Err(CliExit::error(format!(
                    "--coverage-include takes deps and stdlib, got '{other}'"
                )));
            }
        }
    }
    Ok(scope)
}

/// What `--coverage` asked for.
#[derive(Debug, Clone, Default)]
pub struct CoverageOptions {
    pub scope: CoverageScope,
    pub formats: CoverageFormats,
    /// `--coverage-baseline`: the uncovered regions the run must match.
    pub baseline: Option<PathBuf>,
}

/// The plans and hits a run has seen so far.
pub struct CoverageRun {
    options: CoverageOptions,
    /// Report paths are relative to this directory, the current one.
    root: PathBuf,
    coverage: Mutex<Result<Coverage, PlanMismatch>>,
}

impl CoverageRun {
    /// # Errors
    ///
    /// Returns an error when the current directory cannot be read.
    pub fn new(options: CoverageOptions) -> Result<Self, CliExit> {
        let root = std::env::current_dir()
            .and_then(|dir| dir.canonicalize())
            .map_err(|e| CliExit::error(format!("reading the current directory: {e}")))?;
        Ok(Self {
            options,
            root,
            coverage: Mutex::new(Ok(Coverage::default())),
        })
    }

    /// Add the plans of the component compiled from `entry`: where the hits
    /// of each land, or none once plans have disagreed.
    pub fn register(&self, entry: &str, plans: &DecodedPlans) -> Vec<Registered> {
        let base = Path::new(entry).parent().unwrap_or(Path::new(""));
        let mut guard = lock(&self.coverage);
        let Ok(coverage) = guard.as_mut() else {
            return Vec::new();
        };
        coverage
            .register(plans, |path| self.report_path(base, path))
            .unwrap_or_else(|mismatch| {
                *guard = Err(mismatch);
                Vec::new()
            })
    }

    /// Record what `test` hit in the plans `registered` names.
    pub fn record(&self, registered: &[Registered], hits: &BTreeSet<u32>, test: &str) {
        if let Ok(coverage) = lock(&self.coverage).as_mut() {
            coverage.record(registered, hits, Some(test));
        }
    }

    /// The path a module's source is reported under: relative to the run's
    /// root where it lies under it. A plan names a file relative to its entry
    /// module's directory, `base`, and a `core:` module by its import path.
    fn report_path(&self, base: &Path, path: &str) -> String {
        let file = match core_module_file(path) {
            Some(file) => match dev_stdlib_file(file) {
                Some(file) => file,
                None => return path.to_string(),
            },
            None => base.join(path),
        };
        let Ok(absolute) = file.canonicalize() else {
            return path.to_string();
        };
        absolute
            .strip_prefix(&self.root)
            .map_or(absolute.as_path(), |rel| rel)
            .display()
            .to_string()
    }

    /// Report the summary through `reporter` and write the report files.
    ///
    /// # Errors
    ///
    /// Returns an error when plans for one path disagree, or a report cannot
    /// be written.
    pub(crate) fn finish(self, reporter: &dyn TestReporter) -> Result<(), CliExit> {
        let options = &self.options;
        let out_dir = &build_dir(&self.root).join("coverage");
        let coverage = self
            .coverage
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .map_err(|mismatch| {
                CliExit::error(format!(
                    "coverage: {} changed during the run; its plans disagree",
                    mismatch.path
                ))
            })?;
        reporter.on_coverage(&summary(&coverage));
        let write = |name: &str, text: String| -> Result<(), CliExit> {
            std::fs::create_dir_all(out_dir)
                .and_then(|()| std::fs::write(out_dir.join(name), text))
                .map_err(|e| CliExit::error(format!("coverage: writing {name}: {e}")))
        };
        if options.formats.lcov {
            write("lcov.info", coverage::to_lcov(&coverage))?;
        }
        if options.formats.json {
            write("coverage.json", to_json(&coverage))?;
        }
        let uncovered = uncovered_regions(&coverage);
        if options.formats.baseline {
            write("baseline.json", baseline_json(&uncovered))?;
        }
        match &options.baseline {
            Some(path) => check_baseline(&uncovered, path),
            None => Ok(()),
        }
    }
}

/// Where a dev build reads the stdlib `file` from.
#[cfg(debug_assertions)]
fn dev_stdlib_file(file: &str) -> Option<PathBuf> {
    Some(Path::new(DEV_STDLIB_ROOT).join(file))
}

/// A release build embeds the stdlib, so its modules are reported by import
/// path.
#[cfg(not(debug_assertions))]
fn dev_stdlib_file(_file: &str) -> Option<PathBuf> {
    None
}

/// The regions no test ran: by file, then by function, each as
/// `"<kind> <n>"`, the function's `n`th region. A function that never ran
/// lists its body alone, `"fn 0"`, since nothing in it ran either. A name
/// repeated within a file takes `#2`, `#3`, … in source order.
type Uncovered = BTreeMap<String, BTreeMap<String, Vec<String>>>;

fn uncovered_regions(coverage: &Coverage) -> Uncovered {
    let mut out = Uncovered::new();
    for (path, file) in &coverage.files {
        let mut by_function: Vec<Vec<(u32, &Region)>> = vec![Vec::new(); file.plan.functions.len()];
        for (id, region) in file.plan.regions.iter().enumerate() {
            by_function[region.function as usize].push((id as u32, region));
        }
        let mut seen: BTreeMap<&str, u32> = BTreeMap::new();
        for (function, regions) in file.plan.functions.iter().zip(&by_function) {
            let count = seen.entry(&function.name).or_default();
            *count += 1;
            let name = match *count {
                1 => function.name.clone(),
                n => format!("{}#{n}", function.name),
            };
            let (body, _) = regions[0];
            assert_eq!(
                body, function.region,
                "a function's body is its first region"
            );
            let ran = regions.iter().enumerate().map(|(n, (id, region))| {
                (
                    format!("{} {n}", region.kind.label()),
                    file.hit.contains(id),
                )
            });
            let missed: Vec<String> = if file.hit.contains(&body) {
                ran.filter(|(_, hit)| !hit)
                    .map(|(label, _)| label)
                    .collect()
            } else {
                ran.take(1).map(|(label, _)| label).collect()
            };
            if !missed.is_empty() {
                out.entry(path.clone()).or_default().insert(name, missed);
            }
        }
    }
    out
}

fn baseline_json(uncovered: &Uncovered) -> String {
    let mut text = serde_json::to_string_pretty(uncovered).expect("string keys");
    text.push('\n');
    text
}

/// Fail on any difference between the regions left unrun and the baseline
/// at `path`, in either direction, so the baseline only shrinks.
fn check_baseline(uncovered: &Uncovered, path: &Path) -> Result<(), CliExit> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CliExit::error(format!("coverage: reading {}: {e}", path.display())))?;
    let baseline: Uncovered = serde_json::from_str(&text)
        .map_err(|e| CliExit::error(format!("coverage: parsing {}: {e}", path.display())))?;
    let flatten = |map: &Uncovered| -> BTreeSet<String> {
        map.iter()
            .flat_map(|(file, functions)| {
                functions.iter().flat_map(move |(function, regions)| {
                    regions
                        .iter()
                        .map(move |region| format!("{file}: {function}: {region}"))
                })
            })
            .collect()
    };
    let (now, then) = (flatten(uncovered), flatten(&baseline));
    let new: Vec<&String> = now.difference(&then).collect();
    let gone: Vec<&String> = then.difference(&now).collect();
    if new.is_empty() && gone.is_empty() {
        return Ok(());
    }
    let mut message = format!("coverage: the run differs from {}", path.display());
    for region in new {
        let _ = write!(message, "\n  not run, not in the baseline: {region}");
    }
    for region in gone {
        let _ = write!(message, "\n  run now, remove from the baseline: {region}");
    }
    Err(CliExit::error(message))
}

/// Covered and total, as a report line spells them.
fn ratio(covered: usize, total: usize) -> String {
    format!("{covered}/{total}")
}

fn percent(covered: usize, total: usize) -> String {
    if total == 0 {
        return "100.0%".to_string();
    }
    format!("{:.1}%", covered as f64 * 100.0 / total as f64)
}

struct Totals {
    lines: (usize, usize),
    branches: (usize, usize),
    functions: (usize, usize),
}

fn totals(file: &FileCoverage) -> Totals {
    let lines = file.lines();
    let branches = file.branches();
    let functions = file.functions();
    Totals {
        lines: (lines.values().filter(|ran| **ran).count(), lines.len()),
        branches: (branches.iter().filter(|b| b.taken).count(), branches.len()),
        functions: (
            functions.iter().filter(|(_, ran)| *ran).count(),
            functions.len(),
        ),
    }
}

/// The summary `--coverage` reports after the run: the totals, then each file
/// that plans a function.
fn summary(coverage: &Coverage) -> String {
    let per_file: Vec<(&String, Totals)> = coverage
        .files
        .iter()
        .filter(|(_, file)| !file.plan.functions.is_empty())
        .map(|(path, file)| (path, totals(file)))
        .collect();
    let sum = |pick: fn(&Totals) -> (usize, usize)| {
        per_file.iter().fold((0, 0), |(c, t), (_, totals)| {
            let (fc, ft) = pick(totals);
            (c + fc, t + ft)
        })
    };
    let (lines, branches, functions) =
        (sum(|t| t.lines), sum(|t| t.branches), sum(|t| t.functions));
    let mut out = String::new();
    let _ = writeln!(
        out,
        "coverage: lines {} ({}), branches {} ({}), functions {} ({})",
        ratio(lines.0, lines.1),
        percent(lines.0, lines.1),
        ratio(branches.0, branches.1),
        percent(branches.0, branches.1),
        ratio(functions.0, functions.1),
        percent(functions.0, functions.1),
    );
    let width = per_file
        .iter()
        .map(|(path, _)| path.len())
        .max()
        .unwrap_or(0);
    for (path, t) in &per_file {
        let _ = writeln!(
            out,
            "  {path:<width$}  lines {:>9}  branches {:>9}  functions {:>7}",
            ratio(t.lines.0, t.lines.1),
            ratio(t.branches.0, t.branches.1),
            ratio(t.functions.0, t.functions.1),
        );
    }
    out
}

/// `coverage.json`: every file's regions with their spans and the tests that
/// ran them.
fn to_json(coverage: &Coverage) -> String {
    use serde_json::{Value, json};

    let files: Vec<Value> = coverage
        .files
        .iter()
        .map(|(path, file)| {
            let t = totals(file);
            let regions: Vec<Value> = file
                .plan
                .regions
                .iter()
                .enumerate()
                .map(|(index, region)| {
                    let index = index as u32;
                    let tests: Vec<&String> =
                        file.tests.get(&index).into_iter().flatten().collect();
                    json!({
                        "id": index,
                        "kind": region.kind.label(),
                        "function": file.plan.functions[region.function as usize].name,
                        "start": [region.start.line, region.start.column],
                        "end": [region.end.line, region.end.column],
                        "covered": file.hit.contains(&index),
                        "tests": tests,
                    })
                })
                .collect();
            json!({
                "path": path,
                "lines": { "covered": t.lines.0, "total": t.lines.1 },
                "branches": { "covered": t.branches.0, "total": t.branches.1 },
                "functions": { "covered": t.functions.0, "total": t.functions.1 },
                "uncovered_lines": file.uncovered_lines(),
                "regions": regions,
            })
        })
        .collect();
    let mut text = serde_json::to_string_pretty(&json!({ "files": files }))
        .expect("coverage JSON has no map with non-string keys");
    text.push('\n');
    text
}
