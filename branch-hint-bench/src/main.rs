//! Branch-hinting A/B benchmark for wado-produced components.
//!
//! Runs the *same* wasm twice on a patched wasmtime 46 — once with
//! `Config::wasm_branch_hinting(false)` and once with `true` — so the only
//! difference is whether wasmtime acts on the `metadata.code.branch_hint`
//! section wado emitted. wado's own optimization is held constant (identical
//! bytes), isolating the cold-block layout effect.
//!
//! Usage: `cargo run --release -- [path/to/component.wasm] [runs]`

use std::time::Instant;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, OptLevel, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

struct State {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

fn make_config(branch_hinting: bool) -> Config {
    // Mirror wado's runtime feature set so the component validates/compiles the
    // same way in both arms; only `wasm_branch_hinting` differs.
    let mut c = Config::new();
    c.wasm_component_model(true);
    c.wasm_component_model_async(true);
    c.wasm_component_model_more_async_builtins(true);
    c.wasm_component_model_async_stackful(true);
    c.wasm_gc(true);
    c.wasm_function_references(true);
    c.wasm_threads(true);
    c.cranelift_opt_level(OptLevel::Speed);
    c.wasm_branch_hinting(branch_hinting);
    c
}

async fn measure(
    wasm: &[u8],
    branch_hinting: bool,
    warmup: usize,
    runs: usize,
) -> anyhow::Result<Vec<f64>> {
    let engine = Engine::new(&make_config(branch_hinting))?;
    let component = Component::new(&engine, wasm)?;
    let mut linker: Linker<State> = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let state = State {
        ctx: WasiCtx::builder().inherit_stdio().build(),
        table: ResourceTable::new(),
    };
    let mut store = Store::new(&engine, state);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    // wado exports a bare top-level `run: async func() -> result`.
    let run = instance.get_typed_func::<(), (Result<(), ()>,)>(&mut store, "run")?;

    let mut times = Vec::with_capacity(runs);
    for i in 0..(warmup + runs) {
        let start = Instant::now();
        let (res,) = run.call_async(&mut store, ()).await?;
        let dt = start.elapsed().as_secs_f64();
        res.map_err(|()| anyhow::anyhow!("guest `run` returned an error"))?;
        if i >= warmup {
            times.push(dt);
        }
    }
    Ok(times)
}

fn summarize(mut t: Vec<f64>) -> (f64, f64) {
    t.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (t[0], t[t.len() / 2])
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/branch_hint.wasm".to_string());
    let runs: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let wasm = std::fs::read(&path)?;

    eprintln!("benchmarking {path}: {runs} timed runs each (+2 warmup)\n");
    let off = summarize(measure(&wasm, false, 2, runs).await?);
    let on = summarize(measure(&wasm, true, 2, runs).await?);

    eprintln!("branch-hinting OFF: min {:.4}s  median {:.4}s", off.0, off.1);
    eprintln!("branch-hinting ON : min {:.4}s  median {:.4}s", on.0, on.1);
    eprintln!(
        "\nimprovement: {:+.2}% (min)  {:+.2}% (median)",
        (off.0 - on.0) / off.0 * 100.0,
        (off.1 - on.1) / off.1 * 100.0,
    );
    Ok(())
}
