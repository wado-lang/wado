//! End-to-end tests for Wado compiler
//!
//! These tests compile Wado programs and run them with wasmtime,
//! verifying the output matches expected values.

use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use std::path::PathBuf;
use wado_compiler::{compile, compile_file};

struct TestWasiState {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiView for TestWasiState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

/// Result of running a Wasm component with captured output
struct WasmRunResult {
    /// Captured stdout
    stdout: String,
    /// Captured stderr
    stderr: String,
    /// Whether the component trapped (e.g., from unreachable)
    trapped: bool,
}

async fn run_wasm(wasm: Vec<u8>) -> anyhow::Result<WasmRunResult> {
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

    // Create component from wasm bytes
    let component = Component::new(&engine, &wasm)?;

    // Set up linker with WASI P3
    let mut linker: Linker<TestWasiState> = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;

    // Create stdout and stderr capture pipes
    let stdout_pipe = MemoryOutputPipe::new(4096);
    let stdout_clone = stdout_pipe.clone();
    let stderr_pipe = MemoryOutputPipe::new(4096);
    let stderr_clone = stderr_pipe.clone();

    // Create WASI state with captured stdout and stderr
    let ctx = WasiCtxBuilder::new()
        .stdout(stdout_pipe)
        .stderr(stderr_pipe)
        .build();
    let table = ResourceTable::new();

    let state = TestWasiState { ctx, table };
    let mut store = Store::new(&engine, state);

    // Instantiate the component
    let instance = linker.instantiate_async(&mut store, &component).await?;

    // Get and call the "run" function
    let run_func = instance.get_typed_func::<(), (Result<(), ()>,)>(&mut store, "run")?;

    let trapped = match run_func.call_async(&mut store, ()).await {
        Ok((result,)) => result.is_err(),
        Err(_) => true, // Runtime error (trap)
    };

    // Get captured output
    let stdout_bytes = stdout_clone.contents();
    let stdout = String::from_utf8(stdout_bytes.to_vec())?;
    let stderr_bytes = stderr_clone.contents();
    let stderr = String::from_utf8(stderr_bytes.to_vec())?;

    Ok(WasmRunResult {
        stdout,
        stderr,
        trapped,
    })
}

// ============================================================================
// Basic hello world tests
// ============================================================================

#[tokio::test]
async fn test_hello_world() {
    let source = include_str!("fixtures/hello.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output
    assert_eq!(result.stdout, "Hello, world!\n");
}

#[tokio::test]
async fn test_multiple_println() {
    let source = include_str!("fixtures/multiple_println.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output
    assert_eq!(result.stdout, "Line 1\nLine 2\nLine 3\n");
}

// ============================================================================
// Effect function import tests
// ============================================================================

#[tokio::test]
async fn test_effect_import_demo() {
    let source = include_str!("fixtures/effect_import_demo.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify stdout
    assert_eq!(result.stdout, "Hello from imported Stdout!\n");
}

#[tokio::test]
async fn test_effect_import_with_aliasing() {
    let source = include_str!("fixtures/effect_import_aliasing.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output
    assert!(result.stdout.contains("Aliased import works!"));
    assert!(result.stdout.contains("Direct import also works!"));
}

#[tokio::test]
async fn test_multiple_effect_imports() {
    let source = include_str!("fixtures/multiple_effect_imports.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output
    assert_eq!(result.stdout, "Testing multiple imports\n");
}

// ============================================================================
// Local variable tests
// ============================================================================

#[tokio::test]
async fn test_local_let() {
    let source = include_str!("fixtures/local_let.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output
    assert_eq!(result.stdout, "Hello from let!\n");
}

#[tokio::test]
async fn test_local_let_mut() {
    let source = include_str!("fixtures/local_let_mut.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output
    assert_eq!(result.stdout, "First message\nSecond message\n");
}

// ============================================================================
// For loop tests
// ============================================================================

#[tokio::test]
async fn test_for_loop() {
    let source = include_str!("fixtures/for_loop.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output - loop runs 3 times, then "done"
    assert_eq!(result.stdout, "loop\nloop\nloop\ndone\n");
}

// ============================================================================
// Integer variable tests
// ============================================================================

#[tokio::test]
async fn test_local_integers() {
    let source = include_str!("fixtures/local_integers.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output - integer arithmetic works
    assert_eq!(result.stdout, "integers work\n");
}

#[tokio::test]
async fn test_local_integers_mut() {
    let source = include_str!("fixtures/local_integers_mut.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output - mutable integers with reassignment
    assert_eq!(result.stdout, "x is 5\nx is 10\nx is 15\n");
}

// ============================================================================
// Floating point variable tests
// ============================================================================

#[tokio::test]
async fn test_local_floats() {
    let source = include_str!("fixtures/local_floats.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output - float arithmetic works
    assert_eq!(result.stdout, "floats work\n");
}

#[tokio::test]
async fn test_local_floats_mut() {
    let source = include_str!("fixtures/local_floats_mut.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output - mutable floats with reassignment
    assert_eq!(result.stdout, "x is 1.0\nx is 2.5\nx is 3.0\n");
}

#[tokio::test]
async fn test_float_to_string() {
    let source = include_str!("fixtures/float_to_string.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output - float interpolation in template strings
    assert_eq!(
        result.stdout,
        "f is 1.23\npi is approximately 3.14159\n10.5 + 2.5 = 13.0\n"
    );
}

// ============================================================================
// User-defined function tests
// ============================================================================

#[tokio::test]
async fn test_user_function_call() {
    let source = include_str!("fixtures/user_function_call.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output - add(40, 2) should return 42
    assert_eq!(result.stdout, "success\n");
}

// ============================================================================
// Boolean variable tests
// ============================================================================

#[tokio::test]
async fn test_local_bools() {
    let source = include_str!("fixtures/local_bools.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output - boolean variables in conditions
    assert_eq!(result.stdout, "t is true\nf is false\n");
}

#[tokio::test]
async fn test_local_bools_mut() {
    let source = include_str!("fixtures/local_bools_mut.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output - mutable booleans with reassignment
    assert_eq!(result.stdout, "flag is false\nflag is true\n");
}

// ============================================================================
// Local module import tests
// ============================================================================

#[tokio::test]
async fn test_use_local_module() {
    // Get the path to the test fixture
    let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("use_local_module.wado");

    // Compile using compile_file which handles local imports
    let wasm = compile_file(&fixture_path).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output - add(40, 2) from imported module should return 42
    assert_eq!(result.stdout, "success\n");
}

// ============================================================================
// Bitwise operator tests
// ============================================================================

#[tokio::test]
async fn test_bitwise_and() {
    let source = include_str!("fixtures/bitwise_and.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run - test passes if no unreachable trap (assertion uses unreachable on failure)
    run_wasm(wasm).await.expect("runtime error");
}

#[tokio::test]
async fn test_bitwise_or() {
    let source = include_str!("fixtures/bitwise_or.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run - test passes if no unreachable trap
    run_wasm(wasm).await.expect("runtime error");
}

#[tokio::test]
async fn test_bitwise_xor() {
    let source = include_str!("fixtures/bitwise_xor.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run - test passes if no unreachable trap
    run_wasm(wasm).await.expect("runtime error");
}

#[tokio::test]
async fn test_bitwise_shift() {
    let source = include_str!("fixtures/bitwise_shift.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run - test passes if no unreachable trap
    run_wasm(wasm).await.expect("runtime error");
}

#[tokio::test]
async fn test_bitwise_not() {
    let source = include_str!("fixtures/bitwise_not.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run - test passes if no unreachable trap
    run_wasm(wasm).await.expect("runtime error");
}

#[tokio::test]
async fn test_bitwise_combined() {
    let source = include_str!("fixtures/bitwise_combined.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run - test passes if no unreachable trap
    run_wasm(wasm).await.expect("runtime error");
}

// ============================================================================
// Parentheses precedence tests
// ============================================================================

#[tokio::test]
async fn test_parentheses_precedence() {
    let source = include_str!("fixtures/parentheses_precedence.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run - test passes if no unreachable trap
    run_wasm(wasm).await.expect("runtime error");
}

// ============================================================================
// String template literal tests
// ============================================================================

#[tokio::test]
async fn test_template_string_empty() {
    let source = include_str!("fixtures/template_string_empty.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output - empty template produces empty string, println adds newline
    assert_eq!(result.stdout, "\n");
}

#[tokio::test]
async fn test_template_string_two_middle() {
    let source = include_str!("fixtures/template_string_two_middle.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output - template: `a {s1} b {s2} c`
    assert_eq!(result.stdout, "a X b Y c\n");
}

#[tokio::test]
async fn test_template_string_three_interp() {
    let source = include_str!("fixtures/template_string_three_interp.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output - template: `{s1} a {s2} c {s3}`
    assert_eq!(result.stdout, "X a Y c Z\n");
}

#[tokio::test]
async fn test_template_string_two_end() {
    let source = include_str!("fixtures/template_string_two_end.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output - template: `a {s1} b {s2}`
    assert_eq!(result.stdout, "a X b Y\n");
}

#[tokio::test]
async fn test_template_string_two_interp() {
    let source = include_str!("fixtures/template_string_two_interp.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output - template: `{s1} a {s2} c`
    assert_eq!(result.stdout, "X a Y c\n");
}

// ============================================================================
// Type cast tests
// ============================================================================

#[tokio::test]
async fn test_type_cast() {
    let source = include_str!("fixtures/type_cast.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output - all cast tests pass
    assert_eq!(
        result.stdout,
        "i32 to f64 works\nf64 to i32 works\nchained casts work\ncast in arithmetic works\n"
    );
}

// ============================================================================
// Template string scalar interpolation tests
// ============================================================================

#[tokio::test]
async fn test_template_string_bool() {
    let source = include_str!("fixtures/template_string_bool.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output - bool values are converted to "true" and "false"
    assert_eq!(result.stdout, "true: true\nfalse: false\n");
}

#[tokio::test]
async fn test_template_string_char() {
    let source = include_str!("fixtures/template_string_char.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output - char values are converted to their string representation
    assert_eq!(result.stdout, "char A: A\nchar Z: Z\n");
}

#[tokio::test]
async fn test_template_string_i32() {
    let source = include_str!("fixtures/template_string_i32.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output - i32 values are converted to their string representation
    assert_eq!(result.stdout, "positive: 42\nnegative: -17\nzero: 0\n");
}

#[tokio::test]
async fn test_template_string_i64() {
    let source = include_str!("fixtures/template_string_i64.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output - i64 values are converted to their string representation
    assert_eq!(
        result.stdout,
        "positive: 12345\nbig: 100000000000\nnegative: -9876\n"
    );
}

#[tokio::test]
async fn test_template_string_f64() {
    let source = include_str!("fixtures/template_string_f64.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime error");

    // Verify output - f64 values are converted to their string representation
    assert_eq!(result.stdout, "pi: 3.14159\nnegative: -2.5\n");
}

// ============================================================================
// Panic tests
// ============================================================================

#[tokio::test]
async fn test_eprintln_then_trap() {
    let source = include_str!("fixtures/eprintln_then_trap.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime setup error");

    // Verify the error message was printed to stderr
    assert_eq!(result.stderr, "error message\n");

    // Verify the program trapped (unreachable was executed)
    assert!(result.trapped, "should cause a trap");
}

#[tokio::test]
async fn test_panic_basic() {
    let source = include_str!("fixtures/panic_basic.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let result = run_wasm(wasm).await.expect("runtime setup error");

    // Verify the panic message was printed to stderr
    assert_eq!(result.stderr, "This is a panic message\n");

    // Verify the program trapped (unreachable was executed)
    assert!(result.trapped, "panic should cause a trap");
}
