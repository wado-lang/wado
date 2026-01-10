//! End-to-end tests for core:internals module
//!
//! Tests the stringify functions that are used for template string interpolation.
//! While these functions are not intended for direct user use, they can be used
//! and should work correctly.

use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use wado_compiler::compile;

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
// Boolean tests
// ============================================================================

#[tokio::test]
async fn test_stringify_bool_true() {
    let source = r#"
use {println} from "core:cli";
use {stringify_bool} from "core:internals";

pub fn run() -> Result<(), ()> {
    println(stringify_bool(true));
    return Ok(());
}
"#;

    let wasm = compile(source).expect("compilation failed");
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");
    assert_eq!(output, "true\n");
}

#[tokio::test]
async fn test_stringify_bool_false() {
    let source = r#"
use {println} from "core:cli";
use {stringify_bool} from "core:internals";

pub fn run() -> Result<(), ()> {
    println(stringify_bool(false));
    return Ok(());
}
"#;

    let wasm = compile(source).expect("compilation failed");
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");
    assert_eq!(output, "false\n");
}

// ============================================================================
// Character tests
// ============================================================================

#[tokio::test]
async fn test_stringify_char_ascii() {
    let source = r#"
use {println} from "core:cli";
use {stringify_char} from "core:internals";

pub fn run() -> Result<(), ()> {
    println(stringify_char('A'));
    return Ok(());
}
"#;

    let wasm = compile(source).expect("compilation failed");
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");
    assert_eq!(output, "A\n");
}

#[tokio::test]
async fn test_stringify_char_unicode_2byte() {
    let source = r#"
use {println} from "core:cli";
use {stringify_char} from "core:internals";

pub fn run() -> Result<(), ()> {
    println(stringify_char('é'));
    return Ok(());
}
"#;

    let wasm = compile(source).expect("compilation failed");
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");
    assert_eq!(output, "é\n");
}

#[tokio::test]
async fn test_stringify_char_unicode_3byte() {
    let source = r#"
use {println} from "core:cli";
use {stringify_char} from "core:internals";

pub fn run() -> Result<(), ()> {
    println(stringify_char('日'));
    return Ok(());
}
"#;

    let wasm = compile(source).expect("compilation failed");
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");
    assert_eq!(output, "日\n");
}

#[tokio::test]
async fn test_stringify_char_unicode_4byte() {
    let source = r#"
use {println} from "core:cli";
use {stringify_char} from "core:internals";

pub fn run() -> Result<(), ()> {
    println(stringify_char('😀'));
    return Ok(());
}
"#;

    let wasm = compile(source).expect("compilation failed");
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");
    assert_eq!(output, "😀\n");
}

// ============================================================================
// Integer tests - i32
// ============================================================================

#[tokio::test]
async fn test_stringify_i32_zero() {
    let source = r#"
use {println} from "core:cli";
use {stringify_i32} from "core:internals";

pub fn run() -> Result<(), ()> {
    println(stringify_i32(0));
    return Ok(());
}
"#;

    let wasm = compile(source).expect("compilation failed");
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");
    assert_eq!(output, "0\n");
}

#[tokio::test]
async fn test_stringify_i32_positive() {
    let source = r#"
use {println} from "core:cli";
use {stringify_i32} from "core:internals";

pub fn run() -> Result<(), ()> {
    println(stringify_i32(42));
    return Ok(());
}
"#;

    let wasm = compile(source).expect("compilation failed");
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");
    assert_eq!(output, "42\n");
}

#[tokio::test]
async fn test_stringify_i32_negative() {
    let source = r#"
use {println} from "core:cli";
use {stringify_i32} from "core:internals";

pub fn run() -> Result<(), ()> {
    println(stringify_i32(-123));
    return Ok(());
}
"#;

    let wasm = compile(source).expect("compilation failed");
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");
    assert_eq!(output, "-123\n");
}

#[tokio::test]
async fn test_stringify_i32_max() {
    let source = r#"
use {println} from "core:cli";
use {stringify_i32} from "core:internals";

pub fn run() -> Result<(), ()> {
    println(stringify_i32(2147483647));
    return Ok(());
}
"#;

    let wasm = compile(source).expect("compilation failed");
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");
    assert_eq!(output, "2147483647\n");
}

#[tokio::test]
async fn test_stringify_i32_min() {
    let source = r#"
use {println} from "core:cli";
use {stringify_i32} from "core:internals";

pub fn run() -> Result<(), ()> {
    println(stringify_i32(-2147483648));
    return Ok(());
}
"#;

    let wasm = compile(source).expect("compilation failed");
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");
    assert_eq!(output, "-2147483648\n");
}

// ============================================================================
// Integer tests - i64
// ============================================================================

#[tokio::test]
async fn test_stringify_i64_positive() {
    let source = r#"
use {println} from "core:cli";
use {stringify_i64} from "core:internals";

pub fn run() -> Result<(), ()> {
    println(stringify_i64(9223372036854775807));
    return Ok(());
}
"#;

    let wasm = compile(source).expect("compilation failed");
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");
    assert_eq!(output, "9223372036854775807\n");
}

#[tokio::test]
async fn test_stringify_i64_negative() {
    let source = r#"
use {println} from "core:cli";
use {stringify_i64} from "core:internals";

pub fn run() -> Result<(), ()> {
    println(stringify_i64(-9223372036854775808));
    return Ok(());
}
"#;

    let wasm = compile(source).expect("compilation failed");
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");
    assert_eq!(output, "-9223372036854775808\n");
}

// ============================================================================
// Integer tests - u32
// ============================================================================

#[tokio::test]
async fn test_stringify_u32_zero() {
    let source = r#"
use {println} from "core:cli";
use {stringify_u32} from "core:internals";

pub fn run() -> Result<(), ()> {
    println(stringify_u32(0));
    return Ok(());
}
"#;

    let wasm = compile(source).expect("compilation failed");
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");
    assert_eq!(output, "0\n");
}

#[tokio::test]
async fn test_stringify_u32_max() {
    let source = r#"
use {println} from "core:cli";
use {stringify_u32} from "core:internals";

pub fn run() -> Result<(), ()> {
    println(stringify_u32(4294967295));
    return Ok(());
}
"#;

    let wasm = compile(source).expect("compilation failed");
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");
    assert_eq!(output, "4294967295\n");
}

// ============================================================================
// Integer tests - u64
// ============================================================================

#[tokio::test]
async fn test_stringify_u64_max() {
    let source = r#"
use {println} from "core:cli";
use {stringify_u64} from "core:internals";

pub fn run() -> Result<(), ()> {
    println(stringify_u64(18446744073709551615));
    return Ok(());
}
"#;

    let wasm = compile(source).expect("compilation failed");
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");
    assert_eq!(output, "18446744073709551615\n");
}

// ============================================================================
// Delegating functions - i8, i16, u8, u16
// ============================================================================

#[tokio::test]
async fn test_stringify_i8() {
    let source = r#"
use {println} from "core:cli";
use {stringify_i8} from "core:internals";

pub fn run() -> Result<(), ()> {
    println(stringify_i8(127));
    return Ok(());
}
"#;

    let wasm = compile(source).expect("compilation failed");
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");
    assert_eq!(output, "127\n");
}

#[tokio::test]
async fn test_stringify_i16() {
    let source = r#"
use {println} from "core:cli";
use {stringify_i16} from "core:internals";

pub fn run() -> Result<(), ()> {
    println(stringify_i16(-32768));
    return Ok(());
}
"#;

    let wasm = compile(source).expect("compilation failed");
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");
    assert_eq!(output, "-32768\n");
}

#[tokio::test]
async fn test_stringify_u8() {
    let source = r#"
use {println} from "core:cli";
use {stringify_u8} from "core:internals";

pub fn run() -> Result<(), ()> {
    println(stringify_u8(255));
    return Ok(());
}
"#;

    let wasm = compile(source).expect("compilation failed");
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");
    assert_eq!(output, "255\n");
}

#[tokio::test]
async fn test_stringify_u16() {
    let source = r#"
use {println} from "core:cli";
use {stringify_u16} from "core:internals";

pub fn run() -> Result<(), ()> {
    println(stringify_u16(65535));
    return Ok(());
}
"#;

    let wasm = compile(source).expect("compilation failed");
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");
    assert_eq!(output, "65535\n");
}
