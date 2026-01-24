use std::process;

use anyhow::Result;
use lexopt::Arg::{Long, Short, Value};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::args::{next_arg, unexpected_arg};
use crate::compile;

pub struct TestOptions {
    pub paths: Vec<String>,
    pub filter: Option<String>,
}

pub fn print_usage() {
    eprintln!("Usage: wado test [options] <files...>");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  -f, --filter <pattern>  Filter tests by name pattern");
    eprintln!("  --help                  Show this help message");
}

pub fn parse_args(mut parser: lexopt::Parser) -> TestOptions {
    let mut paths: Vec<String> = Vec::new();
    let mut filter: Option<String> = None;

    while let Some(arg) = next_arg(&mut parser) {
        match arg {
            Long("help") => {
                print_usage();
                process::exit(0);
            }
            Short('f') | Long("filter") => {
                filter = Some(parser.value().unwrap().to_string_lossy().into_owned());
            }
            Value(val) => {
                paths.push(val.to_string_lossy().into_owned());
            }
            _ => unexpected_arg(arg, print_usage),
        }
    }

    if paths.is_empty() {
        eprintln!("Error: no test files specified");
        print_usage();
        process::exit(1);
    }

    TestOptions { paths, filter }
}

struct WasiState {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiView for WasiState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

/// Test result for a single test
struct TestResult {
    name: String,
    passed: bool,
    error: Option<String>,
}

/// Run all tests in a compiled Wasm module
async fn run_tests_in_module(wasm: &[u8], filter: Option<&str>) -> Result<Vec<TestResult>> {
    let mut config = Config::new();
    config.async_support(true);
    config.wasm_component_model(true);
    config.wasm_component_model_gc(true);
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_builtins(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_simd(true);
    config.wasm_wide_arithmetic(true);
    config.wasm_threads(true);
    config.wasm_gc(true);
    config.wasm_function_references(true);

    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, wasm)?;

    // Find test functions from exports
    let component_ty = component.component_type();
    let mut test_exports: Vec<String> = Vec::new();

    for (name, _) in component_ty.exports(&engine) {
        // Component Model exports use kebab-case: test-0-simple
        if name.starts_with("test-") {
            // Apply filter if specified
            if let Some(pattern) = filter {
                if !name.contains(pattern) {
                    continue;
                }
            }
            test_exports.push(name.to_string());
        }
    }

    let mut results = Vec::new();

    for test_name in test_exports {
        // Create fresh WASI state for each test
        let ctx = WasiCtx::builder().inherit_stdio().build();
        let table = ResourceTable::new();

        let state = WasiState { ctx, table };
        let mut store = Store::new(&engine, state);

        // Set up linker with WASI P3
        let mut linker: Linker<WasiState> = Linker::new(&engine);
        wasmtime_wasi::p3::add_to_linker(&mut linker)?;

        // Instantiate the component
        let instance = linker.instantiate_async(&mut store, &component).await?;

        // Get and call the test function
        let test_func = instance.get_typed_func::<(), (Result<(), ()>,)>(&mut store, &test_name);

        let (passed, error) = match test_func {
            Ok(func) => match func.call_async(&mut store, ()).await {
                Ok((Ok(()),)) => (true, None),
                Ok((Err(()),)) => (false, Some("test returned error".to_string())),
                Err(e) => (false, Some(format!("{e}"))),
            },
            Err(e) => (false, Some(format!("failed to get test function: {e}"))),
        };

        // Extract display name from function name
        // test-0-simple -> "simple"
        // test-1 -> "<test 1>"
        let display_name = if let Some(name_part) = test_name.strip_prefix("test-") {
            if let Some(idx) = name_part.find('-') {
                name_part[idx + 1..].replace('-', "_")
            } else {
                format!("<test {name_part}>")
            }
        } else {
            test_name.clone()
        };

        results.push(TestResult {
            name: display_name,
            passed,
            error,
        });
    }

    Ok(results)
}

pub async fn run(opts: TestOptions) {
    let mut total_passed = 0;
    let mut total_failed = 0;
    let mut failed_tests: Vec<(String, String, String)> = Vec::new(); // (file, test_name, error)

    for path in &opts.paths {
        println!("Running tests in {path}...");

        let wasm = compile::compile(path).await;

        match run_tests_in_module(&wasm, opts.filter.as_deref()).await {
            Ok(results) => {
                for result in results {
                    if result.passed {
                        println!("  \x1b[32m✓\x1b[0m {}", result.name);
                        total_passed += 1;
                    } else {
                        println!("  \x1b[31m✗\x1b[0m {}", result.name);
                        if let Some(ref error) = result.error {
                            println!("    {error}");
                        }
                        total_failed += 1;
                        failed_tests.push((
                            path.clone(),
                            result.name,
                            result.error.unwrap_or_default(),
                        ));
                    }
                }
            }
            Err(e) => {
                eprintln!("Error running tests in {path}: {e}");
                process::exit(1);
            }
        }
    }

    println!();
    println!("{} passed, {} failed", total_passed, total_failed);

    if total_failed > 0 {
        process::exit(1);
    }
}
