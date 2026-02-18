use std::process;
use std::sync::Arc;

use anyhow::Result;
use glob::glob;
use lexopt::Arg::{Long, Short, Value};
use tokio::sync::mpsc;
use wasmtime::Engine;
use wasmtime::component::Component;

use crate::args::{next_arg, unexpected_arg};
use crate::compile;
use crate::runtime;

pub struct TestOptions {
    pub paths: Vec<String>,
    pub filter: Option<String>,
    pub jobs: usize,
}

pub fn print_usage() {
    eprintln!("Usage: wado test [options] [files...]");
    eprintln!();
    eprintln!("If no files are specified, searches for **/*_test.wado recursively.");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  -f, --filter <pattern>  Filter tests by name pattern");
    eprintln!("  -p, --parallel <N>      Number of parallel workers (default: num CPUs)");
    eprintln!("  --help                  Show this help message");
}

/// Find all *_test.wado files recursively in the current directory
fn find_test_files() -> Vec<String> {
    let pattern = "**/*_test.wado";
    let mut files: Vec<String> = Vec::new();

    match glob(pattern) {
        Ok(paths) => {
            for entry in paths.flatten() {
                files.push(entry.display().to_string());
            }
        }
        Err(e) => {
            eprintln!("Error: failed to glob pattern: {e}");
            process::exit(1);
        }
    }

    files.sort();
    files
}

pub fn parse_args(mut parser: lexopt::Parser) -> TestOptions {
    let mut paths: Vec<String> = Vec::new();
    let mut filter: Option<String> = None;
    let mut jobs: Option<usize> = None;
    while let Some(arg) = next_arg(&mut parser) {
        match arg {
            Long("help") => {
                print_usage();
                process::exit(0);
            }
            Short('f') | Long("filter") => {
                filter = Some(parser.value().unwrap().to_string_lossy().into_owned());
            }
            Short('p') | Long("parallel") => {
                let val = parser.value().unwrap().to_string_lossy().into_owned();
                match val.parse::<usize>() {
                    Ok(n) if n > 0 => jobs = Some(n),
                    _ => {
                        eprintln!("Error: --parallel requires a positive integer");
                        process::exit(1);
                    }
                }
            }
            Value(val) => {
                paths.push(val.to_string_lossy().into_owned());
            }
            _ => unexpected_arg(arg, print_usage),
        }
    }

    // If no files specified, search for *_test.wado files
    if paths.is_empty() {
        paths = find_test_files();
        if paths.is_empty() {
            eprintln!("No test files found (looking for **/*_test.wado)");
            process::exit(0);
        }
    }

    // Default to half of available CPUs (minimum 2)
    // This accounts for hyperthreading and leaves headroom for the system
    let jobs = jobs.unwrap_or_else(|| {
        let cpus = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        (cpus / 2).max(2)
    });

    TestOptions {
        paths,
        filter,
        jobs,
    }
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
}

/// Result from a test execution
struct TestResult {
    file_path: String,
    test_name: String,
    display_name: String,
    passed: bool,
    error: Option<String>,
}

/// Extract display name from test function name
/// test-0-simple -> "simple"
/// test-1 -> "<test 1>"
fn extract_display_name(test_name: &str) -> String {
    if let Some(name_part) = test_name.strip_prefix("test-") {
        if let Some(idx) = name_part.find('-') {
            name_part[idx + 1..].replace('-', "_")
        } else {
            format!("<test {name_part}>")
        }
    } else {
        test_name.to_string()
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
        let wasm = compile::compile_with_opts(
            path,
            crate::compile::OptLevel::default(),
            wado_compiler::LogLevel::default(),
        )
        .await;
        let engine = Arc::new(runtime::create_engine(wasmtime::OptLevel::None)?);
        let component = Arc::new(Component::new(&engine, &wasm)?);

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
            jobs.push(TestJob {
                module_idx,
                test_name: test_name.clone(),
                display_name: extract_display_name(test_name),
            });
        }

        modules.push(Arc::new(CompiledTestModule {
            path: path.clone(),
            engine,
            component,
        }));
    }

    Ok((modules, jobs))
}

/// Run a single test in its own Store
async fn run_single_test(module: &CompiledTestModule, job: &TestJob) -> TestResult {
    // Create fresh Store and Linker for this test
    let mut store = runtime::create_store(&module.engine);
    let linker = match runtime::create_linker(&module.engine) {
        Ok(l) => l,
        Err(e) => {
            return TestResult {
                file_path: module.path.clone(),
                test_name: job.test_name.clone(),
                display_name: job.display_name.clone(),
                passed: false,
                error: Some(format!("failed to set up linker: {e}")),
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
            };
        }
    };

    // Get and call the test function
    let test_func = instance.get_typed_func::<(), (Result<(), ()>,)>(&mut store, &job.test_name);

    let (passed, error) = match test_func {
        Ok(func) => match func.call_async(&mut store, ()).await {
            Ok((Ok(()),)) => (true, None),
            Ok((Err(()),)) => (false, Some("test returned error".to_string())),
            Err(e) => (false, Some(format!("{e}"))),
        },
        Err(e) => (false, Some(format!("failed to get test function: {e}"))),
    };

    TestResult {
        file_path: module.path.clone(),
        test_name: job.test_name.clone(),
        display_name: job.display_name.clone(),
        passed,
        error,
    }
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

    results
}

pub async fn run(opts: TestOptions) {
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

    for path in &opts.paths {
        if let Some(file_results) = results_by_file.get(path) {
            println!("Running tests in {path}...");

            // Sort by test name for consistent output
            let mut sorted_results: Vec<_> = file_results.clone();
            sorted_results.sort_by(|a, b| a.test_name.cmp(&b.test_name));

            for result in sorted_results {
                if result.passed {
                    println!("  \x1b[32m✓\x1b[0m {}", result.display_name);
                    total_passed += 1;
                } else {
                    println!("  \x1b[31m✗\x1b[0m {}", result.display_name);
                    if let Some(ref error) = result.error {
                        println!("    {error}");
                    }
                    total_failed += 1;
                }
            }
        }
    }

    println!();
    println!("{total_passed} passed, {total_failed} failed");

    if total_failed > 0 {
        process::exit(1);
    }
}
