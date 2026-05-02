use std::fmt::Write as _;
use std::path::Path;
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use lexopt::Arg::Value;
use tokio::sync::mpsc;
use wasmtime::Engine;
use wasmtime::component::Component;

use crate::args::{self, CliExit};
use crate::compile::{self, OptLevel};
use crate::discover;
use crate::manifest as project_manifest;

const DEFAULT_TIMEOUT_MS: u64 = 5000;
// Epoch tick interval for wasmtime's epoch-based interruption.
// A coarser interval (1s) reduces thread wake-ups. The trade-off is up to 1s
// of overshoot beyond the nominal timeout, which is acceptable since timeouts
// only fire on runaway tests, not on normal execution.
const EPOCH_INTERVAL_MS: u64 = 1000;
use crate::runtime;

pub struct TestOptions {
    pub paths: Vec<String>,
    pub filter: Option<String>,
    pub jobs: usize,
    pub opt_level: OptLevel,
    /// Preopened directories as `(host_path, guest_path)` pairs.
    pub preopened_dirs: Vec<(String, String)>,
}

#[derive(Clone, Copy)]
enum Opt {
    Filter,
    Parallel,
    OptLevel,
    Dir,
    NoDir,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[
        Self::Filter,
        Self::Parallel,
        Self::OptLevel,
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
                desc: "Filter tests by name pattern",
            },
            Self::Parallel => args::OptSpec {
                long: Some("parallel"),
                short: Some('p'),
                value: Some("<N>"),
                desc: "Number of parallel workers (default: num CPUs)",
            },
            Self::OptLevel => args::OPT_LEVEL_SPEC,
            Self::Dir => args::OptSpec {
                long: Some("dir"),
                short: None,
                value: Some("<path>"),
                desc: "Preopen directory for WASI filesystem access\nUse --dir host::guest to specify different guest path",
            },
            Self::NoDir => args::OptSpec {
                long: Some("no-dir"),
                short: None,
                value: None,
                desc: "Do not preopen any directories (disables the default)",
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
        "If no files are specified, searches for *.wado files under the project root,"
    )
    .unwrap();
    writeln!(
        buf,
        "honouring .gitignore, .gitmodules, dot-prefixed entries, nested wado.toml"
    )
    .unwrap();
    writeln!(
        buf,
        "boundaries, and the [test].exclude list in wado.toml. Files without `test`"
    )
    .unwrap();
    writeln!(
        buf,
        "blocks are still compiled (compile-only check); only files with tests run."
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

/// Discover `*.wado` files using the project-aware walker (WEP 2026-05-02).
///
/// `root` is the directory to walk. When `apply_manifest_excludes` is true and
/// a `wado.toml` is found at or above `root`, its `[test].exclude` patterns
/// are applied.
fn discover_in(root: &Path, apply_manifest_excludes: bool) -> Result<Vec<String>, CliExit> {
    let excludes: Vec<String> = if apply_manifest_excludes {
        match project_manifest::discover(root) {
            Ok(Some(project)) => project.manifest.test.exclude.clone(),
            Ok(None) => Vec::new(),
            Err(e) => return Err(CliExit::error(e)),
        }
    } else {
        Vec::new()
    };
    let files = discover::discover_test_files(root, &excludes).map_err(CliExit::error)?;
    Ok(files
        .into_iter()
        .map(|p| p.display().to_string())
        .collect())
}

/// Resolve paths: expand directories to their contained `*.wado` files using
/// the discovery walker. File paths are passed through unchanged.
fn resolve_paths(paths: Vec<String>) -> Result<Vec<String>, CliExit> {
    let mut resolved = Vec::new();
    for path in paths {
        let p = Path::new(&path);
        if p.is_dir() {
            let mut found = discover_in(p, true)?;
            if found.is_empty() {
                return Err(CliExit::error(format!(
                    "no .wado files found in directory '{path}'"
                )));
            }
            resolved.append(&mut found);
        } else {
            resolved.push(path);
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
    let mut filter: Option<String> = None;
    let mut jobs: Option<usize> = None;
    let mut opt_level = OptLevel::default();
    let mut preopened_dirs: Vec<(String, String)> = Vec::new();
    let mut explicit_dirs = false;
    let mut no_dir = false;
    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Filter => {
                    filter = Some(args::require_string(&mut parser)?);
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
                Opt::OptLevel => {
                    let val = parser.optional_value();
                    let level_str = val
                        .as_ref()
                        .map(|v| v.to_string_lossy())
                        .unwrap_or_default();
                    opt_level = match level_str.as_ref() {
                        "" | "0" | "g" => OptLevel::O0,
                        "1" => OptLevel::O1,
                        "2" => OptLevel::O2,
                        "3" => OptLevel::O3,
                        "s" => OptLevel::Os,
                        _ => {
                            return Err(CliExit::error(format!(
                                "unknown optimization level '-O{level_str}'. Use -O0, -O1, -O2, -O3, -Os, or -Og"
                            )));
                        }
                    };
                }
                Opt::Dir => {
                    let dir_spec = args::require_string(&mut parser)?;
                    let (host, guest) = if let Some((h, g)) = dir_spec.split_once("::") {
                        (h.to_owned(), g.to_owned())
                    } else {
                        (dir_spec.clone(), dir_spec)
                    };
                    preopened_dirs.push((host, guest));
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

    // Resolve paths: walk the project root for *.wado files when no explicit
    // paths are given; otherwise expand any directory arguments via the
    // discovery walker. See WEP 2026-05-02.
    if paths.is_empty() {
        paths = discover_in(Path::new("."), true)?;
        if paths.is_empty() {
            return Err(CliExit {
                message: "No .wado files found under the project root\n".to_owned(),
                exit_code: 0,
            });
        }
    } else {
        paths = resolve_paths(paths)?;
    }

    // Default to half of available CPUs (minimum 2)
    // This accounts for hyperthreading and leaves headroom for the system
    let jobs = jobs.unwrap_or_else(|| {
        let cpus = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        (cpus / 2).max(2)
    });

    Ok(TestOptions {
        paths,
        filter,
        jobs,
        opt_level,
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

/// Parse per-test timeout from export name.
///
/// Export names with custom timeout contain a `tm{N}` segment:
/// - `test-tm2000-0-name` → `Some(2000)`
/// - `test-trap-tm500-0-name` → `Some(500)`
/// - `test-0-name` → `None` (use default)
fn parse_timeout_ms(test_name: &str) -> Option<u64> {
    let rest = test_name
        .strip_prefix("test-trap-")
        .or_else(|| test_name.strip_prefix("test-todo-"))
        .or_else(|| test_name.strip_prefix("test-"))?;
    let rest = rest.strip_prefix("tm")?;
    let end = rest.find('-')?;
    rest[..end].parse::<u64>().ok()
}

/// Strip the `tm{N}-` segment from the name part for display purposes.
fn strip_timeout_segment(name_part: &str) -> &str {
    if let Some(rest) = name_part.strip_prefix("tm")
        && let Some(idx) = rest.find('-')
    {
        return &rest[idx + 1..];
    }
    name_part
}

/// Extract display name from test export name.
///
/// - `test-0-simple` → `"simple"`
/// - `test-1` → `"<test 1>"`
/// - `test-trap-0-panics` → `"panics"` (`expect_trap` tests use the same display convention)
/// - `test-trap-3` → `"<test 3>"`
/// - `test-todo-0-not-yet` → `"not_yet"` (`TODO` tests use the same display convention)
/// - `test-todo-2` → `"<test 2>"`
fn extract_display_name(test_name: &str) -> String {
    // Strip "test-trap-", "test-todo-", or "test-" prefix to get the "index[-name]" part
    let name_part = test_name
        .strip_prefix("test-trap-")
        .or_else(|| test_name.strip_prefix("test-todo-"))
        .or_else(|| test_name.strip_prefix("test-"))
        .unwrap_or(test_name);
    // Strip optional timeout segment (e.g., "tm2000-")
    let name_part = strip_timeout_segment(name_part);
    if let Some(idx) = name_part.find('-') {
        name_part[idx + 1..].replace('-', "_")
    } else {
        format!("<test {name_part}>")
    }
}

/// A `#![TODO]` module that failed to compile.
/// Treated as a passing result since the module is expected to have errors.
struct TodoCompileError {
    path: String,
}

/// Phase 1: Compile all test files and collect test jobs.
///
/// Prints a log line for each file as compilation finishes (with elapsed time),
/// so the user can see progress during long compilation runs.
async fn collect_test_jobs(
    paths: &[String],
    filter: Option<&str>,
    opt_level: OptLevel,
    overall_start: Instant,
) -> Result<(
    Vec<Arc<CompiledTestModule>>,
    Vec<TestJob>,
    Vec<TodoCompileError>,
)> {
    let mut modules = Vec::new();
    let mut jobs = Vec::new();
    let mut todo_compile_errors = Vec::new();

    for path in paths {
        // Compile with --world test so test functions become component exports
        // and non-test code is subject to DCE.
        let compile_start = Instant::now();
        let compile_result = compile::try_compile_with_full_opts(
            path,
            opt_level,
            wado_compiler::LogLevel::default(),
            Some("test".to_string()),
            false,
            None,
            None,
            None, // auto-selects debug allocator for test world
        )
        .await;
        let compile_duration = compile_start.elapsed();
        let elapsed = format_duration(overall_start.elapsed());

        let compile_result = match compile_result {
            Ok(result) => result,
            Err(failure) if failure.is_todo_module => {
                // #![TODO] module failed to compile — expected, count as pass
                let dur = format_duration(compile_duration);
                println!(
                    "[{elapsed}] Compiled {path} (TODO module, compile error expected, {dur})"
                );
                todo_compile_errors.push(TodoCompileError { path: path.clone() });
                continue;
            }
            Err(_) => {
                // Non-TODO module failed to compile — fatal
                process::exit(1);
            }
        };

        let load_start = Instant::now();
        let engine = Arc::new(runtime::create_test_engine(wasmtime::OptLevel::None)?);
        let component = Arc::new(Component::new(&engine, &compile_result.wasm)?);
        let load_duration = load_start.elapsed();

        // Print per-file compilation log with elapsed time
        let dur = format_duration(compile_duration);
        let load_dur = format_duration(load_duration);
        println!("[{elapsed}] Compiled {path} ({dur}, loaded in {load_dur})");

        // Find test functions from exports
        let component_ty = component.component_type();
        let mut test_names: Vec<String> = Vec::new();

        for (name, _) in component_ty.exports(&engine) {
            if name.starts_with("test-") {
                // Apply filter if specified
                if let Some(pattern) = filter
                    && !name.contains(pattern)
                {
                    continue;
                }
                test_names.push(name.to_string());
            }
        }

        let module_idx = modules.len();
        for test_name in &test_names {
            let expect_trap = test_name.starts_with("test-trap-");
            let is_todo = test_name.starts_with("test-todo-");
            let timeout_ms = parse_timeout_ms(test_name).unwrap_or(DEFAULT_TIMEOUT_MS);
            jobs.push(TestJob {
                module_idx,
                test_name: test_name.clone(),
                display_name: extract_display_name(test_name),
                expect_trap,
                is_todo,
                timeout_ms,
            });
        }

        modules.push(Arc::new(CompiledTestModule {
            path: path.clone(),
            engine,
            component,
        }));
    }

    Ok((modules, jobs, todo_compile_errors))
}

/// Run a single test in its own Store
async fn run_single_test(
    module: &CompiledTestModule,
    job: &TestJob,
    preopened_dirs: &[(String, String)],
) -> TestResult {
    let start = Instant::now();

    // Create fresh Store and Linker for this test
    let mut store = match runtime::create_store(&module.engine, preopened_dirs, &[]) {
        Ok(s) => s,
        Err(e) => {
            return TestResult {
                file_path: module.path.clone(),
                test_name: job.test_name.clone(),
                display_name: job.display_name.clone(),
                outcome: TestOutcome::Fail,
                error: Some(format!("failed to set up store: {e}")),
                duration: start.elapsed(),
            };
        }
    };

    // Set epoch deadline for timeout enforcement
    let deadline_ticks = (job.timeout_ms / EPOCH_INTERVAL_MS).max(1);
    store.set_epoch_deadline(deadline_ticks);
    let linker = match runtime::create_linker(&module.engine) {
        Ok(l) => l,
        Err(e) => {
            return TestResult {
                file_path: module.path.clone(),
                test_name: job.test_name.clone(),
                display_name: job.display_name.clone(),
                outcome: TestOutcome::Fail,
                error: Some(format!("failed to set up linker: {e}")),
                duration: start.elapsed(),
            };
        }
    };

    // Instantiate and run
    let instance = match linker
        .instantiate_async(&mut store, &module.component)
        .await
    {
        Ok(inst) => inst,
        Err(e) => {
            return TestResult {
                file_path: module.path.clone(),
                test_name: job.test_name.clone(),
                display_name: job.display_name.clone(),
                outcome: TestOutcome::Fail,
                error: Some(format!("failed to instantiate: {e}")),
                duration: start.elapsed(),
            };
        }
    };

    // Get and call the test function
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

    let (tx, mut rx) = mpsc::channel(jobs.len());
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

pub async fn run(opts: TestOptions) {
    let overall_start = Instant::now();

    // Phase 1: Compile all files and collect test jobs
    let (modules, jobs, todo_compile_errors) = match collect_test_jobs(
        &opts.paths,
        opts.filter.as_deref(),
        opts.opt_level,
        overall_start,
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            eprintln!("Error collecting tests: {e}");
            process::exit(1);
        }
    };

    let total_tests = jobs.len() + todo_compile_errors.len();
    if total_tests == 0 {
        println!("No tests found");
        return;
    }

    // Phase 2: Execute tests in parallel
    let preopened_dirs = Arc::new(opts.preopened_dirs);
    let results = execute_tests_parallel(&modules, jobs, opts.jobs, preopened_dirs).await;

    // Group results by file for display
    let mut results_by_file: indexmap::IndexMap<String, Vec<&TestResult>> =
        indexmap::IndexMap::new();
    for result in &results {
        results_by_file
            .entry(result.file_path.clone())
            .or_default()
            .push(result);
    }

    // Build lookup maps for display
    let todo_error_by_path: indexmap::IndexMap<&str, &TodoCompileError> = todo_compile_errors
        .iter()
        .map(|e| (e.path.as_str(), e))
        .collect();

    // Display results in file order (matching input order)
    let mut total_passed = 0u32;
    let mut total_failed = 0u32;
    let mut total_todo = 0u32;
    let mut total_todo_resolved = 0u32;

    // Collect TODO entries for the summary section at the end
    struct TodoEntry {
        file_path: String,
        display_name: String,
        resolved: bool,
    }
    let mut todo_entries: Vec<TodoEntry> = Vec::new();

    // Collect failures for the summary section at the end (cargo test style)
    struct FailEntry {
        file_path: String,
        display_name: String,
        error: Option<String>,
    }
    let mut fail_entries: Vec<FailEntry> = Vec::new();

    for path in &opts.paths {
        // Handle #![TODO] modules that failed to compile
        if todo_error_by_path.contains_key(path.as_str()) {
            println!("  \x1b[33m·\x1b[0m #![TODO] module — compile error (expected)  ({path})");
            total_todo += 1;
            todo_entries.push(TodoEntry {
                file_path: path.clone(),
                display_name: "#![TODO] module".to_string(),
                resolved: false,
            });
            continue;
        }

        if let Some(file_results) = results_by_file.get(path) {
            println!("Running tests in {path}...");

            // Sort by test name for consistent output
            let mut sorted_results: Vec<_> = file_results.clone();
            sorted_results.sort_by(|a, b| a.test_name.cmp(&b.test_name));

            for result in sorted_results {
                let dur = format_duration(result.duration);
                match result.outcome {
                    TestOutcome::Pass => {
                        println!("  ok   {} ({dur})", result.display_name);
                        total_passed += 1;
                    }
                    TestOutcome::Fail => {
                        println!("  \x1b[31mFAILED\x1b[0m {} ({dur})", result.display_name);
                        fail_entries.push(FailEntry {
                            file_path: result.file_path.clone(),
                            display_name: result.display_name.clone(),
                            error: result.error.clone(),
                        });
                        total_failed += 1;
                    }
                    TestOutcome::TodoPending => {
                        println!(
                            "  \x1b[33m·\x1b[0m {} \x1b[33m# TODO\x1b[0m ({dur})",
                            result.display_name
                        );
                        total_todo += 1;
                        todo_entries.push(TodoEntry {
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
                        total_todo_resolved += 1;
                        todo_entries.push(TodoEntry {
                            file_path: result.file_path.clone(),
                            display_name: result.display_name.clone(),
                            resolved: true,
                        });
                    }
                }
            }
        }
    }

    // Failure summary (cargo test style)
    if !fail_entries.is_empty() {
        println!();
        println!("failures:");
        println!();
        for entry in &fail_entries {
            if let Some(ref error) = entry.error {
                println!("---- {} ({}) ----", entry.display_name, entry.file_path);
                println!("{error}");
                println!();
            }
        }
        println!("failures:");
        for entry in &fail_entries {
            println!("    {} ({})", entry.display_name, entry.file_path);
        }
    }

    // TODO summary section
    if !todo_entries.is_empty() {
        println!();
        let todo_total = total_todo + total_todo_resolved;
        println!("TODO tests ({todo_total}):");
        for entry in &todo_entries {
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
        if total_todo_resolved > 0 {
            println!();
            println!(
                "\x1b[36m{total_todo_resolved} TODO test(s) resolved — \
                 remove the #[TODO] attribute\x1b[0m"
            );
        }
    }

    // Summary line: "N passed, N failed; N todo (M resolved)"
    let total_dur = format_duration(overall_start.elapsed());
    println!();
    let mut summary = format!("{total_passed} passed, {total_failed} failed");
    let todo_total = total_todo + total_todo_resolved;
    if todo_total > 0 {
        summary.push_str(&format!("; {todo_total} todo"));
        if total_todo_resolved > 0 {
            summary.push_str(&format!(" ({total_todo_resolved} resolved)"));
        }
    }
    summary.push_str(&format!(" ({total_dur})"));
    println!("{summary}");

    if total_failed > 0 || total_todo_resolved > 0 {
        process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_timeout_ms() {
        assert_eq!(parse_timeout_ms("test-tm2000-0-slow"), Some(2000));
        assert_eq!(parse_timeout_ms("test-trap-tm500-0-panics"), Some(500));
        assert_eq!(parse_timeout_ms("test-todo-tm3000-1"), Some(3000));
        assert_eq!(parse_timeout_ms("test-0-simple"), None);
        assert_eq!(parse_timeout_ms("test-trap-0-panics"), None);
        assert_eq!(parse_timeout_ms("test-todo-2"), None);
    }

    #[test]
    fn test_extract_display_name_with_timeout() {
        assert_eq!(extract_display_name("test-tm2000-0-slow"), "slow");
        assert_eq!(extract_display_name("test-trap-tm500-0-panics"), "panics");
        assert_eq!(extract_display_name("test-todo-tm3000-1"), "<test 1>");
        // Existing behavior preserved
        assert_eq!(extract_display_name("test-0-simple"), "simple");
        assert_eq!(extract_display_name("test-trap-0-panics"), "panics");
        assert_eq!(extract_display_name("test-1"), "<test 1>");
    }
}
