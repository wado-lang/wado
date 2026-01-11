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

async fn run_wasm_capture_stdout(wasm: Vec<u8>) -> anyhow::Result<String> {
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
    // config.wasm_stack_switching(true); // "runtime error: the wasm_stack_switching feature is not supported on this compiler configuration" on macos
    config.wasm_gc(true);

    let engine = Engine::new(&config)?;

    // Create component from wasm bytes
    let component = Component::new(&engine, &wasm)?;

    // Set up linker with WASI P3
    let mut linker: Linker<TestWasiState> = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;

    // Create stdout capture pipe
    let stdout_pipe = MemoryOutputPipe::new(4096);
    let stdout_clone = stdout_pipe.clone();

    // Create WASI state with captured stdout
    let ctx = WasiCtxBuilder::new().stdout(stdout_pipe).build();
    let table = ResourceTable::new();

    let state = TestWasiState { ctx, table };
    let mut store = Store::new(&engine, state);

    // Instantiate the component
    let instance = linker.instantiate_async(&mut store, &component).await?;

    // Get and call the "run" function
    let run_func = instance.get_typed_func::<(), (Result<(), ()>,)>(&mut store, "run")?;

    let (result,) = run_func.call_async(&mut store, ()).await?;
    result.map_err(|()| anyhow::anyhow!("Component returned error"))?;

    // Get captured stdout using contents() method
    let output_bytes = stdout_clone.contents();
    let output = String::from_utf8(output_bytes.to_vec())?;

    Ok(output)
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
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output
    assert_eq!(output, "Hello, world!\n");
}

#[tokio::test]
async fn test_multiple_println() {
    let source = include_str!("fixtures/multiple_println.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output
    assert_eq!(output, "Line 1\nLine 2\nLine 3\n");
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
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify stdout
    assert_eq!(output, "Hello from imported Stdout!\n");
}

#[tokio::test]
async fn test_effect_import_with_aliasing() {
    let source = include_str!("fixtures/effect_import_aliasing.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output
    assert!(output.contains("Aliased import works!"));
    assert!(output.contains("Direct import also works!"));
}

#[tokio::test]
async fn test_multiple_effect_imports() {
    let source = include_str!("fixtures/multiple_effect_imports.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output
    assert_eq!(output, "Testing multiple imports\n");
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
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output
    assert_eq!(output, "Hello from let!\n");
}

#[tokio::test]
async fn test_local_let_mut() {
    let source = include_str!("fixtures/local_let_mut.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output
    assert_eq!(output, "First message\nSecond message\n");
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
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output - loop runs 3 times, then "done"
    assert_eq!(output, "loop\nloop\nloop\ndone\n");
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
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output - integer arithmetic works
    assert_eq!(output, "integers work\n");
}

#[tokio::test]
async fn test_local_integers_mut() {
    let source = include_str!("fixtures/local_integers_mut.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output - mutable integers with reassignment
    assert_eq!(output, "x is 5\nx is 10\nx is 15\n");
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
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output - float arithmetic works
    assert_eq!(output, "floats work\n");
}

#[tokio::test]
async fn test_local_floats_mut() {
    let source = include_str!("fixtures/local_floats_mut.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output - mutable floats with reassignment
    assert_eq!(output, "x is 1.0\nx is 2.5\nx is 3.0\n");
}

#[tokio::test]
async fn test_float_to_string() {
    let source = include_str!("fixtures/float_to_string.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output - float interpolation in template strings
    assert_eq!(
        output,
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
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output - add(40, 2) should return 42
    assert_eq!(output, "success\n");
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
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output - boolean variables in conditions
    assert_eq!(output, "t is true\nf is false\n");
}

#[tokio::test]
async fn test_local_bools_mut() {
    let source = include_str!("fixtures/local_bools_mut.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output - mutable booleans with reassignment
    assert_eq!(output, "flag is false\nflag is true\n");
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
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output - add(40, 2) from imported module should return 42
    assert_eq!(output, "success\n");
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
    run_wasm_capture_stdout(wasm).await.expect("runtime error");
}

#[tokio::test]
async fn test_bitwise_or() {
    let source = include_str!("fixtures/bitwise_or.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run - test passes if no unreachable trap
    run_wasm_capture_stdout(wasm).await.expect("runtime error");
}

#[tokio::test]
async fn test_bitwise_xor() {
    let source = include_str!("fixtures/bitwise_xor.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run - test passes if no unreachable trap
    run_wasm_capture_stdout(wasm).await.expect("runtime error");
}

#[tokio::test]
async fn test_bitwise_shift() {
    let source = include_str!("fixtures/bitwise_shift.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run - test passes if no unreachable trap
    run_wasm_capture_stdout(wasm).await.expect("runtime error");
}

#[tokio::test]
async fn test_bitwise_not() {
    let source = include_str!("fixtures/bitwise_not.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run - test passes if no unreachable trap
    run_wasm_capture_stdout(wasm).await.expect("runtime error");
}

#[tokio::test]
async fn test_bitwise_combined() {
    let source = include_str!("fixtures/bitwise_combined.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run - test passes if no unreachable trap
    run_wasm_capture_stdout(wasm).await.expect("runtime error");
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
    run_wasm_capture_stdout(wasm).await.expect("runtime error");
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
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output - empty template produces empty string, println adds newline
    assert_eq!(output, "\n");
}

#[tokio::test]
async fn test_template_string_two_middle() {
    let source = include_str!("fixtures/template_string_two_middle.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output - template: `a {s1} b {s2} c`
    assert_eq!(output, "a X b Y c\n");
}

#[tokio::test]
async fn test_template_string_three_interp() {
    let source = include_str!("fixtures/template_string_three_interp.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output - template: `{s1} a {s2} c {s3}`
    assert_eq!(output, "X a Y c Z\n");
}

#[tokio::test]
async fn test_template_string_two_end() {
    let source = include_str!("fixtures/template_string_two_end.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output - template: `a {s1} b {s2}`
    assert_eq!(output, "a X b Y\n");
}

#[tokio::test]
async fn test_template_string_two_interp() {
    let source = include_str!("fixtures/template_string_two_interp.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output - template: `{s1} a {s2} c`
    assert_eq!(output, "X a Y c\n");
}
