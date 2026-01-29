//! End-to-end tests for Wado HTTP server (wasi:http/service world)
//!
//! These tests compile Wado programs targeting the Service world and run them
//! with wasmtime's WASI HTTP support, verifying HTTP responses.

mod common;

use bytes::Bytes;
use futures::try_join;
use http_body_util::{BodyExt, Empty};
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::Store;
use wasmtime_wasi::{WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::p3::bindings::Service;
use wasmtime_wasi_http::p3::{Request, WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

use wado_compiler::{CompileError, CompilerOptions, OptLevel};

// ============================================================================
// HTTP-specific WASI Context
// ============================================================================

struct TestHttpCtx;

impl WasiHttpCtx for TestHttpCtx {}

struct HttpTestState {
    table: ResourceTable,
    wasi: wasmtime_wasi::WasiCtx,
    http: TestHttpCtx,
}

impl WasiView for HttpTestState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for HttpTestState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
        }
    }
}

// ============================================================================
// HTTP Compilation
// ============================================================================

/// Compile a file targeting the Service world
async fn compile_http_service(path: &Path) -> Result<Vec<u8>, CompileError> {
    let source = std::fs::read_to_string(path).map_err(|e| CompileError::Io {
        path: path.to_string_lossy().to_string(),
        message: e.to_string(),
    })?;

    let base_path = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let host = common::FilesystemHost::new(base_path);

    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        target_world: Some("wasi:http/service".to_string()),
    };

    wado_compiler::compile_with_options(&source, &host, Some(&path.to_string_lossy()), options)
        .await
        .map(|r| r.wasm)
}

// ============================================================================
// HTTP Test Runner
// ============================================================================

/// Result of an HTTP request to the Wasm component
#[derive(Debug)]
struct HttpTestResult {
    status: u16,
    body: Vec<u8>,
}

/// Run an HTTP request against a compiled Wasm component
async fn run_http_request_async(wasm: Vec<u8>) -> anyhow::Result<HttpTestResult> {
    let engine = common::http_engine();
    let component = Component::new(&engine, &wasm)
        .map_err(|e| anyhow::anyhow!("failed to create component: {e:?}"))?;

    let mut linker: Linker<HttpTestState> = Linker::new(&engine);
    // Add BOTH P2 and P3 interfaces (like wasmtime tests do)
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;

    let state = HttpTestState {
        table: ResourceTable::new(),
        wasi: WasiCtxBuilder::new().build(),
        http: TestHttpCtx,
    };
    let mut store = Store::new(&engine, state);

    let service = Service::instantiate_async(&mut store, &component, &linker).await?;

    // Create a simple GET request
    let http_req = http::Request::builder()
        .uri("http://localhost/")
        .method(http::Method::GET)
        .body(Empty::<Bytes>::new())?;

    let (req, io) = Request::from_http(http_req);

    // Channel to receive the response
    let (tx, rx) = tokio::sync::oneshot::channel();

    // Add timeout wrapper
    let timeout_duration = Duration::from_secs(10);

    // Run handler and response receiver in parallel
    // Note: We don't await `io` because our handler doesn't consume request body
    let result = tokio::time::timeout(timeout_duration, async {
        let (handle_result, res) = try_join!(
            async {
                store
                    .run_concurrent(async |store| {
                        let (res, task) = match service.handle(store, req).await? {
                            Ok(pair) => pair,
                            Err(err) => return Ok(Err(Some(err))),
                        };
                        let _ = tx.send(store.with(|store| res.into_http(store, async { Ok(()) }))?);
                        task.block(store).await;
                        Ok(Ok(()))
                    })
                    .await?
            },
            async {
                let res = rx.await?;
                let (parts, body) = res.into_parts();
                let body = body.collect().await?;
                anyhow::Ok(http::Response::from_parts(parts, body))
            }
        )?;
        // Drop io - we don't consume request body in our simple tests
        drop(io);
        anyhow::Ok((handle_result, res))
    })
    .await
    .map_err(|_| anyhow::anyhow!("HTTP handler timed out after {timeout_duration:?}"))??;

    match result {
        (Ok(()), res) => Ok(HttpTestResult {
            status: res.status().as_u16(),
            body: res.into_body().to_bytes().to_vec(),
        }),
        (Err(Some(error_code)), _) => {
            // Handler returned error code - map to HTTP 500
            Ok(HttpTestResult {
                status: 500,
                body: format!("{error_code:?}").into_bytes(),
            })
        }
        (Err(None), _) => Err(anyhow::anyhow!("Handler returned error without error code")),
    }
}

// ============================================================================
// Test Spec
// ============================================================================

#[derive(Debug, Deserialize, Default)]
struct HttpTestSpec {
    /// Expected HTTP status code
    http_status: u16,

    /// Expected body content (optional)
    #[serde(default)]
    body: Option<String>,
}

// ============================================================================
// Tests
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn test_http_200_response() {
    let fixture_path = Path::new("tests/fixtures.http/http-200.wado");
    let fixture_name = "http-200.wado";

    // Read source and spec
    let source = std::fs::read_to_string(fixture_path)
        .unwrap_or_else(|e| panic!("[{fixture_name}] failed to read: {e}"));
    let data_section = common::extract_data_section(&source)
        .unwrap_or_else(|| panic!("[{fixture_name}] missing __DATA__ section"));
    let spec: HttpTestSpec = common::parse_data_section(data_section, fixture_name);

    // Compile
    let wasm = compile_http_service(fixture_path)
        .await
        .unwrap_or_else(|e| panic!("[{fixture_name}] compilation failed: {e}"));

    // Run HTTP request with timeout
    let result = run_http_request_async(wasm)
        .await
        .unwrap_or_else(|e| panic!("[{fixture_name}] HTTP request failed: {e:?}"));

    // Verify status
    assert_eq!(
        result.status, spec.http_status,
        "[{fixture_name}] HTTP status mismatch: expected {}, got {}",
        spec.http_status, result.status
    );

    // Verify body if specified
    if let Some(expected_body) = &spec.body {
        let actual_body = String::from_utf8_lossy(&result.body);
        assert_eq!(
            actual_body, expected_body.as_str(),
            "[{fixture_name}] body mismatch"
        );
    }
}
