//! Cross-validation tests comparing Wado's core:digest SHA-256 against the
//! `sha2` crate.
//!
//! A single Wado driver program (`digest_driver.wado`) is compiled once and
//! reused: it reads the message from stdin and prints the lowercase hex
//! digest. Each test only performs cheap Wasm instantiation. Feeding inputs
//! across every length around the 64-byte block boundary exercises the
//! padding scheme exhaustively — the classic location for SHA bugs.

mod common;

use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::OnceLock;
use wasmtime::Store;
use wasmtime::component::Component;
use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder};

static DIGEST_COMPONENT: OnceLock<Component> = OnceLock::new();

fn digest_component() -> &'static Component {
    DIGEST_COMPONENT.get_or_init(|| {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sub/digest_driver.wado");
        let result = common::compile_file(&path)
            .unwrap_or_else(|e| panic!("Failed to compile digest_driver.wado: {e:?}"));
        let engine = common::cli_engine();
        Component::new(engine, &result.wasm)
            .unwrap_or_else(|e| panic!("Failed to create digest component: {e}"))
    })
}

fn run_component(stdin: &[u8]) -> String {
    // Resolve (and lazily compile) the component before entering the tokio
    // runtime: compilation itself blocks on a runtime and must not be nested.
    let component = digest_component();
    let rt = common::runtime();
    let engine = common::cli_engine();

    rt.block_on(async {
        let linker = common::cli_linker(engine).expect("failed to create linker");

        let stdout_pipe = MemoryOutputPipe::new(1 << 20);
        let stdout_clone = stdout_pipe.clone();
        let stderr_pipe = MemoryOutputPipe::new(65536);
        let stderr_clone = stderr_pipe.clone();

        let ctx: WasiCtx = WasiCtxBuilder::new()
            .stdin(MemoryInputPipe::new(stdin.to_vec()))
            .stdout(stdout_pipe)
            .stderr(stderr_pipe)
            .build();
        common::install_rustls_provider_for_tests();
        let state = common::WasiState {
            ctx,
            table: wasmtime::component::ResourceTable::new(),
            http_ctx: wasmtime_wasi_http::WasiHttpCtx::new(),
            http_hooks: common::TestHttpCtx::new(),
            tls_ctx: wasmtime_wasi_tls::WasiTlsCtxBuilder::new().build(),
        };
        let mut store = Store::new(engine, state);
        store.set_epoch_deadline(30);

        let instance = linker
            .instantiate_async(&mut store, component)
            .await
            .expect("failed to instantiate");
        let run_func = instance
            .get_typed_func::<(), (Result<(), ()>,)>(&mut store, "run")
            .expect("failed to get run func");

        match run_func.call_async(&mut store, ()).await {
            Ok((result,)) => {
                if result.is_err() {
                    let stderr =
                        String::from_utf8(stderr_clone.contents().to_vec()).unwrap_or_default();
                    panic!("Wasm component returned error. stderr: {stderr}");
                }
            }
            Err(e) => {
                let stderr =
                    String::from_utf8(stderr_clone.contents().to_vec()).unwrap_or_default();
                panic!("Wasm component trapped: {e:#}\nstderr: {stderr}");
            }
        }

        String::from_utf8(stdout_clone.contents().to_vec()).expect("stdout not valid UTF-8")
    })
}

fn reference_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let out = hasher.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for byte in out {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

fn assert_matches(data: &[u8]) {
    let got = run_component(data);
    let want = reference_hex(data);
    assert_eq!(
        got,
        want,
        "SHA-256 mismatch for input of length {}",
        data.len()
    );
}

#[test]
fn sha256_matches_reference_across_padding_boundaries() {
    // Every length from empty through two full blocks plus a tail. This spans
    // all padding cases: room for the length field (<=55 mod 64), the
    // overflow case that needs an extra block (56..=63 mod 64), and exact
    // block multiples.
    for len in 0..=130usize {
        let data: Vec<u8> = (0..len).map(|i| (i * 31 + 7) as u8).collect();
        assert_matches(&data);
    }
}

#[test]
fn sha256_matches_reference_for_larger_inputs() {
    for &len in &[255usize, 256, 257, 512, 1000, 4096, 65536, 100_000] {
        let data: Vec<u8> = (0..len).map(|i| (i * 131 + 17) as u8).collect();
        assert_matches(&data);
    }
}

#[test]
fn sha256_matches_reference_for_all_byte_values() {
    let data: Vec<u8> = (0..=255u8).collect();
    assert_matches(&data);
}
