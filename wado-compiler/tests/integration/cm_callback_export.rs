//! A closure passed to a `#[cm]` import crosses as a `u32` key, and the host
//! calls it back through the `wado:callback/callback` export.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use wasmtime::Store;
use wasmtime::component::{
    Component, ComponentNamedList, Instance, Lift, Linker, Lower, TypedFunc,
};
use wasmtime_wasi::p3::bindings::Command;

use crate::common::{
    DEFAULT_TIMEOUT_MS, WasiState, compile_source, engine, limit_store, linker, runtime,
};

const DECLARATIONS: &str = r#"
use { MonotonicClock } from "wasi:clocks";

#[cm("example:demo/target", linearity = "unrestricted", classes = "0..=0")]
resource Target {
    #[cm("example:demo/target#listen")]
    #[cm_params("self", "listener")]
    fn listen(&self, listener: fn mut(i32) with Demo);
    #[cm("example:demo/target#listen-f64")]
    #[cm_params("self", "listener")]
    fn listen_f64(&self, listener: fn mut(f64) with Demo);
    #[cm("example:demo/target#listen-target")]
    #[cm_params("self", "listener")]
    fn listen_target(&self, listener: fn mut(Target) with Demo);
}

#[cm("example:demo/global")]
interface Demo {
    #[cm("example:demo/global#target")]
    fn target() -> Target;
    #[cm("example:demo/global#report")]
    fn report(total: i32);
}

fn listen_summing() with Demo {
    let mut total = 0;
    Demo::target().listen(|v: i32| {
        total += v;
        Demo::report(total);
    });
}
"#;

#[derive(Default)]
struct Host {
    listeners: Vec<u32>,
    reports: Vec<i32>,
}

type Shared = Arc<Mutex<Host>>;

fn add_demo_to_linker(linker: &mut Linker<WasiState>, host: &Shared) -> anyhow::Result<()> {
    let mut global = linker.instance("example:demo/global")?;
    global.func_wrap("target", |_, (): ()| Ok((0.0_f64,)))?;
    let reports = Arc::clone(host);
    global.func_wrap("report", move |_, (total,): (i32,)| {
        reports.lock().unwrap().reports.push(total);
        Ok(())
    })?;
    let mut target = linker.instance("example:demo/target")?;
    for listen in ["listen", "listen-f64", "listen-target"] {
        let listeners = Arc::clone(host);
        target.func_wrap(listen, move |_, (_target, key): (f64, u32)| {
            listeners.lock().unwrap().listeners.push(key);
            Ok(())
        })?;
    }
    Ok(())
}

/// The export of `wado:callback/callback` named `name`.
fn callback_export<Params, Return>(
    store: &mut Store<WasiState>,
    instance: &Instance,
    name: &str,
) -> anyhow::Result<TypedFunc<Params, Return>>
where
    Params: ComponentNamedList + Lower,
    Return: ComponentNamedList + Lift,
{
    let interface = instance
        .get_export_index(&mut *store, None, "wado:callback/callback")
        .expect("the component exports the callback interface");
    let call = instance
        .get_export_index(&mut *store, Some(&interface), name)
        .unwrap_or_else(|| panic!("the callback interface exports `{name}`"));
    Ok(instance.get_typed_func(store, call)?)
}

/// Instantiate `DECLARATIONS` followed by `run`, hand `body` the instance, and
/// answer what the host recorded.
fn with_program(
    run: &str,
    body: impl AsyncFnOnce(&mut Store<WasiState>, Command, &Instance, &Shared) -> anyhow::Result<()>,
) -> Host {
    let wasm = compile_source(&format!("{DECLARATIONS}{run}"))
        .unwrap_or_else(|e| panic!("the program should compile, got {e}"))
        .wasm;
    let host = Arc::new(Mutex::new(Host::default()));
    let engine = engine();
    runtime()
        .block_on(async {
            let component = Component::new(engine, &wasm)?;
            let mut linker = linker(engine)?;
            add_demo_to_linker(&mut linker, &host)?;
            let state = WasiState::new_with_pipes(
                wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(65536),
                wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(65536),
            );
            let mut store = Store::new(engine, state);
            limit_store(&mut store, DEFAULT_TIMEOUT_MS);
            let instance = linker.instantiate_async(&mut store, &component).await?;
            let command = Command::new(&mut store, &instance)?;
            body(&mut store, command, &instance, &host).await
        })
        .unwrap_or_else(|e| panic!("the program should run: {e:#}"));
    Arc::into_inner(host)
        .expect("the store is gone, so the host has one owner")
        .into_inner()
        .unwrap()
}

async fn run_to_completion(store: &mut Store<WasiState>, command: Command) -> anyhow::Result<()> {
    store
        .run_concurrent(async |accessor| command.wasi_cli_run().call_run(accessor).await)
        .await??
        .map_err(|()| anyhow::anyhow!("run() returned an error"))
}

fn listeners(host: &Shared) -> Vec<u32> {
    host.lock().unwrap().listeners.clone()
}

#[test]
fn the_host_calls_a_closure_back_by_its_key() {
    let host = with_program(
        "export fn run() with Demo { listen_summing(); }",
        async |store, command, instance, host| {
            run_to_completion(store, command).await?;
            let call = callback_export::<(u32, i32), ()>(store, instance, "call-i32")?;
            let [key] = listeners(host)[..] else {
                panic!("the program listens once");
            };
            for value in [5, 7] {
                call.call_async(&mut *store, (key, value)).await?;
            }
            Ok(())
        },
    );
    // `total` outlives `run`, so each call back sees what the last one wrote.
    assert_eq!(host.reports, vec![5, 12]);
}

/// An event reaches a listener while the task that registered it is suspended.
#[test]
fn a_callback_enters_while_run_is_suspended() {
    let host = with_program(
        "export fn run() with (Demo, MonotonicClock) {
            listen_summing();
            MonotonicClock::wait_for(200_000_000).wait();
            Demo::report(-1);
        }",
        async |store, command, instance, host| {
            let call = callback_export::<(u32, i32), ()>(store, instance, "call-i32")?;
            store
                .run_concurrent(async |accessor| {
                    let fire = async {
                        while host.lock().unwrap().listeners.is_empty() {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                        call.call_concurrent(accessor, (listeners(host)[0], 5))
                            .await
                    };
                    let (ran, fired) =
                        futures::join!(command.wasi_cli_run().call_run(accessor), fire);
                    fired?;
                    ran?.map_err(|()| anyhow::anyhow!("run() returned an error"))
                })
                .await?
        },
    );
    assert_eq!(host.reports, vec![5, -1]);
}

/// A handle crosses as an `f64` but is no `f64` in the guest, so a closure
/// taking one has an export of its own.
#[test]
fn a_handle_argument_and_an_f64_argument_call_back_apart() {
    let host = with_program(
        "export fn run() with Demo {
            let target = Demo::target();
            target.listen_f64(|v: f64| Demo::report(v as i32));
            target.listen_target(|t: Target| Demo::report(if t == Demo::target() { 100 } else { -100 }));
        }",
        async |store, command, instance, host| {
            run_to_completion(store, command).await?;
            let [of_f64, of_target] = listeners(host)[..] else {
                panic!("the program listens twice");
            };
            let call_f64 = callback_export::<(u32, f64), ()>(store, instance, "call-f64")?;
            call_f64.call_async(&mut *store, (of_f64, 3.0)).await?;
            let call_handle = callback_export::<(u32, f64), ()>(store, instance, "call-handle")?;
            call_handle.call_async(&mut *store, (of_target, 0.0)).await?;
            Ok(())
        },
    );
    assert_eq!(host.reports, vec![3, 100]);
}
