use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use futures::stream::{self, StreamExt};
use glob::Pattern;
use lexopt::Arg::Value;
use tokio::sync::mpsc;
use wado_compiler::hashmap::IndexMap;
use wasmtime::Engine;
use wasmtime::component::Component;

use crate::args::{self, CliExit};
use crate::compile::{self, CompileFlags, OptLevel};
use crate::discover;
use crate::manifest as project_manifest;
use crate::runtime;
use wado_compiler::LogLevel;

const DEFAULT_TIMEOUT_MS: u64 = 5000;
// Epoch tick interval for wasmtime's epoch-based interruption.
// A coarser interval (1s) reduces thread wake-ups. The trade-off is up to 1s
// of overshoot beyond the nominal timeout, which is acceptable since timeouts
// only fire on runaway tests, not on normal execution.
const EPOCH_INTERVAL_MS: u64 = 1000;

pub struct TestOptions {
    /// Each entry corresponds to one package context. The first entry is the
    /// root package (or the synthetic `"."` label when `wado test` is invoked
    /// outside a `wado.toml` project); subsequent entries come from
    /// recursively discovered sub-packages.
    pub package_runs: Vec<PackageRun>,
    pub jobs: usize,
    pub opt_level: OptLevel,
    pub log_level: LogLevel,
    pub inline_threshold: Option<usize>,
    pub opt_iterations: Option<u32>,
    /// `None` lets the compiler auto-select the debug allocator for the
    /// `test` world; explicit `--allocator` overrides that.
    pub allocator: Option<String>,
    /// Preopened directories as `(host_path, guest_path)` pairs.
    pub preopened_dirs: Vec<(String, String)>,
}

impl TestOptions {
    /// Build the `CompileFlags` shared with `compile`/`run`/`serve`. The
    /// `target_world` is pinned to `test` so test functions become exports
    /// and DCE prunes everything else.
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

/// One package context's discovered test files.
pub struct PackageRun {
    /// Display label, relative to the original invocation directory
    /// (`"."` for the root, `"subpkg"` for a nested sub-package).
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

/// Walk `root` and produce one `PackageRun` for the root and one for each
/// sub-package discovered transitively (cargo-workspace-style).
///
/// `apply_manifest_excludes` controls whether the root package's
/// `[test].exclude` is honoured. Sub-packages always apply their own
/// manifest. CLI `--exclude` patterns (`extra_excludes`) apply to *every*
/// package walked: each pattern is matched relative to the package being
/// walked, so a project-wide `--exclude 'tests/**'` drops `tests/` in the
/// root and in every nested package consistently.
///
/// The returned list is ordered: the root package first, then sub-packages in
/// source order.
fn discover_packages(
    root: &Path,
    apply_manifest_excludes: bool,
    extra_excludes: &[String],
) -> Result<Vec<PackageRun>, CliExit> {
    let invocation_root = root.to_path_buf();
    let mut runs: Vec<PackageRun> = Vec::new();
    let mut queue: Vec<PathBuf> = Vec::new();

    let root_manifest = if apply_manifest_excludes {
        package_manifest_excludes(&invocation_root)?
    } else {
        Vec::new()
    };
    let root_excludes = compile_excludes(&root_manifest, extra_excludes)?;
    walk_into(
        &invocation_root,
        &invocation_root,
        &root_excludes,
        &mut runs,
        &mut queue,
    )?;

    while let Some(pkg_root) = queue.pop() {
        let manifest = package_manifest_excludes(&pkg_root)?;
        let excludes = compile_excludes(&manifest, extra_excludes)?;
        walk_into(
            &pkg_root,
            &invocation_root,
            &excludes,
            &mut runs,
            &mut queue,
        )?;
    }

    Ok(runs)
}

/// Compile a package's exclude set: the manifest's `[test].exclude` plus
/// any CLI `--exclude` patterns. Both layers share the same shape so the
/// walker treats them uniformly.
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

/// Walk a single package root, append its `PackageRun`, and queue its
/// sub-packages (in source order — pushed in reverse so the LIFO `pop`
/// emits them root-first).
fn walk_into(
    pkg_root: &Path,
    invocation_root: &Path,
    excludes: &discover::ExcludeSet,
    runs: &mut Vec<PackageRun>,
    queue: &mut Vec<PathBuf>,
) -> Result<(), CliExit> {
    let result = discover::discover_test_files(pkg_root, excludes).map_err(CliExit::error)?;
    let label = relative_label(invocation_root, pkg_root);
    let paths = result.files.iter().map(|p| display_path(p)).collect();
    runs.push(PackageRun { label, paths });
    for sub in result.subpackages.into_iter().rev() {
        queue.push(sub);
    }
    Ok(())
}

/// Read `[test].exclude` from the `wado.toml` rooted at `pkg_root` (if any).
/// Returns the empty list when no manifest sits exactly at that directory —
/// sub-packages without their own `wado.toml` simply contribute no excludes.
fn package_manifest_excludes(pkg_root: &Path) -> Result<Vec<String>, CliExit> {
    match project_manifest::discover(pkg_root) {
        Ok(Some(project)) if project.root == pkg_root => Ok(project.manifest.test.exclude),
        Ok(_) => Ok(Vec::new()),
        Err(e) => Err(CliExit::error(e)),
    }
}

/// Format a sub-package path relative to the invocation root, falling back to
/// the absolute display when not a descendant.
fn relative_label(invocation_root: &Path, pkg_root: &Path) -> String {
    if pkg_root == invocation_root {
        return ".".to_string();
    }
    pkg_root.strip_prefix(invocation_root).map_or_else(
        |_| pkg_root.display().to_string(),
        |rel| rel.display().to_string(),
    )
}

/// Render a discovered path as the canonical string the runner stores and
/// matches against. The walker emits `./pkg/file.wado` (or `.\pkg\file.wado`
/// on Windows) when rooted at `.`; we normalise separators to `/` and strip
/// the leading `./` so that `--filter` and `[test].exclude` patterns — which
/// are documented as forward-slash globs — see a consistent shape regardless
/// of OS or how the user invoked `wado test`.
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

/// Resolve a mix of files and directory arguments into a single flat path
/// list. Directories are walked with the discovery rules; files pass through.
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
            // Run explicit file arguments through the same canonical form
            // as discovered paths so `--filter` and `--exclude` patterns
            // see one consistent shape (forward-slash, no leading `./`).
            // Honour `--exclude` even for explicit args, matching the
            // flag's documented "drop files whose path matches" behaviour.
            let canonical = display_path(p);
            if !excludes.matches_str(&canonical) {
                resolved.push(canonical);
            }
        }
    }
    Ok(resolved)
}

/// Parse command-line arguments for the `test` subcommand.
///
/// # Errors
///
/// Returns an error if the arguments are invalid or required arguments are missing.
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

    // Resolve paths into per-package runs. With no args, recurse from cwd
    // through every nested `wado.toml`. With explicit args, collapse
    // everything into a single synthetic root run; sub-package recursion
    // only kicks in for the no-args (project-wide) case.
    let mut package_runs: Vec<PackageRun> = if paths.is_empty() {
        discover_packages(Path::new("."), true, &cli_excludes)?
    } else {
        let resolved = resolve_paths(paths, &cli_excludes)?;
        vec![PackageRun {
            label: ".".to_string(),
            paths: resolved,
        }]
    };

    // --filter: shell-style wildcard match against each discovered path.
    // To match anywhere within a path, wrap the term in `*`s (e.g.
    // `*foo*`). Repeatable: a path is kept if any pattern matches.
    if !filters.is_empty() {
        let patterns: Vec<Pattern> = filters
            .iter()
            .map(|s| {
                Pattern::new(s)
                    .map_err(|e| CliExit::error(format!("invalid --filter pattern {s:?}: {e}")))
            })
            .collect::<Result<_, _>>()?;
        // `--filter` shares the walker's match semantics so users get
        // consistent results between `--filter`, `--exclude`, and the
        // manifest's `[test].exclude`. See `discover::WALK_MATCH_OPTIONS`.
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

    // Default to half of available CPUs (minimum 2)
    // This accounts for hyperthreading and leaves headroom for the system
    let jobs = jobs.unwrap_or_else(|| {
        let cpus = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        (cpus / 2).max(2)
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
    })
}

/// A compiled test module ready for parallel execution
struct CompiledTestModule {
    path: String,
    engine: Arc<Engine>,
    component: Arc<Component>,
}

/// A single test job to execute
struct TestJob {
    module_idx: usize,
    test_name: String,
    display_name: String,
    expect_trap: bool,
    is_todo: bool,
    timeout_ms: u64,
}

/// Outcome of a single test execution.
///
/// Regular tests are either Pass or Fail.
/// TODO tests live on a separate axis:
///   - `TodoPending`: trapped as expected (the feature is still unimplemented)
///   - `TodoResolved`: passed unexpectedly (the feature may now work — remove #[TODO])
#[derive(Clone, Copy, PartialEq, Eq)]
enum TestOutcome {
    Pass,
    Fail,
    TodoPending,
    TodoResolved,
}

/// Result from a test execution
struct TestResult {
    file_path: String,
    test_name: String,
    display_name: String,
    outcome: TestOutcome,
    error: Option<String>,
    duration: Duration,
}

/// Kind of test, parsed from the export name prefix.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TestKind {
    Normal,
    ExpectTrap,
    Todo,
}

/// Parsed test export name.
///
/// Export names follow the format `test-(trap-|todo-)?(tm{N}-)?{index}(-{name})?`
/// emitted by the compiler when targeting the `test` world.
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
        Some(idx) => rest[idx + 1..].replace('-', "_"),
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

/// Outcome of compiling one source file in Phase 1.
enum CompileTaskResult {
    Compiled {
        path: String,
        engine: Arc<Engine>,
        component: Arc<Component>,
        tests: Vec<ParsedTest>,
    },
    TodoCompileError(TodoCompileError),
    CompileFailure(CompileFailure),
}

/// Compile a single source file and prepare the wasmtime Component.
///
/// Per-file log lines are emitted here as compilation finishes; under
/// parallel Phase 1 they interleave across workers, but the leading
/// `[elapsed]` prefix records actual completion time. See
/// [`collect_test_jobs`] for how this is driven across threads.
async fn compile_one_file(
    path: String,
    flags: Arc<CompileFlags>,
    overall_start: Instant,
) -> Result<CompileTaskResult> {
    // `flags.target_world` is pinned to `test` by the caller so test
    // functions become component exports and non-test code is subject to
    // DCE; allocator is left at the caller's choice (None auto-selects
    // the debug allocator for the test world).
    let compile_start = Instant::now();
    let compile_result = compile::try_compile(&path, &flags).await;
    let compile_duration = compile_start.elapsed();
    let elapsed = format_duration(overall_start.elapsed());

    let compile_result = match compile_result {
        Ok(result) => result,
        Err(failure) if failure.is_todo_module => {
            let dur = format_duration(compile_duration);
            println!("[{elapsed}] Compiled {path} (TODO module, compile error expected, {dur})");
            return Ok(CompileTaskResult::TodoCompileError(TodoCompileError {
                path,
            }));
        }
        Err(_) => {
            let dur = format_duration(compile_duration);
            eprintln!("[{elapsed}] FAILED to compile {path} ({dur})");
            return Ok(CompileTaskResult::CompileFailure(CompileFailure { path }));
        }
    };

    let load_start = Instant::now();
    let engine = Arc::new(runtime::create_test_engine(flags.opt_level.to_wasmtime())?);
    let component = Arc::new(Component::new(&engine, &compile_result.wasm)?);
    let load_duration = load_start.elapsed();

    let dur = format_duration(compile_duration);
    let load_dur = format_duration(load_duration);
    println!("[{elapsed}] Compiled {path} ({dur}, loaded in {load_dur})");

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

    Ok(CompileTaskResult::Compiled {
        path,
        engine,
        component,
        tests,
    })
}

/// One discovered test export, paired with its raw export name.
struct ParsedTest {
    export_name: String,
    parsed: TestExportName,
}

/// Aggregate output of [`collect_test_jobs`]: every per-file outcome from
/// Phase 1 sorted into the bucket the runner needs for Phase 2 + reporting.
struct CollectedTests {
    modules: Vec<Arc<CompiledTestModule>>,
    jobs: Vec<TestJob>,
    todo_compile_errors: Vec<TodoCompileError>,
    compile_failures: Vec<CompileFailure>,
}

/// Phase 1: Compile all test files and collect test jobs.
///
/// Compilation fans out across `parallelism` worker tasks (`-p` / `--parallel`).
/// Per-file log lines stream as each file finishes — order is non-deterministic,
/// but the `[elapsed]` prefix reflects actual completion time.
async fn collect_test_jobs(
    paths: &[String],
    flags: Arc<CompileFlags>,
    parallelism: usize,
    overall_start: Instant,
) -> Result<CollectedTests> {
    // The compile future borrows `Cell`/`RefCell` internals (non-Send), so we
    // can't `tokio::spawn` it onto the multi-thread runtime. Instead, hop each
    // file onto the blocking pool and drive it on a per-thread current-thread
    // runtime — the non-Send state never crosses threads. `buffer_unordered`
    // bounds the in-flight count to `parallelism`.
    let task_results: Vec<_> = stream::iter(paths.iter().cloned())
        .map(|path| {
            let flags = Arc::clone(&flags);
            tokio::task::spawn_blocking(move || -> Result<CompileTaskResult> {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| anyhow!("failed to create compile runtime: {e}"))?;
                rt.block_on(compile_one_file(path, flags, overall_start))
            })
        })
        .buffer_unordered(parallelism.max(1))
        .collect()
        .await;

    let mut modules = Vec::new();
    let mut jobs = Vec::new();
    let mut todo_compile_errors = Vec::new();
    let mut compile_failures = Vec::new();

    for join_result in task_results {
        let task_result = join_result.map_err(|e| anyhow!("compile worker panicked: {e}"))??;
        match task_result {
            CompileTaskResult::Compiled {
                path,
                engine,
                component,
                tests,
            } => {
                let module_idx = modules.len();
                for ParsedTest {
                    export_name,
                    parsed,
                } in tests
                {
                    jobs.push(TestJob {
                        module_idx,
                        test_name: export_name,
                        display_name: parsed.display,
                        expect_trap: parsed.kind == TestKind::ExpectTrap,
                        is_todo: parsed.kind == TestKind::Todo,
                        timeout_ms: parsed.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
                    });
                }
                modules.push(Arc::new(CompiledTestModule {
                    path,
                    engine,
                    component,
                }));
            }
            CompileTaskResult::TodoCompileError(e) => todo_compile_errors.push(e),
            CompileTaskResult::CompileFailure(e) => compile_failures.push(e),
        }
    }

    Ok(CollectedTests {
        modules,
        jobs,
        todo_compile_errors,
        compile_failures,
    })
}

/// Build a `TestResult` for a setup-time failure (store/linker/instance/etc.).
fn fail_result(
    module: &CompiledTestModule,
    job: &TestJob,
    error: String,
    start: Instant,
) -> TestResult {
    TestResult {
        file_path: module.path.clone(),
        test_name: job.test_name.clone(),
        display_name: job.display_name.clone(),
        outcome: TestOutcome::Fail,
        error: Some(error),
        duration: start.elapsed(),
    }
}

/// Run a single test in its own Store
async fn run_single_test(
    module: &CompiledTestModule,
    job: &TestJob,
    preopened_dirs: &[(String, String)],
) -> TestResult {
    let start = Instant::now();

    let mut store = match runtime::create_store(&module.engine, preopened_dirs, &[]) {
        Ok(s) => s,
        Err(e) => return fail_result(module, job, format!("failed to set up store: {e}"), start),
    };

    let deadline_ticks = (job.timeout_ms / EPOCH_INTERVAL_MS).max(1);
    store.set_epoch_deadline(deadline_ticks);

    let linker = match runtime::create_linker(&module.engine) {
        Ok(l) => l,
        Err(e) => return fail_result(module, job, format!("failed to set up linker: {e}"), start),
    };

    let instance = match linker
        .instantiate_async(&mut store, &module.component)
        .await
    {
        Ok(inst) => inst,
        Err(e) => return fail_result(module, job, format!("failed to instantiate: {e}"), start),
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

/// Phase 2: Execute tests in parallel
async fn execute_tests_parallel(
    modules: &[Arc<CompiledTestModule>],
    jobs: Vec<TestJob>,
    num_workers: usize,
    preopened_dirs: Arc<Vec<(String, String)>>,
) -> Vec<TestResult> {
    if jobs.is_empty() {
        return Vec::new();
    }

    // Start epoch-incrementing threads for each engine (for timeout enforcement).
    // Each thread increments the engine's epoch every EPOCH_INTERVAL_MS.
    let epoch_stops: Vec<Arc<AtomicBool>> = modules
        .iter()
        .map(|m| {
            let stop = Arc::new(AtomicBool::new(false));
            let stop_clone = stop.clone();
            let engine = m.engine.clone();
            std::thread::spawn(move || {
                while !stop_clone.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(EPOCH_INTERVAL_MS));
                    engine.increment_epoch();
                }
            });
            stop
        })
        .collect();

    // Modest buffer keeps backpressure between workers and the collector
    // without sizing the channel to the full job count.
    let (tx, mut rx) = mpsc::channel((num_workers * 2).max(8));
    let jobs = Arc::new(std::sync::Mutex::new(jobs.into_iter()));

    let handles: Vec<_> = (0..num_workers)
        .map(|_| {
            let modules = modules.to_vec();
            let jobs = jobs.clone();
            let tx = tx.clone();
            let preopened_dirs = preopened_dirs.clone();

            tokio::spawn(async move {
                loop {
                    // Get next job from queue
                    let job = {
                        let mut guard = jobs.lock().unwrap();
                        guard.next()
                    };

                    let Some(job) = job else { break };

                    // Execute test
                    let module = &modules[job.module_idx];
                    let result = run_single_test(module, &job, &preopened_dirs).await;

                    // Send result (ignore error if receiver dropped)
                    let _ = tx.send(result).await;
                }
            })
        })
        .collect();

    drop(tx); // Close sender to signal completion

    // Collect results
    let mut results = Vec::new();
    while let Some(result) = rx.recv().await {
        results.push(result);
    }

    // Wait for all workers
    for handle in handles {
        let _ = handle.await;
    }

    // Stop epoch-incrementing threads
    for stop in &epoch_stops {
        stop.store(true, Ordering::Relaxed);
    }

    results
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
#[derive(Default)]
struct PackageTotals {
    compile_ok: usize,
    compile_failed: usize,
    test_passed: u32,
    test_failed: u32,
    todo_pending: u32,
    todo_resolved: u32,
}

impl PackageTotals {
    fn merge(&mut self, other: &PackageTotals) {
        self.compile_ok += other.compile_ok;
        self.compile_failed += other.compile_failed;
        self.test_passed += other.test_passed;
        self.test_failed += other.test_failed;
        self.todo_pending += other.todo_pending;
        self.todo_resolved += other.todo_resolved;
    }
}

fn print_three_axis(totals: &PackageTotals, duration: Option<&str>) {
    println!(
        "compile: {} ok, {} failed",
        totals.compile_ok, totals.compile_failed
    );
    println!(
        "test:    {} passed, {} failed",
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
        println!("({d})");
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

async fn run_one_package(
    pkg_run: &PackageRun,
    flags: Arc<CompileFlags>,
    parallelism: usize,
    preopened_dirs: Arc<Vec<(String, String)>>,
    overall_start: Instant,
    show_banner: bool,
) -> Result<PackageTotals> {
    if show_banner {
        println!();
        println!("=== package: {} ===", pkg_run.label);
    }

    // Phase 1: Compile all files and collect test jobs
    let CollectedTests {
        modules,
        jobs,
        todo_compile_errors,
        compile_failures,
    } = collect_test_jobs(&pkg_run.paths, flags, parallelism, overall_start).await?;

    // `compile_ok` counts every file whose compilation finished without a
    // non-TODO error. `#![TODO]` modules whose expected compile error fired
    // also count as compile-passed because the compile-level outcome was as
    // declared.
    let compile_ok = modules.len() + todo_compile_errors.len();
    let compile_failed = compile_failures.len();

    let displayable = jobs.len() + todo_compile_errors.len();
    if displayable == 0 && compile_failed == 0 {
        // Compile-only validation finished cleanly with no tests to run.
        // Still emit the three-axis summary so the compile total is visible.
        let totals = PackageTotals {
            compile_ok,
            ..PackageTotals::default()
        };
        println!();
        print_three_axis(&totals, None);
        return Ok(totals);
    }

    // Phase 2: Execute tests in parallel
    let results = execute_tests_parallel(&modules, jobs, parallelism, preopened_dirs).await;

    // Group results by file for display. Iteration order doesn't matter —
    // display walks `pkg_run.paths` and looks each one up here.
    let mut results_by_file: IndexMap<&str, Vec<&TestResult>> = IndexMap::default();
    for result in &results {
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

    let totals = PackageTotals {
        compile_ok,
        compile_failed,
        test_passed: report.test_passed,
        test_failed: report.test_failed,
        todo_pending: report.todo_pending,
        todo_resolved: report.todo_resolved,
    };

    // Per-package three-axis summary (no duration; the aggregate or the run
    // itself owns the elapsed-time line).
    println!();
    print_three_axis(&totals, None);

    Ok(totals)
}

pub async fn run(opts: TestOptions) {
    let overall_start = Instant::now();
    let multi_pkg = opts.package_runs.len() > 1;
    let flags = Arc::new(opts.compile_flags());
    let jobs = opts.jobs;
    let package_runs = opts.package_runs;
    let preopened_dirs = Arc::new(opts.preopened_dirs);

    let mut grand = PackageTotals::default();
    for pkg_run in &package_runs {
        match run_one_package(
            pkg_run,
            Arc::clone(&flags),
            jobs,
            preopened_dirs.clone(),
            overall_start,
            multi_pkg,
        )
        .await
        {
            Ok(totals) => grand.merge(&totals),
            Err(e) => {
                eprintln!("Error collecting tests: {e}");
                process::exit(1);
            }
        }
    }

    let total_dur = format_duration(overall_start.elapsed());
    if multi_pkg {
        println!();
        println!("=== aggregate ===");
        print_three_axis(&grand, Some(&total_dur));
    } else {
        println!("({total_dur})");
    }

    if grand.compile_failed > 0 || grand.test_failed > 0 || grand.todo_resolved > 0 {
        process::exit(1);
    }
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
}
