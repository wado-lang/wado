use std::any::Any;
use std::fmt::Write as _;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::FutureExt;
use futures::stream::{self, StreamExt};
use glob::Pattern;
use lexopt::Arg::Value;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::{Semaphore, mpsc};
use wado_compiler::hashmap::IndexMap;
use wasmtime::Engine;
use wasmtime::component::{Component, Linker};

use crate::args::{self, CliExit};
use crate::compile::{self, CompileFlags, OptLevel};
use crate::discover;
use crate::manifest as project_manifest;
use crate::runtime::{self, WasiState};
use wado_compiler::LogLevel;

const DEFAULT_TIMEOUT_MS: u64 = 5000;
// Coarse epoch interval to keep thread wake-ups down. Cost: up to 1s
// overshoot, which is fine because timeouts only fire on runaway tests.
const EPOCH_INTERVAL_MS: u64 = 1000;

pub struct TestOptions {
    /// First entry is the root package (label `"."` outside a `wado.toml`
    /// project); later entries are sub-packages discovered recursively.
    pub package_runs: Vec<PackageRun>,
    pub jobs: usize,
    pub opt_level: OptLevel,
    pub log_level: LogLevel,
    pub inline_threshold: Option<usize>,
    pub opt_iterations: Option<u32>,
    /// `None` lets the compiler auto-select the debug allocator for the
    /// `test` world; `--allocator` overrides.
    pub allocator: Option<String>,
    pub preopened_dirs: Vec<(String, String)>,
    /// `--no-run`: compile every file (which still refreshes
    /// `<primary>.kiln.json` as a side-effect) but skip wasmtime execution.
    pub no_run: bool,
}

impl TestOptions {
    // Pin `target_world` to `test` so test functions become exports and
    // DCE prunes everything else.
    fn compile_flags(&self) -> CompileFlags {
        CompileFlags {
            opt_level: self.opt_level,
            log_level: self.log_level,
            target_world: Some("test".to_string()),
            skip_validation: false,
            inline_threshold: self.inline_threshold,
            opt_iterations: self.opt_iterations,
            allocator: self.allocator.clone(),
        }
    }
}

pub struct PackageRun {
    /// `"."` for the root, `"subpkg"` for a nested sub-package.
    pub label: String,
    pub paths: Vec<String>,
}

#[derive(Clone, Copy)]
enum Opt {
    Filter,
    Exclude,
    Parallel,
    OptLevel,
    InlineThreshold,
    OptIterations,
    LogLevel,
    Allocator,
    Dir,
    NoDir,
    NoRun,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[
        Self::Filter,
        Self::Exclude,
        Self::Parallel,
        Self::OptLevel,
        Self::InlineThreshold,
        Self::OptIterations,
        Self::LogLevel,
        Self::Allocator,
        Self::Dir,
        Self::NoDir,
        Self::NoRun,
        Self::Help,
    ];

    const fn spec(self) -> args::OptSpec {
        match self {
            Self::Filter => args::OptSpec {
                long: Some("filter"),
                short: Some('f'),
                value: Some("<pattern>"),
                desc: "Keep only files whose path matches the wildcard pattern (`*`, `?`, `[...]`)",
            },
            Self::Exclude => args::OptSpec {
                long: Some("exclude"),
                short: None,
                value: Some("<pattern>"),
                desc: "Drop files whose path matches the glob (repeatable; extends [test].exclude)",
            },
            Self::Parallel => args::OptSpec {
                long: Some("parallel"),
                short: Some('p'),
                value: Some("<N>"),
                desc: "Number of parallel workers (default: num CPUs)",
            },
            Self::OptLevel => args::OPT_LEVEL_SPEC,
            Self::InlineThreshold => args::INLINE_THRESHOLD_SPEC,
            Self::OptIterations => args::OPT_ITERATIONS_SPEC,
            Self::LogLevel => args::LOG_LEVEL_SPEC,
            Self::Allocator => args::ALLOCATOR_SPEC,
            Self::Dir => args::DIR_SPEC,
            Self::NoDir => args::NO_DIR_SPEC,
            Self::NoRun => args::OptSpec {
                long: Some("no-run"),
                short: None,
                value: None,
                desc: "Compile (and refresh Kiln caches) but skip the wasmtime execution phase",
            },
            Self::Help => args::HELP_SPEC,
        }
    }
}

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado test [options] [files or directories...]").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Discovers **/*.wado under the project root (honours .gitignore, .gitmodules,"
    )
    .unwrap();
    writeln!(
        buf,
        "dot-prefixed entries, nested wado.toml, and [test].exclude in wado.toml)."
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    write!(buf, "{}", args::format_opts_help(Opt::ALL, |o| o.spec())).unwrap();
    buf
}

pub fn print_usage() {
    eprint!("{}", format_usage());
}

/// Cargo-workspace-style walk: one `PackageRun` for `root`, plus one per
/// transitively discovered sub-package. Returned root-first.
///
/// `apply_manifest_excludes` gates the root's `[test].exclude`
/// (sub-packages always apply their own). `extra_excludes` (CLI
/// `--exclude`) apply per-package and are matched relative to the package
/// being walked.
fn discover_packages(
    root: &Path,
    apply_manifest_excludes: bool,
    extra_excludes: &[String],
) -> Result<Vec<PackageRun>, CliExit> {
    let invocation_root = root.to_path_buf();
    let mut runs: Vec<PackageRun> = Vec::new();
    let mut queue: Vec<PathBuf> = Vec::new();

    let (root_exclude_manifest, root_include_manifest) = if apply_manifest_excludes {
        package_manifest_test_filters(&invocation_root)?
    } else {
        (Vec::new(), Vec::new())
    };
    let root_excludes = compile_excludes(&root_exclude_manifest, extra_excludes)?;
    let root_includes = compile_includes(&root_include_manifest)?;
    walk_into(
        &invocation_root,
        &invocation_root,
        &root_excludes,
        &root_includes,
        &mut runs,
        &mut queue,
    )?;

    while let Some(pkg_root) = queue.pop() {
        let (exclude_manifest, include_manifest) = package_manifest_test_filters(&pkg_root)?;
        let excludes = compile_excludes(&exclude_manifest, extra_excludes)?;
        let includes = compile_includes(&include_manifest)?;
        walk_into(
            &pkg_root,
            &invocation_root,
            &excludes,
            &includes,
            &mut runs,
            &mut queue,
        )?;
    }

    Ok(runs)
}

// `[test].exclude` (manifest) + `--exclude` (CLI) share the same shape so
// the walker treats them uniformly.
fn compile_excludes(
    manifest: &[String],
    extra_excludes: &[String],
) -> Result<discover::ExcludeSet, CliExit> {
    let combined: Vec<&str> = manifest
        .iter()
        .map(String::as_str)
        .chain(extra_excludes.iter().map(String::as_str))
        .collect();
    discover::ExcludeSet::compile(&combined).map_err(CliExit::error)
}

// `[test].include` is manifest-only (no CLI flag yet) and lets stdlib-style
// packages keep `*_test.wado` files visible inside an excluded directory.
fn compile_includes(manifest: &[String]) -> Result<discover::IncludeSet, CliExit> {
    discover::IncludeSet::compile(manifest).map_err(CliExit::error)
}

fn walk_into(
    pkg_root: &Path,
    invocation_root: &Path,
    excludes: &discover::ExcludeSet,
    includes: &discover::IncludeSet,
    runs: &mut Vec<PackageRun>,
    queue: &mut Vec<PathBuf>,
) -> Result<(), CliExit> {
    let result =
        discover::discover_test_files(pkg_root, excludes, includes).map_err(CliExit::error)?;
    let label = relative_label(invocation_root, pkg_root);
    let paths = result.files.iter().map(|p| display_path(p)).collect();
    runs.push(PackageRun { label, paths });
    for sub in result.subpackages.into_iter().rev() {
        queue.push(sub);
    }
    Ok(())
}

// Returns `(exclude, include)` from the `wado.toml` rooted exactly at
// `pkg_root`. Sub-packages without their own manifest contribute nothing.
fn package_manifest_test_filters(pkg_root: &Path) -> Result<(Vec<String>, Vec<String>), CliExit> {
    match project_manifest::discover(pkg_root) {
        Ok(Some(project)) if project.root == pkg_root => {
            Ok((project.manifest.test.exclude, project.manifest.test.include))
        }
        Ok(_) => Ok((Vec::new(), Vec::new())),
        Err(e) => Err(CliExit::error(e)),
    }
}

fn relative_label(invocation_root: &Path, pkg_root: &Path) -> String {
    if pkg_root == invocation_root {
        return ".".to_string();
    }
    pkg_root.strip_prefix(invocation_root).map_or_else(
        |_| pkg_root.display().to_string(),
        |rel| rel.display().to_string(),
    )
}

// Canonical path shape: forward-slashes, no leading `./`, regardless of
// OS or invocation root. `--filter` and `[test].exclude` patterns are
// documented as forward-slash globs and rely on this.
fn display_path(p: &Path) -> String {
    let raw = p.display().to_string();
    let normalised = if cfg!(windows) {
        raw.replace('\\', "/")
    } else {
        raw
    };
    match normalised.strip_prefix("./") {
        Some(stripped) => stripped.to_string(),
        None => normalised,
    }
}

// Directories are walked with the discovery rules; files pass through.
fn resolve_paths(paths: Vec<String>, extra_excludes: &[String]) -> Result<Vec<String>, CliExit> {
    let excludes = discover::ExcludeSet::compile(extra_excludes).map_err(CliExit::error)?;
    let mut resolved = Vec::new();
    for path in paths {
        let p = Path::new(&path);
        if p.is_dir() {
            let runs = discover_packages(p, true, extra_excludes)?;
            let total: usize = runs.iter().map(|r| r.paths.len()).sum();
            if total == 0 {
                return Err(CliExit::error(format!(
                    "no .wado files found in directory '{path}'"
                )));
            }
            for run in runs {
                resolved.extend(run.paths);
            }
        } else {
            // Canonicalize so explicit args and discovered paths match
            // filters/excludes against the same shape. `--exclude` applies
            // to explicit args too — the flag is documented as "drop files
            // whose path matches".
            let canonical = display_path(p);
            if !excludes.matches_str(&canonical) {
                resolved.push(canonical);
            }
        }
    }
    Ok(resolved)
}

pub fn parse_args(mut parser: lexopt::Parser) -> Result<TestOptions, CliExit> {
    let usage = format_usage();
    let mut paths: Vec<String> = Vec::new();
    let mut filters: Vec<String> = Vec::new();
    let mut cli_excludes: Vec<String> = Vec::new();
    let mut jobs: Option<usize> = None;
    let mut opt_level = OptLevel::default();
    let mut log_level = LogLevel::default();
    let mut inline_threshold: Option<usize> = None;
    let mut opt_iterations: Option<u32> = None;
    let mut allocator: Option<String> = None;
    let mut preopened_dirs: Vec<(String, String)> = Vec::new();
    let mut explicit_dirs = false;
    let mut no_dir = false;
    let mut no_run = false;
    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Filter => {
                    filters.push(args::require_string(&mut parser)?);
                }
                Opt::Exclude => {
                    cli_excludes.push(args::require_string(&mut parser)?);
                }
                Opt::Parallel => {
                    let val = args::require_string(&mut parser)?;
                    match val.parse::<usize>() {
                        Ok(n) if n > 0 => jobs = Some(n),
                        _ => {
                            return Err(CliExit::error("--parallel requires a positive integer"));
                        }
                    }
                }
                Opt::OptLevel => opt_level = compile::parse_opt_level_arg(&mut parser)?,
                Opt::InlineThreshold => {
                    inline_threshold = Some(args::parse_inline_threshold_arg(
                        "--optimize-inline-threshold",
                        &mut parser,
                    )?);
                }
                Opt::OptIterations => {
                    opt_iterations = Some(args::parse_opt_iterations_arg(
                        "--optimize-iterations",
                        &mut parser,
                    )?);
                }
                Opt::LogLevel => log_level = args::parse_log_level_arg(&mut parser)?,
                Opt::Allocator => allocator = Some(args::require_string(&mut parser)?),
                Opt::Dir => {
                    preopened_dirs.push(args::parse_dir_arg(&mut parser)?);
                    explicit_dirs = true;
                }
                Opt::NoDir => no_dir = true,
                Opt::NoRun => no_run = true,
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(val) = arg {
            paths.push(val.to_string_lossy().into_owned());
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    // Default: preopen the current directory unless --dir or --no-dir was given.
    if !explicit_dirs && !no_dir {
        preopened_dirs.push((".".to_owned(), ".".to_owned()));
    }

    // No args ⇒ project-wide recursion through every nested `wado.toml`.
    // With explicit args, collapse everything into one synthetic root run.
    let mut package_runs: Vec<PackageRun> = if paths.is_empty() {
        discover_packages(Path::new("."), true, &cli_excludes)?
    } else {
        let resolved = resolve_paths(paths, &cli_excludes)?;
        vec![PackageRun {
            label: ".".to_string(),
            paths: resolved,
        }]
    };

    // `--filter` is a shell-style wildcard (`*foo*` to match anywhere) and
    // is repeatable: a path is kept if any pattern matches. It shares the
    // walker's match semantics (`discover::WALK_MATCH_OPTIONS`) with
    // `--exclude` and `[test].exclude`.
    if !filters.is_empty() {
        let patterns: Vec<Pattern> = filters
            .iter()
            .map(|s| {
                Pattern::new(s)
                    .map_err(|e| CliExit::error(format!("invalid --filter pattern {s:?}: {e}")))
            })
            .collect::<Result<_, _>>()?;
        for run in &mut package_runs {
            run.paths.retain(|p| {
                patterns
                    .iter()
                    .any(|pat| pat.matches_with(p, discover::WALK_MATCH_OPTIONS))
            });
        }
    }
    package_runs.retain(|run| !run.paths.is_empty());

    if package_runs.is_empty() {
        let message = if filters.is_empty() {
            "No .wado files found under the project root\n".to_owned()
        } else {
            format!("No .wado files match --filter {filters:?}\n")
        };
        return Err(CliExit {
            message,
            exit_code: 0,
        });
    }

    // Default to CPU count to saturate compile/execute. Load derives a
    // lower cap internally (Cranelift JIT is multi-threaded; see `run`).
    // `.max(2)` covers single-core environments.
    let jobs = jobs.unwrap_or_else(|| {
        let cpus = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        cpus.max(2)
    });

    Ok(TestOptions {
        package_runs,
        jobs,
        opt_level,
        log_level,
        inline_threshold,
        opt_iterations,
        allocator,
        preopened_dirs,
        no_run,
    })
}

/// Compile-stage output. Flows into the load stage, or — under `--no-run`
/// — is dropped after kiln-cache side-effects have been written.
struct CompiledArtifact {
    path: String,
    wasm: Vec<u8>,
}

/// Load-stage output: a module fully resident in wasmtime. Wrapped in
/// `Arc` so the execute stage can fan out test jobs without re-cloning
/// the heavy wasmtime objects.
///
/// The `Linker` is built once per engine; every test in the module
/// instantiates against it so the four `p3::add_to_linker` calls are
/// amortised across all tests in the module.
///
/// `_module_permit` caps simultaneously-live `Component`s — the heaviest
/// in-flight resource — independent of how channels between stages buffer.
struct LoadedModule {
    path: String,
    engine: Arc<Engine>,
    component: Arc<Component>,
    linker: Arc<Linker<WasiState>>,
    tests: Vec<ParsedTest>,
    _module_permit: OwnedSemaphorePermit,
}

/// Non-TODO module that failed `Component::new`/`create_linker` or whose
/// load worker panicked. Counted on the `load` axis; does not abort the run.
struct LoadFailure {
    path: String,
}

/// Pipeline-wide resource caps.
///
/// `cpu` caps total CPU-bound tasks across all three stages — every worker
/// acquires one permit. This makes `--parallel N` a truthful global cap
/// that per-stage buffer sizes can't multiply.
///
/// `modules` caps live wasmtime `Component`s independently of channel
/// buffering, bounding peak memory. The permit is held via
/// `LoadedModule::_module_permit` until execute-stage `Arc`s drop.
struct PipelineBudget {
    cpu: Arc<Semaphore>,
    modules: Arc<Semaphore>,
}

impl PipelineBudget {
    fn new(parallel_cap: usize, compile_jobs: usize, execute_jobs: usize) -> Self {
        let cpu_cap = parallel_cap.max(1);
        let modules_cap = compile_jobs.max(1) + execute_jobs.max(1);
        Self {
            cpu: Arc::new(Semaphore::new(cpu_cap)),
            modules: Arc::new(Semaphore::new(modules_cap)),
        }
    }
}

/// `record_input`/`record_output` mark the stage's output wall-clock span;
/// `add_work` accumulates per-worker durations — the latter is what reveals
/// bottlenecks in a well-overlapped pipeline (the streaming span is
/// indistinguishable from total wall-clock).
struct StageObserver {
    first_input_at: Mutex<Option<Instant>>,
    last_output_at: Mutex<Option<Instant>>,
    work_time: Mutex<Duration>,
}

impl StageObserver {
    fn new() -> Self {
        Self {
            first_input_at: Mutex::new(None),
            last_output_at: Mutex::new(None),
            work_time: Mutex::new(Duration::ZERO),
        }
    }

    fn record_input(&self) {
        let mut guard = lock_resilient(&self.first_input_at);
        if guard.is_none() {
            *guard = Some(Instant::now());
        }
    }

    fn record_output(&self) {
        *lock_resilient(&self.last_output_at) = Some(Instant::now());
    }

    fn add_work(&self, d: Duration) {
        *lock_resilient(&self.work_time) += d;
    }

    fn work_time(&self) -> Duration {
        *lock_resilient(&self.work_time)
    }
}

// Plain `lock().unwrap()` would propagate poison — and the EpochTicker
// thread silently dies with it, disabling timeout enforcement. None of
// these mutexes guard logically-coupled invariants, so just recover.
fn lock_resilient<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// Recovers `&'static str` and `String` payloads (what `panic!` produces);
// anything else collapses to a placeholder.
fn format_panic_payload(payload: &Box<dyn Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

struct TestJob {
    module: Arc<LoadedModule>,
    test_name: String,
    display_name: String,
    expect_trap: bool,
    is_todo: bool,
    timeout_ms: u64,
}

/// Regular tests are Pass/Fail. TODO tests live on a separate axis:
/// `TodoPending` trapped as expected, `TodoResolved` passed unexpectedly
/// (consider dropping the `#[TODO]`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum TestOutcome {
    Pass,
    Fail,
    TodoPending,
    TodoResolved,
}

struct TestResult {
    file_path: String,
    test_name: String,
    display_name: String,
    outcome: TestOutcome,
    error: Option<String>,
    duration: Duration,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TestKind {
    Normal,
    ExpectTrap,
    Todo,
}

/// Parser for `test-(trap-|todo-)?(tm{N}-)?{index}(-{name})?` exports.
/// Tolerant of two edges so a stray name doesn't drop a test from the
/// run: malformed `tm…-` (e.g. `tmfoo-…`) falls into the index/name
/// branch, and trailing-dash names (`test-0-`) render as `<test {index}>`.
#[derive(Debug, PartialEq, Eq)]
struct TestExportName {
    kind: TestKind,
    timeout_ms: Option<u64>,
    /// Human-readable display: the `name` segment with `-` → `_`,
    /// or `<test {index}>` when no name was provided.
    display: String,
}

/// Parse a `test-…` component export into its kind, timeout, and display name.
///
/// Returns `None` if the name doesn't begin with the `test-` family of prefixes,
/// so callers can use it both as a filter and as a parser.
fn parse_test_export(name: &str) -> Option<TestExportName> {
    let (kind, rest) = if let Some(rest) = name.strip_prefix("test-trap-") {
        (TestKind::ExpectTrap, rest)
    } else if let Some(rest) = name.strip_prefix("test-todo-") {
        (TestKind::Todo, rest)
    } else if let Some(rest) = name.strip_prefix("test-") {
        (TestKind::Normal, rest)
    } else {
        return None;
    };

    let (timeout_ms, rest) = parse_timeout_segment(rest);

    let display = match rest.find('-') {
        Some(idx) if idx + 1 < rest.len() => rest[idx + 1..].replace('-', "_"),
        // No `-` at all, or a trailing `-` with no name segment: fall
        // back to the bare-index display so we never produce an empty
        // string.
        Some(idx) => format!("<test {}>", &rest[..idx]),
        None => format!("<test {rest}>"),
    };

    Some(TestExportName {
        kind,
        timeout_ms,
        display,
    })
}

/// Strip an optional `tm{N}-` segment, returning the parsed timeout (if any)
/// and the remainder. A malformed `tm…` (no trailing dash, non-numeric N) is
/// treated as part of the name rather than a timeout.
fn parse_timeout_segment(rest: &str) -> (Option<u64>, &str) {
    let Some(after_tm) = rest.strip_prefix("tm") else {
        return (None, rest);
    };
    let Some(dash) = after_tm.find('-') else {
        return (None, rest);
    };
    let Ok(n) = after_tm[..dash].parse::<u64>() else {
        return (None, rest);
    };
    (Some(n), &after_tm[dash + 1..])
}

/// A `#![TODO]` module that failed to compile.
/// Treated as a passing result since the module is expected to have errors.
struct TodoCompileError {
    path: String,
}

/// A non-TODO module that failed to compile.
/// Counted on the `compile` axis of the summary; does not abort the run so
/// other files still get a chance to compile and report.
struct CompileFailure {
    path: String,
}

/// Per-file outcome from the **compile** stage.
enum CompileOutcome {
    Compiled(CompiledArtifact),
    TodoCompileError(TodoCompileError),
    CompileFailure(CompileFailure),
}

/// Per-file outcome from the **load** stage.
enum LoadOutcome {
    Loaded(LoadedModule),
    LoadFailure(LoadFailure),
}

/// One discovered test export, paired with its raw export name.
struct ParsedTest {
    export_name: String,
    parsed: TestExportName,
}

/// **Stage 1 worker** — run the wado compiler over one source file and
/// emit the wasm bytes. No wasmtime touched here, so `--no-run` can cut
/// the pipeline immediately after this stage and pay zero Cranelift cost.
///
/// Wrapped in `catch_unwind` so a panic in `compile::try_compile`
/// (allocator OOM, a real compiler bug surfacing on one input) is
/// downgraded to a per-file `CompileFailure` — the other fixtures in
/// the run still get a chance to compile and report.
///
/// Per-file log lines are emitted as compilation finishes; under parallel
/// scheduling they interleave across workers, but the leading `[elapsed]`
/// prefix records actual completion time.
async fn compile_artifact(
    path: String,
    flags: Arc<CompileFlags>,
    overall_start: Instant,
    observer: Arc<StageObserver>,
) -> CompileOutcome {
    let compile_start = Instant::now();
    let panic_or_result = AssertUnwindSafe(compile::try_compile(&path, &flags))
        .catch_unwind()
        .await;
    let compile_duration = compile_start.elapsed();
    observer.add_work(compile_duration);
    let elapsed = format_duration(overall_start.elapsed());
    let dur = format_duration(compile_duration);

    let compile_result = match panic_or_result {
        Ok(r) => r,
        Err(payload) => {
            let cause = format_panic_payload(&payload);
            eprintln!("[{elapsed}] FAILED to compile {path} ({dur}): panicked: {cause}");
            return CompileOutcome::CompileFailure(CompileFailure { path });
        }
    };

    match compile_result {
        Ok(result) => {
            println!("[{elapsed}] Compiled {path} ({dur})");
            CompileOutcome::Compiled(CompiledArtifact {
                path,
                wasm: result.wasm,
            })
        }
        Err(failure) if failure.is_todo_module => {
            println!("[{elapsed}] Compiled {path} (TODO module, compile error expected, {dur})");
            CompileOutcome::TodoCompileError(TodoCompileError { path })
        }
        Err(_) => {
            eprintln!("[{elapsed}] FAILED to compile {path} ({dur})");
            CompileOutcome::CompileFailure(CompileFailure { path })
        }
    }
}

/// **Stage 2 worker** — hand the wasm bytes to wasmtime: create an
/// `Engine`, AOT-compile the `Component`, build the WASI P3 `Linker`,
/// and discover the test exports. This is the expensive Cranelift step
/// — kept off the critical path of `--no-run` and of stage 1.
///
/// `module_permit` is acquired by the caller (so the wait is observable
/// as backpressure on the load channel, not as inflated `load_duration`)
/// and moved into the resulting `LoadedModule` so the budget slot is
/// held for the module's entire downstream lifetime.
///
/// Wrapped in `catch_unwind` so a `Component::new` panic on malformed
/// wasm (debug-build assertions in wasmtime/cranelift) becomes a per-file
/// `LoadFailure` rather than aborting the whole pipeline.
fn load_module(
    artifact: CompiledArtifact,
    opt_level: wasmtime::OptLevel,
    overall_start: Instant,
    module_permit: OwnedSemaphorePermit,
    observer: Arc<StageObserver>,
) -> LoadOutcome {
    let load_start = Instant::now();
    let panic_or_result = std::panic::catch_unwind(AssertUnwindSafe(|| -> Result<_> {
        let engine = Arc::new(runtime::create_test_engine(opt_level)?);
        let component = Arc::new(Component::new(&engine, &artifact.wasm)?);
        let linker = Arc::new(runtime::create_linker(&engine)?);
        Ok((engine, component, linker))
    }));
    let load_duration = load_start.elapsed();
    observer.add_work(load_duration);
    let elapsed = format_duration(overall_start.elapsed());
    let load_dur = format_duration(load_duration);

    let load_result = match panic_or_result {
        Ok(r) => r,
        Err(payload) => {
            let cause = format_panic_payload(&payload);
            eprintln!(
                "[{elapsed}] FAILED to load {} ({load_dur}): panicked: {cause}",
                artifact.path
            );
            return LoadOutcome::LoadFailure(LoadFailure {
                path: artifact.path,
            });
        }
    };

    match load_result {
        Ok((engine, component, linker)) => {
            println!("[{elapsed}] Loaded {} ({load_dur})", artifact.path);

            let component_ty = component.component_type();
            let mut tests: Vec<ParsedTest> = Vec::new();
            for (name, _) in component_ty.exports(&engine) {
                if let Some(parsed) = parse_test_export(name) {
                    tests.push(ParsedTest {
                        export_name: name.to_string(),
                        parsed,
                    });
                }
            }

            LoadOutcome::Loaded(LoadedModule {
                path: artifact.path,
                engine,
                component,
                linker,
                tests,
                _module_permit: module_permit,
            })
        }
        Err(e) => {
            eprintln!(
                "[{elapsed}] FAILED to load {} ({load_dur}): {e}",
                artifact.path
            );
            LoadOutcome::LoadFailure(LoadFailure {
                path: artifact.path,
            })
        }
    }
}

/// Drive a `Receiver<T>` as a pinned `Stream<Item = T>` so the same
/// `buffer_unordered` pattern composes across all three stages.
///
/// `Pin<Box<…>>` is needed because `stream::unfold` produces a non-`Unpin`
/// stream (its state holds an async block), and `buffer_unordered`
/// requires its input stream to be `Unpin`. Boxing once at the receiver
/// boundary keeps the rest of the pipeline composable.
fn receiver_stream<T: Send + 'static>(
    rx: mpsc::Receiver<T>,
) -> Pin<Box<dyn stream::Stream<Item = T> + Send>> {
    Box::pin(stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|v| (v, rx))
    }))
}

/// **Stage 1 driver** — fan paths out over the shared CPU budget, run
/// the wado compiler on each, and route each per-file outcome to its
/// sink. Returns when the input is exhausted; the senders dropped on
/// return signal stage completion to downstream stages.
///
/// The compile future borrows `Cell`/`RefCell` internals (non-Send), so
/// we can't `tokio::spawn` it onto the multi-thread runtime. Instead,
/// each file is driven by a per-thread current-thread runtime on the
/// blocking pool — the non-Send state never crosses threads.
///
/// Returns `(stage_wall_clock, compiled_count)`. `compiled_count`
/// counts every `CompileOutcome::Compiled` emitted (irrespective of
/// what happens downstream), so the caller can compute `compile_ok`
/// additively even when `--no-run` drains the load stage.
async fn run_compile_stage(
    paths: Vec<String>,
    flags: Arc<CompileFlags>,
    parallelism: usize,
    cpu_budget: Arc<Semaphore>,
    overall_start: Instant,
    observer: Arc<StageObserver>,
    artifact_tx: mpsc::Sender<CompiledArtifact>,
    todo_tx: mpsc::Sender<TodoCompileError>,
    cfail_tx: mpsc::Sender<CompileFailure>,
) -> usize {
    let mut compiled_count = 0_usize;

    let mut stream = stream::iter(paths)
        .map(|path| {
            observer.record_input();
            let flags = Arc::clone(&flags);
            let observer_inner = Arc::clone(&observer);
            let cpu_budget = Arc::clone(&cpu_budget);
            // Spawn each per-fixture worker as an independent task so
            // its progress is not gated by `buffer_unordered`'s outer
            // polling state. If the outer loop is briefly parked on
            // `artifact_tx.send` (channel full), the spawned tasks
            // still progress and release their CPU permit on completion.
            tokio::spawn(async move {
                // Acquire one CPU permit (shared across all three
                // stages) — held for the entire compile body so the
                // pipeline's total in-flight CPU work is `≤ cpus`,
                // regardless of how compile/load/execute overlap.
                let _cpu_permit = cpu_budget
                    .acquire_owned()
                    .await
                    .expect("cpu semaphore closed");
                let path_for_failure = path.clone();
                tokio::task::spawn_blocking(move || {
                    let panic_or_runtime = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                    }));
                    match panic_or_runtime {
                        Ok(Ok(rt)) => rt.block_on(compile_artifact(
                            path,
                            flags,
                            overall_start,
                            observer_inner,
                        )),
                        Ok(Err(e)) => {
                            let elapsed = format_duration(overall_start.elapsed());
                            eprintln!(
                                "[{elapsed}] FAILED to compile {path}: \
                                 unable to create compile runtime: {e}"
                            );
                            CompileOutcome::CompileFailure(CompileFailure { path })
                        }
                        Err(payload) => {
                            let cause = format_panic_payload(&payload);
                            let elapsed = format_duration(overall_start.elapsed());
                            eprintln!(
                                "[{elapsed}] FAILED to compile {path}: \
                                 compile worker panicked: {cause}"
                            );
                            CompileOutcome::CompileFailure(CompileFailure { path })
                        }
                    }
                })
                .await
                .unwrap_or_else(|join_err| {
                    let elapsed = format_duration(overall_start.elapsed());
                    eprintln!(
                        "[{elapsed}] FAILED to compile {path_for_failure}: \
                         blocking-task join error: {join_err}"
                    );
                    CompileOutcome::CompileFailure(CompileFailure {
                        path: path_for_failure,
                    })
                })
            })
        })
        .buffer_unordered(parallelism.max(1));

    while let Some(join_result) = stream.next().await {
        let outcome = join_result.expect("compile task panicked");
        match outcome {
            CompileOutcome::Compiled(a) => {
                compiled_count += 1;
                if artifact_tx.send(a).await.is_ok() {
                    observer.record_output();
                }
            }
            CompileOutcome::TodoCompileError(t) => {
                if todo_tx.send(t).await.is_ok() {
                    observer.record_output();
                }
            }
            CompileOutcome::CompileFailure(f) => {
                if cfail_tx.send(f).await.is_ok() {
                    observer.record_output();
                }
            }
        }
    }
    compiled_count
}

/// **Stage 2 driver** — consume `CompiledArtifact`s as they stream in
/// from stage 1, acquire a `modules` permit (capping live wasmtime
/// `Component`s globally), run `Component::new` under the shared CPU
/// budget, and route to the loaded/failure sinks. Each loaded engine is
/// registered with the epoch ticker (via `Weak<Engine>` so dropping the
/// module also removes its tick obligation).
///
/// The module permit is acquired BEFORE `spawn_blocking` so the wait
/// shows up as channel backpressure on stage 1, not as inflated load
/// duration. The permit is then moved into `LoadedModule` and lives
/// until the last `Arc<LoadedModule>` drops, releasing the slot for
/// the next module.
async fn run_load_stage(
    artifact_rx: mpsc::Receiver<CompiledArtifact>,
    opt_level: wasmtime::OptLevel,
    parallelism: usize,
    cpu_budget: Arc<Semaphore>,
    modules_budget: Arc<Semaphore>,
    overall_start: Instant,
    observer: Arc<StageObserver>,
    loaded_tx: mpsc::Sender<Arc<LoadedModule>>,
    lfail_tx: mpsc::Sender<LoadFailure>,
    epoch_ticker: Arc<EpochTicker>,
) -> usize {
    let mut ok_count = 0_usize;

    let mut stream = receiver_stream(artifact_rx)
        .map(|artifact| {
            observer.record_input();
            let cpu_budget = Arc::clone(&cpu_budget);
            let modules_budget = Arc::clone(&modules_budget);
            let observer_inner = Arc::clone(&observer);
            let path_for_failure = artifact.path.clone();
            // Each load worker is its own task so its progress is not
            // gated by `buffer_unordered`'s outer poll.
            tokio::spawn(async move {
                // `modules` permit caps simultaneously-live wasmtime
                // `Component`s globally; acquired here so the wait
                // shows as channel backpressure (small in-flight set
                // of wasm bytes), not as inflated load duration once
                // we actually start `Component::new`.
                let module_permit = modules_budget
                    .acquire_owned()
                    .await
                    .expect("modules semaphore closed");
                // CPU permit (shared with compile + execute) so
                // Cranelift JIT work doesn't oversubscribe cores.
                let _cpu_permit = cpu_budget
                    .acquire_owned()
                    .await
                    .expect("cpu semaphore closed");
                tokio::task::spawn_blocking(move || {
                    load_module(
                        artifact,
                        opt_level,
                        overall_start,
                        module_permit,
                        observer_inner,
                    )
                })
                .await
                .unwrap_or_else(|join_err| {
                    let elapsed = format_duration(overall_start.elapsed());
                    eprintln!(
                        "[{elapsed}] FAILED to load {path_for_failure}: \
                         blocking-task join error: {join_err}"
                    );
                    LoadOutcome::LoadFailure(LoadFailure {
                        path: path_for_failure,
                    })
                })
            })
        })
        .buffer_unordered(parallelism.max(1));

    while let Some(join_result) = stream.next().await {
        let outcome = join_result.expect("load task panicked");
        match outcome {
            LoadOutcome::Loaded(module) => {
                ok_count += 1;
                epoch_ticker.register(&module.engine);
                if loaded_tx.send(Arc::new(module)).await.is_ok() {
                    observer.record_output();
                }
            }
            LoadOutcome::LoadFailure(f) => {
                if lfail_tx.send(f).await.is_ok() {
                    observer.record_output();
                }
            }
        }
    }
    ok_count
}

/// **Stage 3 driver** — consume `LoadedModule`s as they stream in,
/// fan each module's test exports out into individual jobs, and run up
/// to `parallelism` jobs concurrently. Results stream through
/// `result_tx` so the collector can attach them to the per-file display
/// order without waiting for the full pipeline to drain.
///
/// Each job is a bare async future (not `tokio::spawn`'d) so that
/// dropping the outer stream — for example because the pipeline is
/// being torn down — actually cancels the in-flight test rather than
/// leaving it as a detached background task.
async fn run_execute_stage(
    loaded_rx: mpsc::Receiver<Arc<LoadedModule>>,
    parallelism: usize,
    cpu_budget: Arc<Semaphore>,
    preopened_dirs: Arc<Vec<(String, String)>>,
    observer: Arc<StageObserver>,
    result_tx: mpsc::Sender<TestResult>,
) {
    // Each loaded module fans out into 0+ test jobs. `flat_map` keeps
    // backpressure honest: as soon as one module's jobs are queued, the
    // next module can start loading without waiting for the first
    // module's tests to finish executing.
    let jobs_stream = receiver_stream(loaded_rx).flat_map(|module| {
        let jobs: Vec<TestJob> = module
            .tests
            .iter()
            .map(|t| TestJob {
                module: module.clone(),
                test_name: t.export_name.clone(),
                display_name: t.parsed.display.clone(),
                expect_trap: t.parsed.kind == TestKind::ExpectTrap,
                is_todo: t.parsed.kind == TestKind::Todo,
                timeout_ms: t.parsed.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            })
            .collect();
        stream::iter(jobs)
    });

    let mut stream = jobs_stream
        .map(|job| {
            observer.record_input();
            let preopened_dirs = preopened_dirs.clone();
            let cpu_budget = Arc::clone(&cpu_budget);
            // Spawn each test as an independent task. Dropping the
            // outer stream does NOT cancel a running guest wasm
            // function — but `EpochTicker`'s background epoch
            // increments will trap a runaway test at its
            // `#[timeout_ms]` deadline regardless of polling state,
            // so a long-running test still terminates cleanly when
            // the pipeline is torn down. The spawn also breaks the
            // outer-await-pause cycle that the compile and load
            // stages had with their CPU permits: a paused execute
            // loop on `result_tx.send` no longer freezes the
            // in-flight test futures (which hold `Arc<LoadedModule>`
            // and thus modules-budget permits).
            tokio::spawn(async move {
                // CPU permit (shared with compile + load) so that
                // CPU-bound tests don't compete with simultaneously
                // streaming compile / load work. Held for the full
                // test body — including async wasi-host waits. The
                // few I/O-dominated tests pay a small fairness cost
                // but the vast majority of fixtures are CPU-bound
                // and would otherwise trip the 5 s default timeout
                // under contention on 2 vCPU CI runners.
                let _cpu_permit = cpu_budget
                    .acquire_owned()
                    .await
                    .expect("cpu semaphore closed");
                run_single_test_safe(job, &preopened_dirs).await
            })
        })
        .buffer_unordered(parallelism.max(1));

    while let Some(join_result) = stream.next().await {
        let result = join_result.expect("test task panicked");
        observer.add_work(result.duration);
        if result_tx.send(result).await.is_ok() {
            observer.record_output();
        }
    }
}

/// Per-stage wall-clock timing reported in the three-axis summary so the
/// user can see where the pipeline's time actually went. With the 3-stage
/// streaming design, these overlap heavily — each duration is the wall
/// time from the stage's first input to its last output.
#[derive(Default, Clone, Copy)]
struct StageTimings {
    compile: Duration,
    load: Duration,
    execute: Duration,
}

impl StageTimings {
    fn merge(&mut self, other: StageTimings) {
        self.compile += other.compile;
        self.load += other.load;
        self.execute += other.execute;
    }
}

/// Aggregate output of [`run_pipeline`]: every per-file/per-test outcome
/// sorted into the bucket the caller needs for reporting, plus the
/// per-stage wall-clock breakdown.
///
/// `compiled_count` is the number of `CompileOutcome::Compiled` items
/// emitted by stage 1 — needed so `compile_ok` can be derived
/// additively in both run and `--no-run` modes from the same formula:
/// `compile_ok = compiled_count + todo_compile_errors.len()`.
///
/// `load_ok` is the count of modules that cleared the load stage (and
/// therefore reached the execute stage); it's tracked explicitly because
/// modules with zero `#[test]` exports leave no trace in `test_results`,
/// and under `--no-run` the load stage doesn't run so `load_ok = 0`.
struct PipelineOutcome {
    compiled_count: usize,
    load_ok: usize,
    test_results: Vec<TestResult>,
    todo_compile_errors: Vec<TodoCompileError>,
    compile_failures: Vec<CompileFailure>,
    load_failures: Vec<LoadFailure>,
    timings: StageTimings,
}

/// Background thread that increments the wasmtime epoch on every
/// registered engine. The registry holds `Weak<Engine>`, so once the
/// last `LoadedModule` for an engine drops the engine is freed and the
/// ticker stops ticking it (and lazily prunes the dead entry on the
/// next tick). This avoids the monotonic growth the previous
/// `Vec<Arc<Engine>>` design suffered when many modules loaded over the
/// course of one package run.
struct EpochTicker {
    engines: Arc<Mutex<Vec<Weak<Engine>>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl EpochTicker {
    fn start() -> Self {
        let engines: Arc<Mutex<Vec<Weak<Engine>>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let engines_clone = engines.clone();
        let handle = std::thread::spawn(move || {
            while !stop_clone.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(EPOCH_INTERVAL_MS));
                // Clone the Arcs we still need, then release the lock
                // before doing the actual epoch increments. Keeps the
                // critical section short — the lock is also taken by
                // `register`, which must not block the load stage on
                // per-engine work.
                let live: Vec<Arc<Engine>> = {
                    let mut guard = lock_resilient(&engines_clone);
                    guard.retain(|w| w.strong_count() > 0);
                    guard.iter().filter_map(Weak::upgrade).collect()
                };
                for engine in &live {
                    engine.increment_epoch();
                }
            }
        });
        Self {
            engines,
            stop,
            handle: Some(handle),
        }
    }

    fn register(&self, engine: &Arc<Engine>) {
        lock_resilient(&self.engines).push(Arc::downgrade(engine));
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Drive the three pipeline stages with bounded mpsc channels between
/// them. Each stage runs concurrently; the channels apply backpressure
/// so a slow stage keeps upstream from racing ahead. Sink channels
/// (todo / compile-failure / load-failure / test-result) are drained
/// by dedicated collector tasks in parallel.
///
/// Resource bounds are owned by `PipelineBudget`, not by the channel
/// buffers: a shared CPU semaphore caps combined compile + load
/// parallelism so the streaming overlap doesn't oversubscribe physical
/// cores, and a `modules` semaphore caps the number of fully-loaded
/// wasmtime `Component`s alive at any moment so peak memory stays
/// bounded regardless of stage-rate skew. Inter-stage channels are
/// therefore small (just enough to keep the pipeline non-bursty), with
/// the real limits enforced at the per-task-acquire points.
///
/// Under `--no-run`, the load and execute stages are replaced with
/// "drain to /dev/null" tasks so the compile stage producers don't
/// block on a full channel — all the work after stage 1 disappears.
#[allow(clippy::too_many_arguments)]
async fn run_pipeline(
    paths: &[String],
    flags: Arc<CompileFlags>,
    parallel_cap: usize,
    compile_jobs: usize,
    load_jobs: usize,
    execute_jobs: usize,
    preopened_dirs: Arc<Vec<(String, String)>>,
    overall_start: Instant,
    no_run: bool,
) -> PipelineOutcome {
    let opt_level = flags.opt_level.to_wasmtime();
    let budget = Arc::new(PipelineBudget::new(
        parallel_cap,
        compile_jobs,
        execute_jobs,
    ));

    // Inter-stage channel capacities: small. The `modules` semaphore
    // and per-stage `buffer_unordered` caps are what actually limit
    // concurrency and memory; the channels just need enough slack to
    // keep each stage's dequeue/enqueue from forming a strict
    // rendezvous.
    let artifact_cap = compile_jobs.max(1);
    let loaded_cap = load_jobs.max(1);
    let result_cap = execute_jobs.max(1) * 2;
    let sink_cap = compile_jobs.max(1);

    let (artifact_tx, artifact_rx) = mpsc::channel::<CompiledArtifact>(artifact_cap);
    let (loaded_tx, loaded_rx) = mpsc::channel::<Arc<LoadedModule>>(loaded_cap);
    let (todo_tx, todo_rx) = mpsc::channel::<TodoCompileError>(sink_cap);
    let (cfail_tx, cfail_rx) = mpsc::channel::<CompileFailure>(sink_cap);
    let (lfail_tx, lfail_rx) = mpsc::channel::<LoadFailure>(sink_cap);
    let (result_tx, result_rx) = mpsc::channel::<TestResult>(result_cap);

    let compile_observer = Arc::new(StageObserver::new());
    let load_observer = Arc::new(StageObserver::new());
    let execute_observer = Arc::new(StageObserver::new());

    // Epoch ticker lives only when execute will run; in `--no-run` mode
    // no engines are loaded and there's nothing to tick.
    let epoch_ticker = (!no_run).then(|| Arc::new(EpochTicker::start()));

    let paths_owned: Vec<String> = paths.to_vec();
    let compile_future = run_compile_stage(
        paths_owned,
        flags.clone(),
        compile_jobs,
        budget.cpu.clone(),
        overall_start,
        compile_observer.clone(),
        artifact_tx,
        todo_tx,
        cfail_tx,
    );

    let load_future: Pin<Box<dyn Future<Output = usize> + Send>> =
        if let Some(ticker) = epoch_ticker.as_ref() {
            Box::pin(run_load_stage(
                artifact_rx,
                opt_level,
                load_jobs,
                budget.cpu.clone(),
                budget.modules.clone(),
                overall_start,
                load_observer.clone(),
                loaded_tx,
                lfail_tx,
                ticker.clone(),
            ))
        } else {
            // Drain the artifact channel so compile-stage producers don't
            // stall on a closed sink. Drop downstream senders explicitly
            // to short-circuit later stages.
            drop(loaded_tx);
            drop(lfail_tx);
            Box::pin(async move {
                let mut rx = artifact_rx;
                while rx.recv().await.is_some() {}
                0
            })
        };

    let execute_future: Pin<Box<dyn Future<Output = ()> + Send>> = if epoch_ticker.is_some() {
        Box::pin(run_execute_stage(
            loaded_rx,
            execute_jobs,
            budget.cpu.clone(),
            preopened_dirs,
            execute_observer.clone(),
            result_tx,
        ))
    } else {
        drop(result_tx);
        Box::pin(async move {
            let mut rx = loaded_rx;
            while rx.recv().await.is_some() {}
        })
    };

    // All seven tasks (3 stages + 4 collectors) run concurrently as
    // independent runtime tasks. Per-fixture panics are already caught
    // by `compile_artifact` / `load_module` / `run_single_test_safe`,
    // so the stage drivers themselves return cleanly. Sequential
    // `.await` of the handles is fine for concurrency (the tasks
    // progress in parallel on the runtime; await just gathers their
    // results) and lets us avoid the `tokio::macros` feature.
    let compile_task = tokio::spawn(compile_future);
    let load_task = tokio::spawn(load_future);
    let execute_task = tokio::spawn(execute_future);
    let todo_task = tokio::spawn(collect_into_vec(todo_rx));
    let cfail_task = tokio::spawn(collect_into_vec(cfail_rx));
    let lfail_task = tokio::spawn(collect_into_vec(lfail_rx));
    let result_task = tokio::spawn(collect_into_vec(result_rx));

    let compiled_count = compile_task.await.expect("compile stage panicked");
    let load_ok = load_task.await.expect("load stage panicked");
    execute_task.await.expect("execute stage panicked");
    let todos = todo_task.await.expect("todo collector panicked");
    let cfails = cfail_task.await.expect("cfail collector panicked");
    let lfails = lfail_task.await.expect("lfail collector panicked");
    let results = result_task.await.expect("result collector panicked");

    drop(epoch_ticker); // Stops the tick thread.

    PipelineOutcome {
        compiled_count,
        load_ok,
        test_results: results,
        todo_compile_errors: todos,
        compile_failures: cfails,
        load_failures: lfails,
        timings: StageTimings {
            compile: compile_observer.work_time(),
            load: load_observer.work_time(),
            execute: execute_observer.work_time(),
        },
    }
}

async fn collect_into_vec<T>(mut rx: mpsc::Receiver<T>) -> Vec<T> {
    let mut out = Vec::new();
    while let Some(item) = rx.recv().await {
        out.push(item);
    }
    out
}

/// Build a `TestResult` for a setup-time failure (store/linker/instance/etc.).
fn fail_result(job: &TestJob, error: String, start: Instant) -> TestResult {
    TestResult {
        file_path: job.module.path.clone(),
        test_name: job.test_name.clone(),
        display_name: job.display_name.clone(),
        outcome: TestOutcome::Fail,
        error: Some(error),
        duration: start.elapsed(),
    }
}

/// `run_single_test` wrapped in `catch_unwind` so a panic anywhere
/// inside (host-side bug, wasmtime debug assertion, allocator OOM)
/// becomes a per-test `Fail` rather than aborting the whole pipeline.
async fn run_single_test_safe(job: TestJob, preopened_dirs: &[(String, String)]) -> TestResult {
    let start = Instant::now();
    let panic_or_result = AssertUnwindSafe(run_single_test(&job, preopened_dirs))
        .catch_unwind()
        .await;
    panic_or_result.unwrap_or_else(|payload| {
        let cause = format_panic_payload(&payload);
        fail_result(&job, format!("test worker panicked: {cause}"), start)
    })
}

/// Run a single test in its own Store
async fn run_single_test(job: &TestJob, preopened_dirs: &[(String, String)]) -> TestResult {
    let start = Instant::now();
    let module = job.module.as_ref();

    let mut store = match runtime::create_store(&module.engine, preopened_dirs, &[]) {
        Ok(s) => s,
        Err(e) => return fail_result(job, format!("failed to set up store: {e}"), start),
    };

    let deadline_ticks = (job.timeout_ms / EPOCH_INTERVAL_MS).max(1);
    store.set_epoch_deadline(deadline_ticks);

    let instance = match module
        .linker
        .instantiate_async(&mut store, &module.component)
        .await
    {
        Ok(inst) => inst,
        Err(e) => return fail_result(job, format!("failed to instantiate: {e}"), start),
    };

    let test_func = instance.get_typed_func::<(), (Result<(), ()>,)>(&mut store, &job.test_name);

    let (outcome, error) = match test_func {
        Ok(func) => match func.call_async(&mut store, ()).await {
            Ok((Ok(()),)) => {
                if job.is_todo {
                    // TODO test passed — the feature may now work. This is good news,
                    // not a failure. Report as "resolved" so the developer can remove #[TODO].
                    (
                        TestOutcome::TodoResolved,
                        Some("remove the #[TODO] attribute".to_string()),
                    )
                } else if job.expect_trap {
                    (
                        TestOutcome::Fail,
                        Some("expected trap but test returned Ok(())".to_string()),
                    )
                } else {
                    (TestOutcome::Pass, None)
                }
            }
            Ok((Err(()),)) => (TestOutcome::Fail, Some("test returned error".to_string())),
            Err(e) => {
                let is_timeout = is_epoch_deadline_error(&e);
                if is_timeout {
                    (
                        TestOutcome::Fail,
                        Some(format!(
                            "test timed out after {}ms (use #[timeout_ms(N)] to increase)",
                            job.timeout_ms
                        )),
                    )
                } else if job.is_todo {
                    (TestOutcome::TodoPending, None) // TODO test trapped as expected
                } else if job.expect_trap {
                    (TestOutcome::Pass, None) // expect_trap test trapped as expected
                } else {
                    (TestOutcome::Fail, Some(format!("{e}")))
                }
            }
        },
        Err(e) => (
            TestOutcome::Fail,
            Some(format!("failed to get test function: {e}")),
        ),
    };

    TestResult {
        file_path: module.path.clone(),
        test_name: job.test_name.clone(),
        display_name: job.display_name.clone(),
        outcome,
        error,
        duration: start.elapsed(),
    }
}

/// Check if an error is caused by an epoch deadline (timeout).
fn is_epoch_deadline_error(err: &wasmtime::Error) -> bool {
    err.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::Interrupt)
}

/// Format a duration in human-readable form with appropriate units.
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs >= 1.0 {
        format!("{secs:.2}s")
    } else {
        let ms = secs * 1000.0;
        format!("{ms:.0}ms")
    }
}

/// Per-package totals tracked while running one package's tests; merged
/// across all packages by [`run`] to produce the aggregate summary.
///
/// The three pipeline stages each contribute their own ok/failed pair so
/// the summary line can show, e.g., a load-stage regression independent
/// from a wado-side compile error.
#[derive(Default)]
struct PackageTotals {
    compile_ok: usize,
    compile_failed: usize,
    load_ok: usize,
    load_failed: usize,
    test_passed: u32,
    test_failed: u32,
    todo_pending: u32,
    todo_resolved: u32,
    timings: StageTimings,
}

impl PackageTotals {
    fn merge(&mut self, other: &PackageTotals) {
        self.compile_ok += other.compile_ok;
        self.compile_failed += other.compile_failed;
        self.load_ok += other.load_ok;
        self.load_failed += other.load_failed;
        self.test_passed += other.test_passed;
        self.test_failed += other.test_failed;
        self.todo_pending += other.todo_pending;
        self.todo_resolved += other.todo_resolved;
        self.timings.merge(other.timings);
    }
}

/// Print the per-stage tally lines (`compile:` / `load:` / `test:` /
/// optionally `todo:`). The `load:` line is suppressed when the stage
/// didn't run (e.g. `--no-run`) so the no-run output stays as concise
/// as before.
fn print_three_axis(totals: &PackageTotals, duration: Option<&str>) {
    let compile_dur = format_stage_duration(totals.timings.compile);
    let load_dur = format_stage_duration(totals.timings.load);
    let execute_dur = format_stage_duration(totals.timings.execute);
    println!(
        "compile: {} ok, {} failed{compile_dur}",
        totals.compile_ok, totals.compile_failed
    );
    let load_total = totals.load_ok + totals.load_failed;
    if load_total > 0 {
        println!(
            "load:    {} ok, {} failed{load_dur}",
            totals.load_ok, totals.load_failed
        );
    }
    println!(
        "test:    {} passed, {} failed{execute_dur}",
        totals.test_passed, totals.test_failed
    );
    let todo_total = totals.todo_pending + totals.todo_resolved;
    if todo_total > 0 {
        let mut todo_line = format!("todo:    {} pending", totals.todo_pending);
        if totals.todo_resolved > 0 {
            todo_line.push_str(&format!(", {} resolved", totals.todo_resolved));
        }
        println!("{todo_line}");
    }
    if let Some(d) = duration {
        println!("(wall: {d})");
    }
}

/// Render a stage's cumulative-work-time tag for the summary line —
/// `Σ worker_duration` for the stage, NOT a wall-clock span. Returns
/// an empty string when the stage did no work (so the line stays
/// clean for e.g. `--no-run`, where load and execute report zero).
fn format_stage_duration(d: Duration) -> String {
    if d.is_zero() {
        String::new()
    } else {
        format!("  [cpu: {}]", format_duration(d))
    }
}

/// One entry in the post-summary "TODO tests" section.
struct TodoEntry {
    file_path: String,
    display_name: String,
    resolved: bool,
}

/// One entry in the post-summary "failures:" section.
struct FailEntry {
    file_path: String,
    display_name: String,
    error: Option<String>,
}

/// Tally + per-entry detail produced by [`display_test_results`].
#[derive(Default)]
struct RunReport {
    test_passed: u32,
    test_failed: u32,
    todo_pending: u32,
    todo_resolved: u32,
    todo_entries: Vec<TodoEntry>,
    fail_entries: Vec<FailEntry>,
}

/// Print per-file/per-test results in source order while accumulating
/// totals and the entries needed for the post-run summary sections.
fn display_test_results(
    pkg_paths: &[String],
    results_by_file: &IndexMap<&str, Vec<&TestResult>>,
    todo_error_by_path: &IndexMap<&str, &TodoCompileError>,
) -> RunReport {
    let mut report = RunReport::default();
    for path in pkg_paths {
        if todo_error_by_path.contains_key(path.as_str()) {
            println!("  \x1b[33m·\x1b[0m #![TODO] module — compile error (expected)  ({path})");
            report.todo_pending += 1;
            report.todo_entries.push(TodoEntry {
                file_path: path.clone(),
                display_name: "#![TODO] module".to_string(),
                resolved: false,
            });
            continue;
        }
        let Some(file_results) = results_by_file.get(path.as_str()) else {
            continue;
        };
        println!("Running tests in {path}...");

        let mut sorted_results: Vec<_> = file_results.clone();
        sorted_results.sort_by(|a, b| a.test_name.cmp(&b.test_name));
        for result in sorted_results {
            let dur = format_duration(result.duration);
            match result.outcome {
                TestOutcome::Pass => {
                    println!("  ok   {} ({dur})", result.display_name);
                    report.test_passed += 1;
                }
                TestOutcome::Fail => {
                    println!("  \x1b[31mFAILED\x1b[0m {} ({dur})", result.display_name);
                    report.fail_entries.push(FailEntry {
                        file_path: result.file_path.clone(),
                        display_name: result.display_name.clone(),
                        error: result.error.clone(),
                    });
                    report.test_failed += 1;
                }
                TestOutcome::TodoPending => {
                    println!(
                        "  \x1b[33m·\x1b[0m {} \x1b[33m# TODO\x1b[0m ({dur})",
                        result.display_name
                    );
                    report.todo_pending += 1;
                    report.todo_entries.push(TodoEntry {
                        file_path: result.file_path.clone(),
                        display_name: result.display_name.clone(),
                        resolved: false,
                    });
                }
                TestOutcome::TodoResolved => {
                    println!(
                        "  \x1b[36m✓\x1b[0m {} \x1b[36m# TODO resolved\x1b[0m ({dur})",
                        result.display_name
                    );
                    if let Some(ref error) = result.error {
                        println!("    {error}");
                    }
                    report.todo_resolved += 1;
                    report.todo_entries.push(TodoEntry {
                        file_path: result.file_path.clone(),
                        display_name: result.display_name.clone(),
                        resolved: true,
                    });
                }
            }
        }
    }
    report
}

fn print_failure_section(fail_entries: &[FailEntry]) {
    if fail_entries.is_empty() {
        return;
    }
    println!();
    println!("failures:");
    println!();
    for entry in fail_entries {
        if let Some(ref error) = entry.error {
            println!("---- {} ({}) ----", entry.display_name, entry.file_path);
            println!("{error}");
            println!();
        }
    }
    println!("failures:");
    for entry in fail_entries {
        println!("    {} ({})", entry.display_name, entry.file_path);
    }
}

fn print_todo_section(todo_entries: &[TodoEntry], todo_resolved: u32) {
    if todo_entries.is_empty() {
        return;
    }
    println!();
    let todo_total = todo_entries.len();
    println!("TODO tests ({todo_total}):");
    for entry in todo_entries {
        if entry.resolved {
            println!(
                "  \x1b[36m✓ resolved\x1b[0m  {} — {}",
                entry.file_path, entry.display_name
            );
        } else {
            println!(
                "  \x1b[33m· pending\x1b[0m   {} — {}",
                entry.file_path, entry.display_name
            );
        }
    }
    if todo_resolved > 0 {
        println!();
        println!(
            "\x1b[36m{todo_resolved} TODO test(s) resolved — \
             remove the #[TODO] attribute\x1b[0m"
        );
    }
}

fn print_compile_failures_section(failures: &[CompileFailure]) {
    if failures.is_empty() {
        return;
    }
    println!();
    println!("compile failures:");
    for entry in failures {
        println!("    {}", entry.path);
    }
}

fn print_load_failures_section(failures: &[LoadFailure]) {
    if failures.is_empty() {
        return;
    }
    println!();
    println!("load failures:");
    for entry in failures {
        println!("    {}", entry.path);
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_one_package(
    pkg_run: &PackageRun,
    flags: Arc<CompileFlags>,
    parallel_cap: usize,
    compile_jobs: usize,
    load_jobs: usize,
    execute_jobs: usize,
    preopened_dirs: Arc<Vec<(String, String)>>,
    overall_start: Instant,
    show_banner: bool,
    no_run: bool,
) -> PackageTotals {
    if show_banner {
        println!();
        println!("=== package: {} ===", pkg_run.label);
    }

    let outcome = run_pipeline(
        &pkg_run.paths,
        flags,
        parallel_cap,
        compile_jobs,
        load_jobs,
        execute_jobs,
        preopened_dirs,
        overall_start,
        no_run,
    )
    .await;

    let PipelineOutcome {
        compiled_count,
        load_ok,
        test_results,
        todo_compile_errors,
        compile_failures,
        load_failures,
        timings,
    } = outcome;

    // Compile_ok is additive in both modes: every file that produced a
    // `CompileOutcome::Compiled` (= `compiled_count`) or a
    // `TodoCompileError` (= TODO module whose expected compile-error
    // fired) is compile-passed. Files that compiled but later failed
    // the load stage still count here as compile-passed; they appear
    // separately on the `load` axis. The same formula holds under
    // `--no-run` because `compiled_count` is tracked in stage 1
    // directly, independent of whether stages 2/3 run.
    let compile_ok = compiled_count + todo_compile_errors.len();
    let compile_failed = compile_failures.len();
    let load_failed = load_failures.len();

    // `--no-run`: stages 2 and 3 were short-circuited. The compile stage
    // wrote each fixture's `<primary>.kiln.json` as a side effect, which
    // is the whole point of the flag. We still surface compile failures
    // (so a stale-cache run that fails to compile isn't silently
    // swallowed) and `#![TODO]` modules whose expected compile-error
    // fired (so the TODO surface stays visible in summary parity with
    // the normal-run path — only test-level TODO resolution is
    // unobservable here, since we never executed any tests).
    if no_run {
        let todo_entries: Vec<TodoEntry> = todo_compile_errors
            .iter()
            .map(|e| TodoEntry {
                file_path: e.path.clone(),
                display_name: "#![TODO] module".to_string(),
                resolved: false,
            })
            .collect();
        let todo_pending = u32::try_from(todo_compile_errors.len()).unwrap_or(u32::MAX);

        print_compile_failures_section(&compile_failures);
        print_todo_section(&todo_entries, 0);
        let totals = PackageTotals {
            compile_ok,
            compile_failed,
            todo_pending,
            timings,
            ..PackageTotals::default()
        };
        println!();
        print_three_axis(&totals, None);
        return totals;
    }

    // Group results by file for display. Iteration order doesn't matter —
    // display walks `pkg_run.paths` and looks each one up here.
    let mut results_by_file: IndexMap<&str, Vec<&TestResult>> = IndexMap::default();
    for result in &test_results {
        results_by_file
            .entry(result.file_path.as_str())
            .or_default()
            .push(result);
    }
    let todo_error_by_path: IndexMap<&str, &TodoCompileError> = todo_compile_errors
        .iter()
        .map(|e| (e.path.as_str(), e))
        .collect();

    let report = display_test_results(&pkg_run.paths, &results_by_file, &todo_error_by_path);
    print_failure_section(&report.fail_entries);
    print_todo_section(&report.todo_entries, report.todo_resolved);
    print_compile_failures_section(&compile_failures);
    print_load_failures_section(&load_failures);

    let totals = PackageTotals {
        compile_ok,
        compile_failed,
        load_ok,
        load_failed,
        test_passed: report.test_passed,
        test_failed: report.test_failed,
        todo_pending: report.todo_pending,
        todo_resolved: report.todo_resolved,
        timings,
    };

    // Per-package summary (no duration; the aggregate or the run itself
    // owns the elapsed-time line).
    println!();
    print_three_axis(&totals, None);

    totals
}

/// Build the thread-local stdlib snapshot on `parallelism` distinct
/// blocking-pool worker threads in parallel, ahead of any compile work.
///
/// Each worker would otherwise build the snapshot lazily on its first
/// `annotate_loaded` call (~120 ms), serialising the cost behind that
/// task.  A `std::sync::Barrier` keeps every prewarm task running
/// simultaneously so tokio's blocking pool allocates `parallelism`
/// distinct threads; those same threads are then reused for the
/// `spawn_blocking` compile tasks scheduled by [`run_compile_stage`],
/// turning each first-compile from a cold miss into a cache hit.
async fn prewarm_stdlib_snapshot_on_workers(parallelism: usize) {
    let parallelism = parallelism.max(1);
    let barrier = Arc::new(std::sync::Barrier::new(parallelism));
    let handles: Vec<_> = (0..parallelism)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            tokio::task::spawn_blocking(move || {
                // Catch any panic from the snapshot build (the only
                // place this can fail is the `expect` in
                // `build_snapshot`, which would indicate a stdlib bug)
                // so that **every** task reaches the barrier.  If one
                // task panicked before the barrier the remaining
                // `parallelism - 1` tasks would block forever waiting
                // for a party count that can never be met,
                // deadlocking the test runner.  Re-raise after the
                // barrier so the original panic still propagates
                // through `handle.await`.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    wado_compiler::prewarm_stdlib_snapshot();
                }));
                // Block until every prewarm task is concurrently
                // running so the blocking pool cannot satisfy all
                // tasks with a single thread.
                barrier.wait();
                if let Err(panic) = result {
                    std::panic::resume_unwind(panic);
                }
            })
        })
        .collect();
    for handle in handles {
        let _ = handle.await;
    }
}

pub async fn run(opts: TestOptions) -> Result<(), CliExit> {
    let overall_start = Instant::now();
    let multi_pkg = opts.package_runs.len() > 1;
    let flags = Arc::new(opts.compile_flags());
    let jobs = opts.jobs;
    let no_run = opts.no_run;
    let package_runs = opts.package_runs;
    let preopened_dirs = Arc::new(opts.preopened_dirs);

    // `jobs` (= `--parallel N`, default `cpus`) is the **true** cap
    // on peak in-flight CPU work, enforced by the pipeline's shared
    // `cpu` semaphore (`PipelineBudget::cpu`): every per-fixture
    // worker — compile, load, execute — acquires one CPU permit
    // before doing its work. Per-stage `*_jobs` caps below only
    // shape queueing depth and how upstream backpressure is
    // distributed; they cannot exceed `jobs` in actual concurrency.
    //
    // Load gets `jobs / 2` per-stage cap because wasmtime's
    // Cranelift JIT is itself multi-threaded — doubling external
    // load workers on top of internal Cranelift threading thrashes
    // more than it helps. The shared cpu semaphore enforces the
    // cross-stage total either way.
    let compile_jobs = jobs.max(1);
    let load_jobs = (jobs / 2).max(1);
    let execute_jobs = jobs.max(1);

    // Prewarm the stdlib snapshot on `compile_jobs` distinct worker
    // threads before stage 1 starts. Each worker would otherwise build
    // the snapshot (~120 ms) on its first compile and steal that time
    // from the first batch of compiles; running the builds in parallel
    // up-front amortises the cost and leaves the tokio blocking-pool
    // threads in the steady state that the compile stage's
    // `spawn_blocking` tasks can re-use.
    prewarm_stdlib_snapshot_on_workers(compile_jobs).await;

    let mut grand = PackageTotals::default();
    for pkg_run in &package_runs {
        let totals = run_one_package(
            pkg_run,
            Arc::clone(&flags),
            jobs,
            compile_jobs,
            load_jobs,
            execute_jobs,
            preopened_dirs.clone(),
            overall_start,
            multi_pkg,
            no_run,
        )
        .await;
        grand.merge(&totals);
    }

    let total_dur = format_duration(overall_start.elapsed());
    if multi_pkg {
        println!();
        println!("=== aggregate ===");
        print_three_axis(&grand, Some(&total_dur));
    } else {
        println!("(wall: {total_dur})");
    }

    if grand.compile_failed > 0
        || grand.load_failed > 0
        || grand.test_failed > 0
        || grand.todo_resolved > 0
    {
        return Err(CliExit::silent_failure(1));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(name: &str) -> TestExportName {
        parse_test_export(name).unwrap_or_else(|| panic!("expected `test-` prefix in {name:?}"))
    }

    #[test]
    fn parses_kind() {
        assert_eq!(parse("test-0-simple").kind, TestKind::Normal);
        assert_eq!(parse("test-trap-0-panics").kind, TestKind::ExpectTrap);
        assert_eq!(parse("test-todo-1").kind, TestKind::Todo);
    }

    #[test]
    fn parses_timeout() {
        assert_eq!(parse("test-tm2000-0-slow").timeout_ms, Some(2000));
        assert_eq!(parse("test-trap-tm500-0-panics").timeout_ms, Some(500));
        assert_eq!(parse("test-todo-tm3000-1").timeout_ms, Some(3000));
        assert_eq!(parse("test-0-simple").timeout_ms, None);
        assert_eq!(parse("test-trap-0-panics").timeout_ms, None);
        assert_eq!(parse("test-todo-2").timeout_ms, None);
    }

    #[test]
    fn parses_display_name() {
        assert_eq!(parse("test-0-simple").display, "simple");
        assert_eq!(parse("test-trap-0-panics").display, "panics");
        assert_eq!(parse("test-todo-0-not-yet").display, "not_yet");
        assert_eq!(parse("test-tm2000-0-slow").display, "slow");
        assert_eq!(parse("test-trap-tm500-0-panics").display, "panics");
        assert_eq!(parse("test-1").display, "<test 1>");
        assert_eq!(parse("test-trap-3").display, "<test 3>");
        assert_eq!(parse("test-todo-tm3000-1").display, "<test 1>");
    }

    #[test]
    fn rejects_non_test_exports() {
        assert!(parse_test_export("foo").is_none());
        assert!(parse_test_export("testfoo").is_none());
    }

    #[test]
    fn malformed_timeout_segment_is_kept_as_name() {
        // `tm` not followed by digits-then-dash should fall through into the
        // index/name branch instead of being silently swallowed as a timeout.
        let parsed = parse("test-tmfoo-0-x");
        assert_eq!(parsed.timeout_ms, None);
        assert_eq!(parsed.display, "0_x");
    }

    #[test]
    fn trailing_dash_falls_back_to_index_form() {
        // Empty post-dash segment shouldn't render as an empty display
        // string — fall back to the bare-index form.
        assert_eq!(parse("test-0-").display, "<test 0>");
        assert_eq!(parse("test-trap-3-").display, "<test 3>");
        assert_eq!(parse("test-tm2000-0-").display, "<test 0>");
    }
}
