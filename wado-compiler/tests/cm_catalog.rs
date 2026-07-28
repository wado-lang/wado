//! Lift/lower binding test for Component Model value types outside any WASI
//! world.
//!
//! `tests/fixtures/cm_catalog.wado` enumerates the CM value-type ABI surface as
//! `identity` exports (each returns its argument unchanged). This harness
//! compiles that fixture as a `--lib` (a synthesized library world, the
//! `wado compile --lib` path), instantiates the component, and round-trips a
//! crafted [`Val`] through every `id_*` export, asserting `lift(lower(x)) == x`.
//!
//! Because the exports are identities, equality of the returned value with the
//! input is the whole oracle — one data-driven table covers the entire value
//! surface without per-type Rust bindings.
//!
//! The same fixture is also discovered by `e2e.rs` under the test world (it
//! carries an empty `test` block); that run only confirms a library-shaped
//! source compiles and instantiates. The lift/lower assertions live here.

#![allow(unused_crate_dependencies)]

mod common;

use std::path::Path;

use std::pin::Pin;
use std::task::{Context, Poll};

use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use wado_compiler::{CompilerOptions, OptLevel};
use wasmtime::component::{
    Component, ComponentExportIndex, Destination, FutureAny, FutureConsumer, FutureReader,
    Instance, Source, StreamAny, StreamConsumer, StreamProducer, StreamReader, StreamResult, Val,
    VecBuffer,
};
use wasmtime::{AsContextMut, Store, StoreContextMut};

/// Stream producer that delivers a batch with `Completed`, then signals
/// end-of-stream with a separate `Dropped` poll. Unlike the built-in `Vec`
/// producer (which coalesces data and the drop into one `Dropped(n)` result),
/// this models the common streaming shape — stdin, an HTTP body — where the
/// final data and the close arrive on distinct reads. That lets a guest drive
/// the idiomatic `loop { read; if empty break }` consume pattern without
/// reading past the close.
struct ChunkedStreamProducer<T>(Option<Vec<T>>);

impl<D, T> StreamProducer<D> for ChunkedStreamProducer<T>
where
    T: wasmtime::component::Lower + Unpin + Send + Sync + 'static,
{
    type Item = T;
    type Buffer = VecBuffer<T>;

    fn poll_produce<'a>(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _store: StoreContextMut<'a, D>,
        mut destination: Destination<'a, T, VecBuffer<T>>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        match self.get_mut().0.take() {
            Some(items) => {
                destination.set_buffer(items.into());
                Poll::Ready(Ok(StreamResult::Completed))
            }
            None => Poll::Ready(Ok(StreamResult::Dropped)),
        }
    }
}

/// Future consumer that forwards the single lifted payload to a oneshot channel,
/// so the host can assert the value after driving the event loop — the oracle
/// for "the payload survived", not just "the handle re-typed".
struct OneshotConsumer<T>(Option<oneshot::Sender<T>>);

impl<T> OneshotConsumer<T> {
    fn new(tx: oneshot::Sender<T>) -> Self {
        Self(Some(tx))
    }
}

impl<D, T> FutureConsumer<D> for OneshotConsumer<T>
where
    T: wasmtime::component::Lift + Send + 'static,
{
    type Item = T;

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        store: StoreContextMut<D>,
        mut source: Source<'_, T>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<()>> {
        let value = &mut None;
        source.read(store, value)?;
        let _ = self
            .get_mut()
            .0
            .take()
            .expect("future consumed once")
            .send(value.take().expect("future value lifted"));
        Poll::Ready(Ok(()))
    }
}

/// Stream consumer that forwards each lifted element to an unbounded channel.
/// The channel closes when the stream ends, letting the host collect the whole
/// payload and compare it against the input.
struct StreamCollectConsumer<T>(mpsc::UnboundedSender<T>);

impl<T> StreamCollectConsumer<T> {
    fn new(tx: mpsc::UnboundedSender<T>) -> Self {
        Self(tx)
    }
}

impl<D, T> StreamConsumer<D> for StreamCollectConsumer<T>
where
    T: wasmtime::component::Lift + Send + 'static,
{
    type Item = T;

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: StoreContextMut<D>,
        mut source: Source<'_, T>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if source.remaining(store.as_context_mut()) == 0 {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let item = &mut None;
        source.read(store, item)?;
        let _ = self
            .get_mut()
            .0
            .unbounded_send(item.take().expect("stream item lifted"));
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

/// FQ of the synthesized library world. Mirrors `lib_world_fq` in
/// `wado-cli`: `namespace:name/name@version`.
const LIB_WORLD_FQ: &str = "wado-lang:cm-catalog/cm-catalog@0.0.16";

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/cm_catalog.wado"
);

/// A single round-trip case: call the kebab-named export with `value` and
/// assert the returned value equals `value`.
struct Case {
    export: &'static str,
    value: Val,
}

fn case(export: &'static str, value: Val) -> Case {
    Case { export, value }
}

fn b(v: Val) -> Option<Box<Val>> {
    Some(Box::new(v))
}

/// The full value-type catalog as round-trip cases. Mirrors the `id_*` exports
/// in `tests/fixtures/cm_catalog.wado`.
fn cases() -> Vec<Case> {
    let point = || {
        Val::Record(vec![
            ("x".into(), Val::Float64(1.5)),
            ("y".into(), Val::Float64(-2.5)),
        ])
    };
    vec![
        // Primitives.
        case("id-bool", Val::Bool(true)),
        case("id-u8", Val::U8(0xAB)),
        case("id-u16", Val::U16(0xBEEF)),
        case("id-u32", Val::U32(0xDEAD_BEEF)),
        case("id-u64", Val::U64(0x0123_4567_89AB_CDEF)),
        case("id-s8", Val::S8(-12)),
        case("id-s16", Val::S16(-1234)),
        case("id-s32", Val::S32(-123_456)),
        case("id-s64", Val::S64(-1_234_567_890_123)),
        case("id-f32", Val::Float32(3.5)),
        case("id-f64", Val::Float64(-7.25)),
        case("id-char", Val::Char('λ')),
        case("id-string", Val::String("héllo, wörld".into())),
        // Built-in containers.
        case(
            "id-list-u8",
            Val::List(vec![Val::U8(1), Val::U8(2), Val::U8(3)]),
        ),
        case(
            "id-list-string",
            Val::List(vec![Val::String("a".into()), Val::String("bb".into())]),
        ),
        case(
            "id-tuple-pair",
            Val::Tuple(vec![Val::U32(7), Val::String("x".into())]),
        ),
        case(
            "id-tuple-triple",
            Val::Tuple(vec![Val::U8(9), Val::Bool(true), Val::Char('z')]),
        ),
        case("id-option-u32", Val::Option(b(Val::U32(42)))),
        case("id-option-string", Val::Option(None)),
        // `result` in all four WIT forms.
        case("id-result-both", Val::Result(Ok(b(Val::U32(5))))),
        case("id-result-ok", Val::Result(Ok(b(Val::U32(6))))),
        case(
            "id-result-err",
            Val::Result(Err(b(Val::String("boom".into())))),
        ),
        case("id-result-unit", Val::Result(Ok(None))),
        // Named value types.
        case("id-record", point()),
        // A record that flattens to a single core value is returned flat, not
        // via an outptr.
        case(
            "id-record-flat",
            Val::Record(vec![("value".into(), Val::U64(42))]),
        ),
        case("id-enum", Val::Enum("green".into())),
        case(
            "id-variant",
            Val::Variant("circle".into(), b(Val::Float64(4.0))),
        ),
        case(
            "id-flags",
            Val::Flags(vec!["read".into(), "execute".into()]),
        ),
        case("id-newtype", Val::Float64(100.0)),
        // Newtype inside option/result: the payload round-trips through the
        // newtype's `f64` base, including the result join that widens the
        // `f64`/`u32` arms to a shared 64-bit slot.
        case("id-option-newtype", Val::Option(b(Val::Float64(2.5)))),
        case("id-option-newtype", Val::Option(None)),
        case("id-result-newtype", Val::Result(Ok(b(Val::Float64(1.5))))),
        case("id-result-newtype", Val::Result(Err(b(Val::U32(42))))),
        // Mixed-core-class variant: the `as-float` arm's f32 payload shares a
        // slot the join widened to i32, so it must be bit-reinterpreted.
        case(
            "id-mixed",
            Val::Variant("as-float".into(), b(Val::Float32(2.5))),
        ),
        case("id-mixed", Val::Variant("as-int".into(), b(Val::U32(7)))),
        // Mixed-core-class result: the `err` arm's u32 shares an f32-widened slot.
        case("id-result-fu", Val::Result(Err(b(Val::U32(42))))),
        case("id-result-fu", Val::Result(Ok(b(Val::Float32(1.5))))),
        // Nested compositions.
        case(
            "id-list-option",
            Val::List(vec![Val::Option(b(Val::U32(1))), Val::Option(None)]),
        ),
        case(
            "id-option-list",
            Val::Option(b(Val::List(vec![Val::U8(1), Val::U8(2)]))),
        ),
        case("id-list-record", Val::List(vec![point(), point()])),
        case("id-option-record", Val::Option(b(point()))),
        case(
            "id-list-tuple",
            Val::List(vec![Val::Tuple(vec![Val::U32(1), Val::String("a".into())])]),
        ),
        case(
            "id-result-list",
            Val::Result(Ok(b(Val::List(vec![point()])))),
        ),
        // Association list (the CM map idiom `list<tuple<k, v>>`).
        case(
            "id-assoc-array",
            Val::List(vec![Val::Tuple(vec![Val::String("k".into()), Val::U32(1)])]),
        ),
        // Tuples carrying aggregate (non-primitive) elements.
        case("id-tuple-record", Val::Tuple(vec![point(), Val::U32(7)])),
        case(
            "id-tuple-option",
            Val::Tuple(vec![Val::Option(b(Val::U32(9))), Val::U8(3)]),
        ),
        case(
            "id-tuple-list",
            Val::Tuple(vec![
                Val::List(vec![Val::String("a".into()), Val::String("bb".into())]),
                Val::U32(5),
            ]),
        ),
        case(
            "id-list-tuple-record",
            Val::List(vec![
                Val::Tuple(vec![point(), Val::U32(1)]),
                Val::Tuple(vec![point(), Val::U32(2)]),
            ]),
        ),
    ]
}

fn lookup_func(
    store: &mut Store<common::WasiState>,
    instance: &Instance,
    iface: Option<&ComponentExportIndex>,
    export: &str,
) -> Result<wasmtime::component::Func, String> {
    // Named-type libraries group exports into a default interface; a library of
    // only structural types exports them directly at the component root. Try the
    // interface first, then fall back to the root.
    iface
        .and_then(|i| instance.get_export(&mut *store, Some(i), export))
        .or_else(|| instance.get_export(&mut *store, None, export))
        .map(|(_, idx)| idx)
        .and_then(|idx| instance.get_func(&mut *store, idx))
        .ok_or_else(|| format!("export `${export}` not found"))
}

/// Round-trip a future's payload and assert it survived, not just the handle:
/// lower a host-created `future<T>` carrying `payload`, then pipe the returned
/// future into a oneshot and assert the lifted value equals `payload`.
async fn future_round_trip<T>(
    store: &mut Store<common::WasiState>,
    instance: &Instance,
    iface: Option<&ComponentExportIndex>,
    export: &'static str,
    payload: T,
) -> Result<(), String>
where
    T: wasmtime::component::Lower
        + wasmtime::component::Lift
        + Clone
        + PartialEq
        + std::fmt::Debug
        + Send
        + Sync
        + 'static,
{
    let expected = payload.clone();
    let func = lookup_func(store, instance, iface, export)?;
    let f = FutureReader::new(&mut *store, async move { wasmtime::error::Ok(payload) })
        .map_err(|e| format!("`${export}`: host future create failed: {e:#}"))?;
    let any = f
        .try_into_future_any(&mut *store)
        .map_err(|e| format!("`${export}`: future -> any failed: {e:#}"))?;
    let mut results = vec![Val::Bool(false); 1];
    func.call_async(&mut *store, &[Val::Future(any)], &mut results)
        .await
        .map_err(|e| format!("`${export}`: call trapped: {e:#}"))?;
    let out = match results.into_iter().next() {
        Some(Val::Future(a)) => a,
        other => {
            return Err(format!(
                "`${export}`: expected a future result, got {other:?}"
            ));
        }
    };
    let reader = out.try_into_future_reader::<T>().map_err(|e| {
        format!(
            "`${export}`: result is not future<{}>: {e:#}",
            std::any::type_name::<T>()
        )
    })?;
    let (tx, rx) = oneshot::channel::<T>();
    reader
        .pipe(&mut *store, OneshotConsumer::new(tx))
        .map_err(|e| format!("`${export}`: pipe failed: {e:#}"))?;
    let got = store
        .run_concurrent(async move |_| rx.await)
        .await
        .map_err(|e| format!("`${export}`: run_concurrent failed: {e:#}"))?
        .map_err(|_| format!("`${export}`: future closed without a value"))?;
    if got != expected {
        return Err(format!(
            "`${export}`: future payload mismatch\n  in:  {expected:?}\n  out: {got:?}"
        ));
    }
    Ok(())
}

async fn stream_round_trip<T>(
    store: &mut Store<common::WasiState>,
    instance: &Instance,
    iface: Option<&ComponentExportIndex>,
    export: &'static str,
    payload: Vec<T>,
) -> Result<(), String>
where
    T: wasmtime::component::Lower
        + wasmtime::component::Lift
        + Clone
        + PartialEq
        + std::fmt::Debug
        + Send
        + Sync
        + Unpin
        + 'static,
{
    let expected = payload.clone();
    let func = lookup_func(store, instance, iface, export)?;
    let s = StreamReader::new(&mut *store, ChunkedStreamProducer(Some(payload)))
        .map_err(|e| format!("`${export}`: host stream create failed: {e:#}"))?;
    let any = s
        .try_into_stream_any(&mut *store)
        .map_err(|e| format!("`${export}`: stream -> any failed: {e:#}"))?;
    let mut results = vec![Val::Bool(false); 1];
    func.call_async(&mut *store, &[Val::Stream(any)], &mut results)
        .await
        .map_err(|e| format!("`${export}`: call trapped: {e:#}"))?;
    let out = match results.into_iter().next() {
        Some(Val::Stream(a)) => a,
        other => {
            return Err(format!(
                "`${export}`: expected a stream result, got {other:?}"
            ));
        }
    };
    let reader = out.try_into_stream_reader::<T>().map_err(|e| {
        format!(
            "`${export}`: result is not stream<{}>: {e:#}",
            std::any::type_name::<T>()
        )
    })?;
    let (tx, mut rx) = mpsc::unbounded::<T>();
    reader
        .pipe(&mut *store, StreamCollectConsumer::new(tx))
        .map_err(|e| format!("`${export}`: pipe failed: {e:#}"))?;
    let got = store
        .run_concurrent(async move |_| {
            let mut items = Vec::new();
            while let Some(item) = rx.next().await {
                items.push(item);
            }
            items
        })
        .await
        .map_err(|e| format!("`${export}`: run_concurrent failed: {e:#}"))?;
    if got != expected {
        return Err(format!(
            "`${export}`: stream payload mismatch\n  in:  {expected:?}\n  out: {got:?}"
        ));
    }
    Ok(())
}

/// `wrap` builds the input `Val` around a `future<u32>`; `unwrap` extracts the
/// inner `FutureAny` from the lifted result.
async fn embedded_future_round_trip(
    store: &mut Store<common::WasiState>,
    instance: &Instance,
    iface: Option<&ComponentExportIndex>,
    export: &'static str,
    wrap: impl FnOnce(Val) -> Val,
    unwrap: impl FnOnce(Val) -> Option<FutureAny>,
) -> Result<(), String> {
    let f = FutureReader::new(&mut *store, async { wasmtime::error::Ok(0xFEED_u32) })
        .map_err(|e| format!("`${export}`: host future create failed: {e:#}"))?;
    let any = f
        .try_into_future_any(&mut *store)
        .map_err(|e| format!("`${export}`: future -> any failed: {e:#}"))?;
    let func = lookup_func(store, instance, iface, export)?;
    let mut results = vec![Val::Bool(false); 1];
    func.call_async(&mut *store, &[wrap(Val::Future(any))], &mut results)
        .await
        .map_err(|e| format!("`${export}`: call trapped: {e:#}"))?;
    let result = results.into_iter().next().unwrap_or(Val::Bool(false));
    let inner = unwrap(result)
        .ok_or_else(|| format!("`${export}`: result did not carry the inner future"))?;
    let reader = inner
        .try_into_future_reader::<u32>()
        .map_err(|e| format!("`${export}`: inner handle is not future<u32>: {e:#}"))?;
    let (tx, rx) = oneshot::channel::<u32>();
    reader
        .pipe(&mut *store, OneshotConsumer::new(tx))
        .map_err(|e| format!("`${export}`: pipe failed: {e:#}"))?;
    let got = store
        .run_concurrent(async move |_| rx.await)
        .await
        .map_err(|e| format!("`${export}`: run_concurrent failed: {e:#}"))?
        .map_err(|_| format!("`${export}`: inner future closed without a value"))?;
    if got != 0xFEED_u32 {
        return Err(format!(
            "`${export}`: inner future payload mismatch: expected 0xFEED, got {got:#x}"
        ));
    }
    Ok(())
}

async fn embedded_stream_round_trip(
    store: &mut Store<common::WasiState>,
    instance: &Instance,
    iface: Option<&ComponentExportIndex>,
    export: &'static str,
    wrap: impl FnOnce(Val) -> Val,
    unwrap: impl FnOnce(Val) -> Option<StreamAny>,
) -> Result<(), String> {
    let s = StreamReader::new(&mut *store, vec![1u8, 2, 3])
        .map_err(|e| format!("`${export}`: host stream create failed: {e:#}"))?;
    let any = s
        .try_into_stream_any(&mut *store)
        .map_err(|e| format!("`${export}`: stream -> any failed: {e:#}"))?;
    let func = lookup_func(store, instance, iface, export)?;
    let mut results = vec![Val::Bool(false); 1];
    func.call_async(&mut *store, &[wrap(Val::Stream(any))], &mut results)
        .await
        .map_err(|e| format!("`${export}`: call trapped: {e:#}"))?;
    let result = results.into_iter().next().unwrap_or(Val::Bool(false));
    let inner = unwrap(result)
        .ok_or_else(|| format!("`${export}`: result did not carry the inner stream"))?;
    let reader = inner
        .try_into_stream_reader::<u8>()
        .map_err(|e| format!("`${export}`: inner handle is not stream<u8>: {e:#}"))?;
    let (tx, mut rx) = mpsc::unbounded::<u8>();
    reader
        .pipe(&mut *store, StreamCollectConsumer::new(tx))
        .map_err(|e| format!("`${export}`: pipe failed: {e:#}"))?;
    let got = store
        .run_concurrent(async move |_| {
            let mut items = Vec::new();
            while let Some(item) = rx.next().await {
                items.push(item);
            }
            items
        })
        .await
        .map_err(|e| format!("`${export}`: run_concurrent failed: {e:#}"))?;
    if got != vec![1u8, 2, 3] {
        return Err(format!(
            "`${export}`: inner stream payload mismatch: expected [1, 2, 3], got {got:?}"
        ));
    }
    Ok(())
}

/// Compile the catalog fixture as a library world at `opt_level`, under the
/// `debug` allocator: it poisons freed memory, surfacing use-after-free at the
/// boundary.
fn compile_catalog(opt_level: OptLevel) -> Vec<u8> {
    compile_catalog_with_allocator(opt_level, "debug")
}

fn compile_catalog_with_allocator(opt_level: OptLevel, allocator: &str) -> Vec<u8> {
    let source = std::fs::read_to_string(FIXTURE).expect("read cm_catalog fixture");
    let options = CompilerOptions {
        opt_level,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        allocator: Some(allocator.to_string()),
        ..Default::default()
    };
    common::compile_source_with_compiler_options(Path::new(FIXTURE), &source, options)
        .expect("catalog failed to compile as a library world")
        .wasm
}

/// Round-trip every case through the compiled component, asserting identity.
fn run_round_trips(opt_level: OptLevel) {
    let wasm = compile_catalog(opt_level);
    let engine = common::engine();
    let rt = common::runtime();
    let opt = common::opt_level_name(opt_level);

    rt.block_on(async {
        let component = Component::new(engine, &wasm).expect("instantiate component type");
        // The value-type library still imports the prelude's `wasi:cli/stderr`
        // (assertion/format diagnostics), so use the shared WASI linker.
        let linker = common::linker(engine).expect("build linker");
        let state = common::WasiState::new_with_pipes(
            wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(65536),
            wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(65536),
        );
        let mut store = Store::new(engine, state);
        // The shared engine enables epoch interruption; without a deadline the
        // first call traps with `interrupt`.
        store.set_epoch_deadline((common::DEFAULT_TIMEOUT_MS / 1000).max(1));
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate library component");

        // Named-type exports group into the default interface; resolve the
        // `wado-lang:cm-catalog/cm-catalog@…` instance once and look funcs up inside.
        let iface = instance
            .get_export(&mut store, None, LIB_WORLD_FQ)
            .map(|(_, idx)| idx);

        let mut failures = Vec::new();
        for Case { export, value } in cases() {
            let func = iface
                .as_ref()
                .and_then(|i| instance.get_export(&mut store, Some(i), export))
                .map(|(_, idx)| idx)
                .and_then(|idx| instance.get_func(&mut store, idx));
            let Some(func) = func else {
                failures.push(format!("[{opt}] export `${export}` not found"));
                continue;
            };
            let result_arity = func.ty(&store).results().len();
            let mut results = vec![Val::Bool(false); result_arity];
            match func.call_async(&mut store, std::slice::from_ref(&value), &mut results).await {
                Ok(()) => {
                    if results.len() != 1 {
                        failures.push(format!(
                            "[{opt}] `${export}`: expected 1 result, got {}",
                            results.len()
                        ));
                    } else if results[0] != value {
                        failures.push(format!(
                            "[{opt}] `${export}`: round-trip mismatch\n  in:  {value:?}\n  out: {:?}",
                            results[0]
                        ));
                    }
                }
                Err(e) => failures.push(format!("[{opt}] `${export}`: call trapped: {e:#}")),
            }
        }

        macro_rules! check {
            ($call:expr) => {
                if let Err(e) = $call.await {
                    failures.push(format!("[{opt}] {e}"));
                }
            };
        }
        let i = iface.as_ref();

        check!(future_round_trip(&mut store, &instance, i, "id-future-bool", true));
        check!(future_round_trip(&mut store, &instance, i, "id-future-u8", 0xABu8));
        check!(future_round_trip(&mut store, &instance, i, "id-future-u16", 0xBEEFu16));
        check!(future_round_trip(&mut store, &instance, i, "id-future-u32", 0xDEAD_BEEFu32));
        check!(future_round_trip(&mut store, &instance, i, "id-future-u64", 0x0123_4567_89AB_CDEFu64));
        check!(future_round_trip(&mut store, &instance, i, "id-future-s8", -12i8));
        check!(future_round_trip(&mut store, &instance, i, "id-future-s16", -1234i16));
        check!(future_round_trip(&mut store, &instance, i, "id-future-s32", -123_456i32));
        check!(future_round_trip(&mut store, &instance, i, "id-future-s64", -1_234_567_890_123i64));
        check!(future_round_trip(&mut store, &instance, i, "id-future-f32", 3.5f32));
        check!(future_round_trip(&mut store, &instance, i, "id-future-f64", -7.25f64));
        check!(future_round_trip(&mut store, &instance, i, "id-future-char", 'λ'));

        // Aggregate future consume/produce (async exports; the payload
        // round-trips through CM linear memory, not just the handle).
        check!(future_round_trip(&mut store, &instance, i, "id-future-string", "héllo, wörld".to_string()));
        check!(future_round_trip(&mut store, &instance, i, "id-future-option", Some(42u32)));
        check!(future_round_trip(&mut store, &instance, i, "id-future-result", Ok::<u32, String>(7)));
        check!(future_round_trip(&mut store, &instance, i, "id-future-list", vec![1u32, 2, 3]));
        check!(future_round_trip(&mut store, &instance, i, "id-future-tuple", (5u32, "x".to_string())));
        check!(future_round_trip(&mut store, &instance, i, "id-future-record", Point { x: 1.5, y: -2.5 }));

        check!(stream_round_trip(&mut store, &instance, i, "id-stream-u8", vec![1u8, 2, 3, 4]));
        // Stream consume/produce: each element round-trips through CM memory.
        check!(stream_round_trip(&mut store, &instance, i, "id-stream-u32", vec![10u32, 20, 30, 40]));
        check!(stream_round_trip(
            &mut store, &instance, i, "id-stream-string",
            vec!["a".to_string(), "bb".to_string(), "céç".to_string()],
        ));
        check!(stream_round_trip(
            &mut store, &instance, i, "id-stream-record",
            vec![Point { x: 1.0, y: 2.0 }, Point { x: -3.5, y: 4.5 }],
        ));

        check!(embedded_future_round_trip(
            &mut store, &instance, i, "id-option-future",
            |v| Val::Option(Some(Box::new(v))),
            |r| match r {
                Val::Option(Some(b)) => match *b {
                    Val::Future(a) => Some(a),
                    _ => None,
                },
                _ => None,
            },
        ));
        check!(embedded_future_round_trip(
            &mut store, &instance, i, "id-result-future",
            |v| Val::Result(Ok(Some(Box::new(v)))),
            |r| match r {
                Val::Result(Ok(Some(b))) => match *b {
                    Val::Future(a) => Some(a),
                    _ => None,
                },
                _ => None,
            },
        ));
        check!(embedded_future_round_trip(
            &mut store, &instance, i, "id-list-future",
            |v| Val::List(vec![v]),
            |r| match r {
                Val::List(items) => items.into_iter().find_map(|x| match x {
                    Val::Future(a) => Some(a),
                    _ => None,
                }),
                _ => None,
            },
        ));
        check!(embedded_future_round_trip(
            &mut store, &instance, i, "id-tuple-future",
            |v| Val::Tuple(vec![v, Val::U32(7)]),
            |r| match r {
                Val::Tuple(items) => items.into_iter().find_map(|x| match x {
                    Val::Future(a) => Some(a),
                    _ => None,
                }),
                _ => None,
            },
        ));
        check!(embedded_future_round_trip(
            &mut store, &instance, i, "id-record-future",
            |v| Val::Record(vec![("value".to_string(), v)]),
            |r| match r {
                Val::Record(fields) => fields.into_iter().find_map(|(name, x)| match x {
                    Val::Future(a) if name == "value" => Some(a),
                    _ => None,
                }),
                _ => None,
            },
        ));
        check!(embedded_stream_round_trip(
            &mut store, &instance, i, "id-list-stream",
            |v| Val::List(vec![v]),
            |r| match r {
                Val::List(items) => items.into_iter().find_map(|x| match x {
                    Val::Stream(a) => Some(a),
                    _ => None,
                }),
                _ => None,
            },
        ));

        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    });
}

/// A producer library: each `mk_future_*` lowers an aggregate value into a fresh
/// `future<T>` and returns the readable end. Exercises `synthesize_future_writes`
/// (the aggregate lower path) end to end — the guest writes, the host reads back
/// and asserts the payload survived. The write is cross-component (the host reads
/// it), so the BLOCKED write keeps its buffer alive without the guest blocking,
/// which is why a plain `export fn` suffices here (unlike a consuming read, which
/// would block and require an async task).
const PRODUCER_SOURCE: &str = r#"
export fn mk_future_string(s: String) -> Future<String> {
    let [rx, tx] = Future::<String>::new();
    tx.write(s);
    return rx;
}
export fn mk_future_option(v: Option<u32>) -> Future<Option<u32>> {
    let [rx, tx] = Future::<Option<u32>>::new();
    tx.write(v);
    return rx;
}
export fn mk_future_result(v: Result<u32, String>) -> Future<Result<u32, String>> {
    let [rx, tx] = Future::<Result<u32, String>>::new();
    tx.write(v);
    return rx;
}
export fn mk_future_list(v: List<u32>) -> Future<List<u32>> {
    let [rx, tx] = Future::<List<u32>>::new();
    tx.write(v);
    return rx;
}
export fn mk_future_tuple(v: [u32, String]) -> Future<[u32, String]> {
    let [rx, tx] = Future::<[u32, String]>::new();
    tx.write(v);
    return rx;
}
"#;

/// Compile an inline library source to a component, with the debug allocator.
fn compile_lib_source(source: &str, opt_level: OptLevel) -> Vec<u8> {
    let options = CompilerOptions {
        opt_level,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        allocator: Some("debug".to_string()),
        ..Default::default()
    };
    common::compile_source_with_compiler_options(Path::new("lib.wado"), source, options)
        .expect("inline library failed to compile")
        .wasm
}

/// Call a `mk_future_*` producer with `input`, then read its produced future back
/// on the host and assert the lifted value equals `expected`.
async fn produce_and_read_back<T>(
    store: &mut Store<common::WasiState>,
    instance: &Instance,
    iface: Option<&ComponentExportIndex>,
    export: &'static str,
    input: Val,
    expected: T,
) -> Result<(), String>
where
    T: wasmtime::component::Lift + PartialEq + std::fmt::Debug + Send + 'static,
{
    let func = lookup_func(store, instance, iface, export)?;
    let mut results = vec![Val::Bool(false); 1];
    func.call_async(&mut *store, &[input], &mut results)
        .await
        .map_err(|e| format!("`${export}`: call trapped: {e:#}"))?;
    let any = match results.into_iter().next() {
        Some(Val::Future(a)) => a,
        other => {
            return Err(format!(
                "`${export}`: expected a future result, got {other:?}"
            ));
        }
    };
    let reader = any.try_into_future_reader::<T>().map_err(|e| {
        format!(
            "`${export}`: result is not future<{}>: {e:#}",
            std::any::type_name::<T>()
        )
    })?;
    let (tx, rx) = oneshot::channel::<T>();
    reader
        .pipe(&mut *store, OneshotConsumer::new(tx))
        .map_err(|e| format!("`${export}`: pipe failed: {e:#}"))?;
    let got = store
        .run_concurrent(async move |_| rx.await)
        .await
        .map_err(|e| format!("`${export}`: run_concurrent failed: {e:#}"))?
        .map_err(|_| format!("`${export}`: future closed without a value"))?;
    if got != expected {
        return Err(format!(
            "`${export}`: produced payload mismatch\n  in:  {expected:?}\n  out: {got:?}"
        ));
    }
    Ok(())
}

fn run_producer_round_trips(opt_level: OptLevel) {
    let wasm = compile_lib_source(PRODUCER_SOURCE, opt_level);
    let engine = common::engine();
    let rt = common::runtime();
    let opt = common::opt_level_name(opt_level);

    rt.block_on(async {
        let component = Component::new(engine, &wasm).expect("instantiate component type");
        let linker = common::linker(engine).expect("build linker");
        let state = common::WasiState::new_with_pipes(
            wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(65536),
            wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(65536),
        );
        let mut store = Store::new(engine, state);
        store.set_epoch_deadline((common::DEFAULT_TIMEOUT_MS / 1000).max(1));
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate producer component");
        let iface = instance
            .get_export(&mut store, None, LIB_WORLD_FQ)
            .map(|(_, idx)| idx);
        let i = iface.as_ref();

        let mut failures = Vec::new();
        macro_rules! check {
            ($call:expr) => {
                if let Err(e) = $call.await {
                    failures.push(format!("[{opt}] {e}"));
                }
            };
        }

        check!(produce_and_read_back(
            &mut store,
            &instance,
            i,
            "mk-future-string",
            Val::String("héllo, wörld".into()),
            "héllo, wörld".to_string(),
        ));
        check!(produce_and_read_back(
            &mut store,
            &instance,
            i,
            "mk-future-option",
            Val::Option(b(Val::U32(42))),
            Some(42u32),
        ));
        check!(produce_and_read_back(
            &mut store,
            &instance,
            i,
            "mk-future-result",
            Val::Result(Ok(b(Val::U32(7)))),
            Ok::<u32, String>(7),
        ));
        check!(produce_and_read_back(
            &mut store,
            &instance,
            i,
            "mk-future-list",
            Val::List(vec![Val::U32(1), Val::U32(2), Val::U32(3)]),
            vec![1u32, 2, 3],
        ));
        check!(produce_and_read_back(
            &mut store,
            &instance,
            i,
            "mk-future-tuple",
            Val::Tuple(vec![Val::U32(5), Val::String("x".into())]),
            (5u32, "x".to_string()),
        ));

        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    });
}

#[test]
fn cm_future_aggregate_producer_o0() {
    run_producer_round_trips(OptLevel::O0);
}

#[test]
fn cm_future_aggregate_producer_o2() {
    run_producer_round_trips(OptLevel::O2);
}

/// An async `--lib` stream producer: write the elements into a fresh
/// `stream<T>` and deliver the readable end via `task return`. Exercises the
/// general stream `new` / `write` lowering for scalar element types (the host
/// reads the produced elements back). `async` because the write blocks until
/// the reader (the host) consumes.
const STREAM_PRODUCER_SOURCE: &str = r#"
export async fn mk_stream_u32(data: List<u32>) -> Stream<u32> {
    let [rx, tx] = Stream::<u32>::new();
    task return rx;
    tx.write(data);
    tx.drop();
}
"#;

async fn produce_stream_and_read_back<T>(
    store: &mut Store<common::WasiState>,
    instance: &Instance,
    iface: Option<&ComponentExportIndex>,
    export: &'static str,
    input: Val,
    expected: Vec<T>,
) -> Result<(), String>
where
    T: wasmtime::component::Lift + PartialEq + std::fmt::Debug + Send + Sync + Unpin + 'static,
{
    let func = lookup_func(store, instance, iface, export)?;
    let mut results = vec![Val::Bool(false); 1];
    func.call_async(&mut *store, &[input], &mut results)
        .await
        .map_err(|e| format!("`${export}`: call trapped: {e:#}"))?;
    let any = match results.into_iter().next() {
        Some(Val::Stream(a)) => a,
        other => {
            return Err(format!(
                "`${export}`: expected a stream result, got {other:?}"
            ));
        }
    };
    let reader = any.try_into_stream_reader::<T>().map_err(|e| {
        format!(
            "`${export}`: result is not stream<{}>: {e:#}",
            std::any::type_name::<T>()
        )
    })?;
    let (tx, mut rx) = mpsc::unbounded::<T>();
    reader
        .pipe(&mut *store, StreamCollectConsumer::new(tx))
        .map_err(|e| format!("`${export}`: pipe failed: {e:#}"))?;
    let got = store
        .run_concurrent(async move |_| {
            let mut items = Vec::new();
            while let Some(item) = rx.next().await {
                items.push(item);
            }
            items
        })
        .await
        .map_err(|e| format!("`${export}`: run_concurrent failed: {e:#}"))?;
    if got != expected {
        return Err(format!(
            "`${export}`: produced stream mismatch\n  in:  {expected:?}\n  out: {got:?}"
        ));
    }
    Ok(())
}

fn run_stream_producer_round_trips(opt_level: OptLevel) {
    let wasm = compile_lib_source(STREAM_PRODUCER_SOURCE, opt_level);
    let engine = common::engine();
    let rt = common::runtime();
    let opt = common::opt_level_name(opt_level);

    rt.block_on(async {
        let component = Component::new(engine, &wasm).expect("instantiate component type");
        let linker = common::linker(engine).expect("build linker");
        let state = common::WasiState::new_with_pipes(
            wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(65536),
            wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(65536),
        );
        let mut store = Store::new(engine, state);
        store.set_epoch_deadline((common::DEFAULT_TIMEOUT_MS / 1000).max(1));
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate stream producer");
        let iface = instance
            .get_export(&mut store, None, LIB_WORLD_FQ)
            .map(|(_, idx)| idx);
        if let Err(e) = produce_stream_and_read_back(
            &mut store,
            &instance,
            iface.as_ref(),
            "mk-stream-u32",
            Val::List(vec![Val::U32(1), Val::U32(2), Val::U32(3), Val::U32(4)]),
            vec![1u32, 2, 3, 4],
        )
        .await
        {
            panic!("[{opt}] {e}");
        }
    });
}

#[test]
fn cm_stream_scalar_producer_o0() {
    run_stream_producer_round_trips(OptLevel::O0);
}

#[test]
fn cm_stream_scalar_producer_o2() {
    run_stream_producer_round_trips(OptLevel::O2);
}

/// A single-export `--lib` async identity over `future<T>`: read the input
/// future's payload, write it into a fresh future, and deliver that future via
/// `task return`. The full aggregate consume/produce round-trip — the guest
/// both lifts (read) and lowers (write) the payload across the boundary. It is
/// `async` because an aggregate future read blocks (unlike a scalar, via the CM
/// number-type guard).
fn future_identity_source(ty: &str, name: &str) -> String {
    format!(
        "export async fn {name}(v: Future<{ty}>) -> Future<{ty}> {{\n\
         \x20   let value = v.read();\n\
         \x20   v.drop();\n\
         \x20   let [rx, tx] = Future::<{ty}>::new();\n\
         \x20   task return rx;\n\
         \x20   if let Some(x) = value {{\n\
         \x20       tx.write(x);\n\
         \x20   }}\n\
         }}\n"
    )
}

fn run_future_identity<T>(opt_level: OptLevel, ty: &str, export: &'static str, payload: T)
where
    T: wasmtime::component::Lower
        + wasmtime::component::Lift
        + Clone
        + PartialEq
        + std::fmt::Debug
        + Send
        + Sync
        + 'static,
{
    let fn_name = export.replace('-', "_");
    let wasm = compile_lib_source(&future_identity_source(ty, &fn_name), opt_level);
    let engine = common::engine();
    let rt = common::runtime();
    let opt = common::opt_level_name(opt_level);

    rt.block_on(async {
        let component = Component::new(engine, &wasm).expect("instantiate component type");
        let linker = common::linker(engine).expect("build linker");
        let state = common::WasiState::new_with_pipes(
            wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(65536),
            wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(65536),
        );
        let mut store = Store::new(engine, state);
        store.set_epoch_deadline((common::DEFAULT_TIMEOUT_MS / 1000).max(1));
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate identity component");
        let iface = instance
            .get_export(&mut store, None, LIB_WORLD_FQ)
            .map(|(_, idx)| idx);
        if let Err(e) =
            future_round_trip(&mut store, &instance, iface.as_ref(), export, payload).await
        {
            panic!("[{opt}] {e}");
        }
    });
}

fn run_future_identity_round_trips(opt_level: OptLevel) {
    run_future_identity(
        opt_level,
        "String",
        "id-future-string",
        "héllo, wörld".to_string(),
    );
    run_future_identity(opt_level, "Option<u32>", "id-future-option", Some(42u32));
    run_future_identity(
        opt_level,
        "Result<u32, String>",
        "id-future-result",
        Ok::<u32, String>(7),
    );
    run_future_identity(opt_level, "List<u32>", "id-future-list", vec![1u32, 2, 3]);
    run_future_identity(
        opt_level,
        "[u32, String]",
        "id-future-tuple",
        (5u32, "x".to_string()),
    );
}

#[test]
fn cm_future_aggregate_identity_o0() {
    run_future_identity_round_trips(OptLevel::O0);
}

#[test]
fn cm_future_aggregate_identity_o2() {
    run_future_identity_round_trips(OptLevel::O2);
}

/// Host mirror of the catalog `Point` record, for the `future<record>` /
/// `stream<record>` round-trips. Field order and CM names must match the Wado
/// `struct Point { x: f64, y: f64 }`.
#[derive(
    wasmtime::component::ComponentType,
    wasmtime::component::Lower,
    wasmtime::component::Lift,
    Clone,
    PartialEq,
    Debug,
)]
#[component(record)]
struct Point {
    x: f64,
    y: f64,
}

const RECORD_FUTURE_SOURCE: &str = r#"
struct Point {
    x: f64,
    y: f64,
}
export async fn id_future_point(v: Future<Point>) -> Future<Point> {
    let value = v.read();
    v.drop();
    let [rx, tx] = Future::<Point>::new();
    task return rx;
    if let Some(x) = value {
        tx.write(x);
    }
}
"#;

fn run_record_future_identity(opt_level: OptLevel) {
    let wasm = compile_lib_source(RECORD_FUTURE_SOURCE, opt_level);
    let engine = common::engine();
    let rt = common::runtime();
    let opt = common::opt_level_name(opt_level);

    rt.block_on(async {
        let component = Component::new(engine, &wasm).expect("instantiate component type");
        let linker = common::linker(engine).expect("build linker");
        let state = common::WasiState::new_with_pipes(
            wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(65536),
            wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(65536),
        );
        let mut store = Store::new(engine, state);
        store.set_epoch_deadline((common::DEFAULT_TIMEOUT_MS / 1000).max(1));
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate record future component");
        let iface = instance
            .get_export(&mut store, None, LIB_WORLD_FQ)
            .map(|(_, idx)| idx);
        if let Err(e) = future_round_trip(
            &mut store,
            &instance,
            iface.as_ref(),
            "id-future-point",
            Point { x: 1.5, y: -2.5 },
        )
        .await
        {
            panic!("[{opt}] {e}");
        }
    });
}

#[test]
fn cm_future_record_identity_o0() {
    run_record_future_identity(OptLevel::O0);
}

#[test]
fn cm_future_record_identity_o2() {
    run_record_future_identity(OptLevel::O2);
}

const RECORD_STREAM_SOURCE: &str = r#"
struct Point {
    x: f64,
    y: f64,
}
export async fn id_stream_point(v: Stream<Point>) -> Stream<Point> {
    let [rx, tx] = Stream::<Point>::new();
    task return rx;
    loop {
        let chunk = v.read(16);
        if chunk.len() == 0 {
            break;
        }
        tx.write(chunk);
    }
    v.drop();
    tx.drop();
}
"#;

fn run_record_stream_identity(opt_level: OptLevel) {
    let wasm = compile_lib_source(RECORD_STREAM_SOURCE, opt_level);
    let engine = common::engine();
    let rt = common::runtime();
    let opt = common::opt_level_name(opt_level);

    rt.block_on(async {
        let component = Component::new(engine, &wasm).expect("instantiate component type");
        let linker = common::linker(engine).expect("build linker");
        let state = common::WasiState::new_with_pipes(
            wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(65536),
            wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(65536),
        );
        let mut store = Store::new(engine, state);
        store.set_epoch_deadline((common::DEFAULT_TIMEOUT_MS / 1000).max(1));
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate record stream component");
        let iface = instance
            .get_export(&mut store, None, LIB_WORLD_FQ)
            .map(|(_, idx)| idx);
        if let Err(e) = stream_round_trip(
            &mut store,
            &instance,
            iface.as_ref(),
            "id-stream-point",
            vec![Point { x: 1.0, y: 2.0 }, Point { x: -3.5, y: 4.5 }],
        )
        .await
        {
            panic!("[{opt}] {e}");
        }
    });
}

#[test]
fn cm_stream_record_identity_o0() {
    run_record_stream_identity(OptLevel::O0);
}

#[test]
fn cm_stream_record_identity_o2() {
    run_record_stream_identity(OptLevel::O2);
}

/// A single-export `--lib` async identity over `stream<T>`: read the input
/// stream element-by-element, write each chunk into a fresh `stream<T>`, and
/// deliver the readable end via `task return`. Exercises the general stream
/// `read` (lift) and `write` (lower) lowering for scalar and aggregate element
/// payloads. `async` because both halves block until the peer makes progress.
fn stream_identity_source(ty: &str, name: &str) -> String {
    format!(
        "export async fn {name}(v: Stream<{ty}>) -> Stream<{ty}> {{\n\
         \x20   let [rx, tx] = Stream::<{ty}>::new();\n\
         \x20   task return rx;\n\
         \x20   loop {{\n\
         \x20       let chunk = v.read(16);\n\
         \x20       if chunk.len() == 0 {{\n\
         \x20           break;\n\
         \x20       }}\n\
         \x20       tx.write(chunk);\n\
         \x20   }}\n\
         \x20   v.drop();\n\
         \x20   tx.drop();\n\
         }}\n"
    )
}

fn run_stream_identity<T>(opt_level: OptLevel, ty: &str, export: &'static str, payload: Vec<T>)
where
    T: wasmtime::component::Lower
        + wasmtime::component::Lift
        + Clone
        + PartialEq
        + std::fmt::Debug
        + Send
        + Sync
        + Unpin
        + 'static,
{
    let fn_name = export.replace('-', "_");
    let wasm = compile_lib_source(&stream_identity_source(ty, &fn_name), opt_level);
    let engine = common::engine();
    let rt = common::runtime();
    let opt = common::opt_level_name(opt_level);

    rt.block_on(async {
        let component = Component::new(engine, &wasm).expect("instantiate component type");
        let linker = common::linker(engine).expect("build linker");
        let state = common::WasiState::new_with_pipes(
            wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(65536),
            wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(65536),
        );
        let mut store = Store::new(engine, state);
        store.set_epoch_deadline((common::DEFAULT_TIMEOUT_MS / 1000).max(1));
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate identity component");
        let iface = instance
            .get_export(&mut store, None, LIB_WORLD_FQ)
            .map(|(_, idx)| idx);
        if let Err(e) =
            stream_round_trip(&mut store, &instance, iface.as_ref(), export, payload).await
        {
            panic!("[{opt}] {e}");
        }
    });
}

fn run_stream_identity_round_trips(opt_level: OptLevel) {
    run_stream_identity(opt_level, "u32", "id-stream-u32", vec![1u32, 2, 3, 4]);
    run_stream_identity(
        opt_level,
        "String",
        "id-stream-string",
        vec!["a".to_string(), "bb".to_string(), "céç".to_string()],
    );
    run_stream_identity(
        opt_level,
        "Option<u32>",
        "id-stream-option",
        vec![Some(1u32), None, Some(3u32)],
    );
    run_stream_identity(
        opt_level,
        "List<u32>",
        "id-stream-list",
        vec![vec![1u32, 2], vec![], vec![3u32]],
    );
    run_stream_identity(
        opt_level,
        "[u32, String]",
        "id-stream-tuple",
        vec![(1u32, "a".to_string()), (2u32, "bb".to_string())],
    );
}

#[test]
fn cm_stream_aggregate_identity_o0() {
    run_stream_identity_round_trips(OptLevel::O0);
}

#[test]
fn cm_stream_aggregate_identity_o2() {
    run_stream_identity_round_trips(OptLevel::O2);
}

/// Compile an inline library source at O0, returning the compiler's result.
fn try_compile_lib(source: &str) -> Result<(), String> {
    let options = CompilerOptions {
        opt_level: OptLevel::O0,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        ..Default::default()
    };
    common::compile_source_with_compiler_options(Path::new("lib.wado"), source, options)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// A library export whose signature carries a type with no Component Model value
/// representation must be rejected with a readable compile error, not an ICE or
/// silently-wrong code. An empty record is the canonical case.
#[test]
fn cm_lib_rejects_empty_record_boundary_type() {
    let err = try_compile_lib(
        "pub struct Empty {}\nexport fn id_empty(v: Empty) -> Empty {\n    return v;\n}\n",
    )
    .expect_err("empty-record export should fail to compile");
    assert!(
        err.contains("id_empty") && err.contains("empty record"),
        "expected an empty-record boundary diagnostic, got: {err}"
    );
}

/// A Component Model interface has one namespace shared by its types and its
/// functions (`WIT.md`). Wado keeps the two apart, so a `PascalCase` type and a
/// `snake_case` function look unambiguous until kebab-casing maps them onto one
/// CM name. That must be a diagnostic naming both, not an ICE from Wasm
/// validation.
#[test]
fn cm_lib_rejects_export_name_colliding_with_type_name() {
    let err = try_compile_lib(
        "variant Shape {\n    Dot,\n    Line(u32),\n}\n\
         export fn shape(v: u32) -> u32 {\n    return v;\n}\n\
         export fn make(v: u32) -> Shape {\n    if v == 0 {\n        \
         return Shape::Dot;\n    } else {\n        return Shape::Line(v);\n    }\n}\n",
    )
    .expect_err("a function and a type sharing a CM name should fail to compile");
    assert!(
        err.contains("`shape`") && err.contains("`Shape`"),
        "expected a collision diagnostic naming both the function and the type, got: {err}"
    );
}

/// The interface name check walks every export signature through the CM type
/// engine, which recurses without a depth guard. It has to run behind the
/// boundary-representability check, or a recursive type overflows the stack
/// instead of getting the diagnostic that already exists for it.
#[test]
fn cm_lib_rejects_recursive_type_before_the_name_check_walks_it() {
    let err = try_compile_lib(
        "pub struct Node {\n    next: Option<Node>,\n    value: u32,\n}\n\
         export fn depth(n: Node) -> u32 {\n    return n.value;\n}\n",
    )
    .expect_err("a recursive boundary type should fail to compile");
    assert!(
        err.contains("recursive") && err.contains("Node"),
        "expected the recursive-type boundary diagnostic, got: {err}"
    );
}

/// Two type names that differ only in a way kebab-casing erases land on the same
/// CM name with no function involved at all.
#[test]
fn cm_lib_rejects_two_types_sharing_a_cm_name() {
    let err = try_compile_lib(
        "struct HTTPServer {\n    port: u32,\n}\n\
         struct HttpServer {\n    host: String,\n}\n\
         export fn a(v: HTTPServer) -> u32 {\n    return v.port;\n}\n\
         export fn b(v: HttpServer) -> String {\n    return v.host;\n}\n",
    )
    .expect_err("two types sharing a CM name should fail to compile");
    assert!(
        err.contains("http-server"),
        "expected a diagnostic naming the shared CM name, got: {err}"
    );
}

/// The fixture is the published package source reused verbatim, so it must stay
/// byte-identical to `package-cm-catalog/src/lib.wado`; otherwise the test
/// corpus and the shipped package could drift apart.
#[test]
fn cm_catalog_fixture_matches_package_source() {
    const PACKAGE_SOURCE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../package-cm-catalog/src/lib.wado"
    );
    let fixture = std::fs::read_to_string(FIXTURE).expect("read cm_catalog fixture");
    let package = std::fs::read_to_string(PACKAGE_SOURCE).expect("read package source");
    assert_eq!(
        fixture, package,
        "tests/fixtures/cm_catalog.wado must be byte-identical to \
         package-cm-catalog/src/lib.wado"
    );
}

#[test]
fn cm_catalog_round_trip_o0() {
    run_round_trips(OptLevel::O0);
}

#[test]
fn cm_catalog_round_trip_o2() {
    run_round_trips(OptLevel::O2);
}

/// Round-trip every value case twice under `freelist`, whose free path traps on
/// a block that is already free.
///
/// The guard for the `post-return` free walk (wado-lang/wado#1683): a buffer the
/// walk visits twice is a double-free, and calling twice also catches a buffer
/// freed while the host still needs it, since `freelist` hands the block straight
/// back on the next call. The catalog is the widest shape corpus available.
fn run_double_free_guard(opt_level: OptLevel) {
    let wasm = compile_catalog_with_allocator(opt_level, "freelist");
    let engine = common::engine();
    let opt = common::opt_level_name(opt_level);

    common::runtime().block_on(async {
        let component = Component::new(engine, &wasm).expect("instantiate component type");
        let linker = common::linker(engine).expect("build linker");
        let state = common::WasiState::new_with_pipes(
            wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(65536),
            wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(65536),
        );
        let mut store = Store::new(engine, state);
        store.set_epoch_deadline((common::DEFAULT_TIMEOUT_MS / 1000).max(1));
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate library component");
        let iface = instance
            .get_export(&mut store, None, LIB_WORLD_FQ)
            .map(|(_, idx)| idx);

        let mut failures = Vec::new();
        for Case { export, value } in cases() {
            let func = iface
                .as_ref()
                .and_then(|i| instance.get_export(&mut store, Some(i), export))
                .map(|(_, idx)| idx)
                .and_then(|idx| instance.get_func(&mut store, idx));
            let Some(func) = func else {
                failures.push(format!("[{opt}] export `{export}` not found"));
                continue;
            };
            for call in 0..2 {
                let mut results = vec![Val::Bool(false); func.ty(&store).results().len()];
                match func
                    .call_async(&mut store, std::slice::from_ref(&value), &mut results)
                    .await
                {
                    Ok(()) if results.first() == Some(&value) => {}
                    Ok(()) => failures.push(format!(
                        "[{opt}] `{export}` call {call}: round-trip mismatch\n  \
                         in:  {value:?}\n  out: {:?}",
                        results.first()
                    )),
                    Err(e) => failures.push(format!(
                        "[{opt}] `{export}` call {call} trapped: {e:#}\n  \
                         A `freelist` trap here is a double-free or a \
                         premature free in the `post-return` walk."
                    )),
                }
            }
        }

        assert!(
            failures.is_empty(),
            "{} of {} catalog exports failed under `freelist`:\n{}",
            failures.len(),
            cases().len(),
            failures.join("\n")
        );
    });
}

#[test]
fn cm_catalog_no_double_free_o0() {
    run_double_free_guard(OptLevel::O0);
}

#[test]
fn cm_catalog_no_double_free_o2() {
    run_double_free_guard(OptLevel::O2);
}
