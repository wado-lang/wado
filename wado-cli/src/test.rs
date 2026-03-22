use std::fmt::Write as _;
use std::path::Path;
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use glob::glob;
use lexopt::Arg::Value;
use tokio::sync::mpsc;
use wasmtime::Engine;
use wasmtime::component::Component;

use crate::args::{self, CliExit};
use crate::compile;

const DEFAULT_TIMEOUT_MS: u64 = 1000;
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
}

#[derive(Clone, Copy)]
enum Opt {
    Filter,
    Parallel,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[Self::Filter, Self::Parallel, Self::Help];

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
        "If no files are specified, searches for **/*_test.wado recursively."
    )
    .unwrap();
    writeln!(
        buf,
        "If a directory is given, searches for *_test.wado files within it."
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

/// Find all *_test.wado files recursively in the given directory.
fn find_test_files_in(dir: &str) -> Result<Vec<String>, CliExit> {
    let pattern = format!("{dir}/**/*_test.wado");
    let mut files: Vec<String> = Vec::new();

    match glob(&pattern) {
        Ok(paths) => {
            for entry in paths.flatten() {
                files.push(entry.display().to_string());
            }
        }
        Err(e) => {
            return Err(CliExit::error(format!("failed to glob pattern: {e}")));
        }
    }

    files.sort();
    Ok(files)
}

/// Find all *_test.wado files recursively in the current directory.
fn find_test_files() -> Result<Vec<String>, CliExit> {
    find_test_files_in(".")
}

/// Resolve paths: expand directories to their contained *_test.wado files.
fn resolve_paths(paths: Vec<String>) -> Result<Vec<String>, CliExit> {
    let mut resolved = Vec::new();
    for path in paths {
        if Path::new(&path).is_dir() {
            let mut found = find_test_files_in(&path)?;
            if found.is_empty() {
                return Err(CliExit::error(format!(
                    "no *_test.wado files found in directory '{path}'"
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
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(val) = arg {
            paths.push(val.to_string_lossy().into_owned());
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    // Resolve paths: expand directories to *_test.wado files
    if paths.is_empty() {
        paths = find_test_files()?;
        if paths.is_empty() {
            return Err(CliExit {
                message: "No test files found (looking for **/*_test.wado)\n".to_owned(),
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
    })
}

/// A compiled test module ready for parallel execution
struct CompiledTestModule {
    path: String,
    engine: Arc<Engine>,
    component: Arc<Component>,
    compile_duration: Duration,
    load_duration: Duration,
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

/// Result from a test execution
struct TestResult {
    file_path: String,
    test_name: String,
    display_name: String,
    passed: bool,
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

/// Phase 1: Compile all test files and collect test jobs
async fn collect_test_jobs(
    paths: &[String],
    filter: Option<&str>,
) -> Result<(Vec<Arc<CompiledTestModule>>, Vec<TestJob>)> {
    let mut modules = Vec::new();
    let mut jobs = Vec::new();

    for (module_idx, path) in paths.iter().enumerate() {
        // Compile with --world test so test functions become component exports
        // and non-test code is subject to DCE.
        let compile_start = Instant::now();
        let wasm = compile::compile_with_full_opts(
            path,
            crate::compile::OptLevel::default(),
            wado_compiler::LogLevel::default(),
            Some("test".to_string()),
            false,
            None,
            None,
            None, // auto-selects debug allocator for test world
        )
        .await;
        let compile_duration = compile_start.elapsed();
        let load_start = Instant::now();
        let engine = Arc::new(runtime::create_test_engine(wasmtime::OptLevel::None)?);
        let component = Arc::new(Component::new(&engine, &wasm)?);
        let load_duration = load_start.elapsed();

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
            compile_duration,
            load_duration,
        }));
    }

    Ok((modules, jobs))
}

/// Run a single test in its own Store
async fn run_single_test(module: &CompiledTestModule, job: &TestJob) -> TestResult {
    let start = Instant::now();

    // Create fresh Store and Linker for this test
    let mut store = match runtime::create_store(&module.engine, &[], &[]) {
        Ok(s) => s,
        Err(e) => {
            return TestResult {
                file_path: module.path.clone(),
                test_name: job.test_name.clone(),
                display_name: job.display_name.clone(),
                passed: false,
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
                passed: false,
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
                passed: false,
                error: Some(format!("failed to instantiate: {e}")),
                duration: start.elapsed(),
            };
        }
    };

    // Get and call the test function
    let test_func = instance.get_typed_func::<(), (Result<(), ()>,)>(&mut store, &job.test_name);

    let (passed, error) = match test_func {
        Ok(func) => match func.call_async(&mut store, ()).await {
            Ok((Ok(()),)) => {
                if job.is_todo {
                    (
                        false,
                        Some(
                            "TODO test passed unexpectedly — \
                             the feature may be implemented; remove the #[TODO] attribute"
                                .to_string(),
                        ),
                    )
                } else if job.expect_trap {
                    (
                        false,
                        Some("expected trap but test returned Ok(())".to_string()),
                    )
                } else {
                    (true, None)
                }
            }
            Ok((Err(()),)) => (false, Some("test returned error".to_string())),
            Err(e) => {
                let is_timeout = is_epoch_deadline_error(&e);
                if is_timeout {
                    (
                        false,
                        Some(format!(
                            "test timed out after {}ms (use #[timeout_ms(N)] to increase)",
                            job.timeout_ms
                        )),
                    )
                } else if job.expect_trap || job.is_todo {
                    (true, None) // expected trap: pass
                } else {
                    (false, Some(format!("{e}")))
                }
            }
        },
        Err(e) => (false, Some(format!("failed to get test function: {e}"))),
    };

    TestResult {
        file_path: module.path.clone(),
        test_name: job.test_name.clone(),
        display_name: job.display_name.clone(),
        passed,
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
                    let result = run_single_test(module, &job).await;

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
    let (modules, jobs) = match collect_test_jobs(&opts.paths, opts.filter.as_deref()).await {
        Ok(result) => result,
        Err(e) => {
            eprintln!("Error collecting tests: {e}");
            process::exit(1);
        }
    };

    let total_tests = jobs.len();
    if total_tests == 0 {
        println!("No tests found");
        return;
    }

    // Phase 2: Execute tests in parallel
    let results = execute_tests_parallel(&modules, jobs, opts.jobs).await;

    // Group results by file for display
    let mut results_by_file: indexmap::IndexMap<String, Vec<&TestResult>> =
        indexmap::IndexMap::new();
    for result in &results {
        results_by_file
            .entry(result.file_path.clone())
            .or_default()
            .push(result);
    }

    // Display results in file order (matching input order)
    let mut total_passed = 0;
    let mut total_failed = 0;

    // Build a lookup from path to compiled module for compile duration
    let module_by_path: indexmap::IndexMap<&str, &CompiledTestModule> = modules
        .iter()
        .map(|m| (m.path.as_str(), m.as_ref()))
        .collect();

    for path in &opts.paths {
        if let Some(file_results) = results_by_file.get(path) {
            let timing = module_by_path
                .get(path.as_str())
                .map(|m| {
                    let compile = format_duration(m.compile_duration);
                    if m.load_duration.as_secs() >= 1 {
                        let load = format_duration(m.load_duration);
                        format!(" (compiled in {compile}, loaded in {load})")
                    } else {
                        format!(" (compiled in {compile})")
                    }
                })
                .unwrap_or_default();
            println!("Running tests in {path}...{timing}");

            // Sort by test name for consistent output
            let mut sorted_results: Vec<_> = file_results.clone();
            sorted_results.sort_by(|a, b| a.test_name.cmp(&b.test_name));

            for result in sorted_results {
                let dur = format_duration(result.duration);
                if result.passed {
                    println!("  \x1b[32m✓\x1b[0m {} ({dur})", result.display_name);
                    total_passed += 1;
                } else {
                    println!("  \x1b[31m✗\x1b[0m {} ({dur})", result.display_name);
                    if let Some(ref error) = result.error {
                        println!("    {error}");
                    }
                    total_failed += 1;
                }
            }
        }
    }

    let total_dur = format_duration(overall_start.elapsed());
    println!();
    println!("{total_passed} passed, {total_failed} failed ({total_dur})");

    if total_failed > 0 {
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
