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
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_builtins(true);
    config.wasm_component_model_async_stackful(true);
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
// Operator precedence tests
// ============================================================================

#[tokio::test]
async fn test_operator_precedence_bitwise() {
    let source = include_str!("fixtures/operator_precedence_bitwise.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output - all bitwise precedence tests should pass
    assert!(output.contains("bitwise and precedence works"));
    assert!(output.contains("bitwise or precedence works"));
    assert!(output.contains("bitwise xor precedence works"));
    assert!(output.contains("left shift precedence works"));
    assert!(output.contains("right shift precedence works"));
    assert!(output.contains("bitwise not works"));
    assert!(output.contains("combined bitwise operations work"));
}

#[tokio::test]
async fn test_operator_precedence_comparison_chaining() {
    let source = include_str!("fixtures/operator_precedence_comparison_chaining.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output - all comparison chaining tests should pass
    assert!(output.contains("ascending chain works"));
    assert!(output.contains("descending chain works"));
    assert!(output.contains("range check works"));
    assert!(output.contains("equality chain works"));
    assert!(output.contains("boundary chain works"));
    assert!(output.contains("false chain works correctly"));
    // Should not contain error message
    assert!(!output.contains("ERROR"));
}

#[tokio::test]
async fn test_operator_precedence_comprehensive() {
    let source = include_str!("fixtures/operator_precedence_comprehensive.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output - all comprehensive precedence tests should pass
    assert!(output.contains("additive > shift works"));
    assert!(output.contains("bitwise and > bitwise or works"));
    assert!(output.contains("bitwise xor precedence works"));
    assert!(output.contains("multiplicative > additive works"));
    assert!(output.contains("unary > multiplicative works"));
    assert!(output.contains("bitwise not unary works"));
    assert!(output.contains("comparison > logical and works"));
    assert!(output.contains("logical and > logical or works"));
    assert!(output.contains("assignment has lowest precedence"));
    assert!(output.contains("complex precedence works"));
}

// ============================================================================
// Float formatting tests (ryu integration)
// ============================================================================

// TODO: Re-enable once template string concatenation is fully implemented
// Currently, template strings with multiple parts (e.g., `Pi: {3.14}`) don't concatenate correctly
// The float formatting with ryu works, but the string concatenation logic needs completion
#[tokio::test]
#[ignore = "Template string concatenation not yet implemented"]
async fn test_float_template_formatting() {
    let source = include_str!("fixtures/float_template.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify ryu-formatted float output
    // ryu produces deterministic, minimal representations
    assert_eq!(output, "Pi: 3.14159\n", "Expected 'Pi: 3.14159\\n', got: {output}");
}
