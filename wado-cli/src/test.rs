use std::fmt::Write as _;
use std::path::Path;
use std::process;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use glob::glob;
use lexopt::Arg::Value;
use wasmtime::Engine;
use wasmtime::component::Component;

use crate::args::{self, CliExit};
use crate::compile;
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
                desc: "Number of parallel workers (default: num CPUs / 2)",
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

/// A compiled test module
struct CompiledTestModule {
    engine: Arc<Engine>,
    component: Arc<Component>,
    compile_duration: Duration,
}

/// A single test job to execute
struct TestJob {
    test_name: String,
    display_name: String,
    expect_trap: bool,
    is_todo: bool,
}

/// Result from a test execution
struct TestResult {
    display_name: String,
    passed: bool,
    error: Option<String>,
    duration: Duration,
}

/// Result from processing an entire test file
struct FileResult {
    path: String,
    compile_duration: Duration,
    test_results: Vec<TestResult>,
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
    if let Some(idx) = name_part.find('-') {
        name_part[idx + 1..].replace('-', "_")
    } else {
        format!("<test {name_part}>")
    }
}

/// Compile a single test file and return the compiled module.
async fn compile_one(path: &str) -> Result<CompiledTestModule> {
    let compile_start = Instant::now();
    let wasm = compile::compile_with_full_opts(
        path,
        crate::compile::OptLevel::default(),
        wado_compiler::LogLevel::default(),
        Some("test".to_string()),
        false,
    )
    .await?;
    let compile_duration = compile_start.elapsed();
    let engine = Arc::new(runtime::create_engine(
        wasmtime::OptLevel::None,
        &runtime::ProfileMode::None,
    )?);
    let component = Arc::new(Component::new(&engine, &wasm)?);
    Ok(CompiledTestModule {
        engine,
        component,
        compile_duration,
    })
}

/// Run a single test in its own Store
async fn run_single_test(module: &CompiledTestModule, job: &TestJob) -> TestResult {
    let start = Instant::now();

    // Create fresh Store and Linker for this test
    let mut store = match runtime::create_store(&module.engine, &[], &[]) {
        Ok(s) => s,
        Err(e) => {
            return TestResult {
                display_name: job.display_name.clone(),
                passed: false,
                error: Some(format!("failed to set up store: {e}")),
                duration: start.elapsed(),
            };
        }
    };
    let linker = match runtime::create_linker(&module.engine) {
        Ok(l) => l,
        Err(e) => {
            return TestResult {
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
                if job.expect_trap || job.is_todo {
                    (true, None) // expected trap: pass
                } else {
                    (false, Some(format!("{e}")))
                }
            }
        },
        Err(e) => (false, Some(format!("failed to get test function: {e}"))),
    };

    TestResult {
        display_name: job.display_name.clone(),
        passed,
        error,
        duration: start.elapsed(),
    }
}

/// Process a single test file: compile, discover tests, run them.
async fn process_one_file(path: &str, filter: Option<&str>) -> Result<FileResult> {
    let compiled = compile_one(path).await?;

    // Discover test exports
    let component_ty = compiled.component.component_type();
    let mut jobs = Vec::new();
    for (name, _) in component_ty.exports(&compiled.engine) {
        if name.starts_with("test-") {
            if let Some(pattern) = filter
                && !name.contains(pattern)
            {
                continue;
            }
            let expect_trap = name.starts_with("test-trap-");
            let is_todo = name.starts_with("test-todo-");
            jobs.push(TestJob {
                test_name: name.to_string(),
                display_name: extract_display_name(name),
                expect_trap,
                is_todo,
            });
        }
    }
    jobs.sort_by(|a, b| a.test_name.cmp(&b.test_name));

    // Run tests sequentially within this file
    let mut test_results = Vec::with_capacity(jobs.len());
    for job in &jobs {
        test_results.push(run_single_test(&compiled, job).await);
    }

    Ok(FileResult {
        path: path.to_owned(),
        compile_duration: compiled.compile_duration,
        test_results,
    })
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

pub fn run(opts: TestOptions) {
    let overall_start = Instant::now();

    // Spawn worker threads that compile and run tests per file.
    // Each worker gets its own single-threaded tokio runtime because the
    // compiler future is not Send (contains RefCell).
    // Each worker sends (file_index, result) so we can print in input order.
    let (tx, rx) = std::sync::mpsc::channel::<(usize, Result<FileResult>)>();
    let paths = Arc::new(opts.paths);
    let path_iter = Arc::new(std::sync::Mutex::new(0usize));
    let filter = opts.filter.clone();

    let handles: Vec<_> = (0..opts.jobs)
        .map(|_| {
            let paths = paths.clone();
            let path_iter = path_iter.clone();
            let tx = tx.clone();
            let filter = filter.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                loop {
                    let idx = {
                        let mut guard = path_iter.lock().unwrap();
                        let i = *guard;
                        if i >= paths.len() {
                            break;
                        }
                        *guard += 1;
                        i
                    };
                    let result = rt.block_on(process_one_file(&paths[idx], filter.as_deref()));
                    let _ = tx.send((idx, result));
                }
            })
        })
        .collect();

    drop(tx);

    // Stream results in input order: buffer out-of-order completions and
    // flush consecutive results as they become available.
    let mut total_passed = 0;
    let mut total_failed = 0;
    let mut total_tests = 0;
    let num_files = paths.len();

    let print_file_result = |fr: &FileResult,
                             total_tests: &mut usize,
                             total_passed: &mut usize,
                             total_failed: &mut usize| {
        if fr.test_results.is_empty() {
            return;
        }
        let compile_dur = format_duration(fr.compile_duration);
        println!(
            "Running tests in {}... (compiled in {compile_dur})",
            fr.path
        );
        for result in &fr.test_results {
            *total_tests += 1;
            let dur = format_duration(result.duration);
            if result.passed {
                println!("  \x1b[32m✓\x1b[0m {} ({dur})", result.display_name);
                *total_passed += 1;
            } else {
                println!("  \x1b[31m✗\x1b[0m {} ({dur})", result.display_name);
                if let Some(ref error) = result.error {
                    println!("    {error}");
                }
                *total_failed += 1;
            }
        }
    };

    tokio::task::block_in_place(|| {
        let mut next_to_print = 0usize;
        let mut buffer: Vec<Option<Result<FileResult>>> = (0..num_files).map(|_| None).collect();

        for (idx, file_result) in &rx {
            buffer[idx] = Some(file_result);

            // Flush all consecutive ready results
            while next_to_print < num_files && buffer[next_to_print].is_some() {
                match buffer[next_to_print].take().unwrap() {
                    Ok(fr) => {
                        print_file_result(
                            &fr,
                            &mut total_tests,
                            &mut total_passed,
                            &mut total_failed,
                        );
                    }
                    Err(e) => {
                        eprintln!("Error: {e}");
                        total_failed += 1;
                    }
                }
                next_to_print += 1;
            }
        }
    });

    for handle in handles {
        let _ = handle.join();
    }

    if total_tests == 0 {
        println!("No tests found");
        return;
    }

    let total_dur = format_duration(overall_start.elapsed());
    println!();
    println!("{total_passed} passed, {total_failed} failed ({total_dur})");

    if total_failed > 0 {
        process::exit(1);
    }
}
