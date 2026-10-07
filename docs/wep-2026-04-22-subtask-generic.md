# WEP: Generic `AsyncCall<T>` for CM async imports

## Context

WASI P3 defines `async func` imports (e.g. `wasi:http/client#send`) that are
lowered via `canon lower async`. The canonical ABI for this form:

1. Allocates a result buffer in caller memory (outptr).
2. Calls the lowered import with the params and outptr.
3. The import returns immediately with `(subtask_handle << 4) | status` —
   `Status::Returned` means the host completed synchronously, `Status::Started`
   means the host spawned a subtask that will write the result later.
4. The caller later calls `waitable-set.wait` on the subtask to wait for
   completion and then lifts the result from outptr.

Wado's current CM binding synthesis (`wado-compiler/src/synthesis/cm_binding.rs`,
`needs_async_lower` branch around line 3205) combines all four steps into a
single adapter function that blocks the caller until the subtask returns.

### Problem

For `async fn` imports whose request contains a stream parameter (e.g.
`Client::send` with a `Request` carrying a body `Stream<u8>`), the eager wait
creates a deadlock:

- User writes `body_tx.write(body)` after `Client::send(req)` so the reader end
  (`body_rx` embedded in the request) has a subtask consuming it.
- But `Client::send(req)` never returns, because the synthesized adapter
  suspends the guest fiber in `wait_for_subtask` immediately after starting the
  host subtask.
- The host subtask, in turn, cannot make progress because it is reading
  `body_rx` which has no writer ready (the writer `body_tx.write` call would
  have run after `Client::send` returned).
- Both sides are suspended. `Trap::AsyncDeadlock`.

The CM spec supports interleaving via the `Status::Started` fast path — the
caller receives the subtask handle synchronously and can perform concurrent
operations before awaiting the subtask. The current Wado synthesis hides this
behind the eager wait, so user code never sees the handle.

### Reference: wasmtime's Rust test

`vendor/wasmtime/crates/test-programs/src/bin/p3_http_outbound_request_content_length.rs`
shows the canonical pattern with `futures::join!`:

```rust
let (handle, transmit, ()) = join!(
    async { client::send(request).await },         // async send
    async { transmit.await },                      // transmit future
    async {
        contents_tx.write_all(body).await;          // body writer
        trailers_tx.write(Ok(None)).await;
        drop(contents_tx);
    },
);
```

Three concurrent futures drive each other to completion. In CM terms: the send
subtask, the transmit future, and the body stream rendezvous progress on a
single task via `waitable-set.wait`.

## Decision

Expose the CM `subtask` primitive to Wado user code as a type, `AsyncCall<T>`,
and change the synthesized signature of CM `async func` imports to return
`AsyncCall<T>` instead of `T` directly. The user explicitly `.wait()`s when
they want the result.

> Naming note: this WEP was originally titled `Subtask<T>`. The type shipped
> as `AsyncCall<T>` — "subtask" is the CM primitive it wraps, but the Wado type
> is the whole in-flight call (handle plus result buffer plus lift function),
> so `AsyncCall` reads better at call sites. The non-generic `Subtask` resource
> (a thin wrapper over the raw CM subtask handle) was retained, not replaced;
> see "Breaking changes". This document uses `AsyncCall<T>` throughout.

### Wado type

```wado
// core/prelude/types.wado
pub struct AsyncCall<T> {
    __cm_packed: i32,          // the CM subtask handle and its status
    __cm_outptr: i32,          // the linear-memory result buffer
    __cm_size: i32,            // its size and alignment, for `realloc`-based free
    __cm_align: i32,
    __cm_lift: fn(i32) -> T,   // reads `T` out of the buffer
    __cm_ended: &mut bool,     // set by the first `wait` or `cancel`
}

impl<T> AsyncCall<T> {
    pub fn wait(&self) -> T with (Subtask, WaitableSet);
    pub fn cancel(&self) with Subtask;
    pub fn join(&self, set: &WaitableSet) -> Waitable with Subtask;
}
```

`AsyncCall<T>` is a `struct`, not a `resource`. It is not a CM primitive but
the guest's own bookkeeping around one: the subtask handle, the result buffer
the subtask writes into, and the lift that reads `T` back out. That is what the
CM `subtask` lacks, and a resource stands for a CM handle. Size and alignment
are baked in at monomorphization time based on `T`.

`wait` and `cancel` end the call: the subtask handle is dropped and the buffer
freed. A copy of the struct shares the handle and the buffer, so it shares the
end too. `__cm_ended` is a reference for that reason, and a `wait`, `cancel` or
`join` on an ended call traps rather than lift a freed buffer. A call never
ended leaks its handle and buffer: ending it on drop would need to know that no
copy is left, which a struct does not track.

### Synthesis of async imports

For each WIT `async func` import, the compiler synthesizes a Wado function
whose return type is `AsyncCall<T>` where `T` is the CM return type lifted to
Wado. The body:

1. Allocates outptr via `realloc` with size/align computed from `T`.
2. Calls the import via `canon lower async`, receiving the packed subtask
   handle / status.
3. Wraps the packed handle, the result buffer pointer, its size and
   alignment, and a per-`T` lift function in an `AsyncCall<T>` value and
   returns it.

The eager wait + lift + free pipeline lives in `AsyncCall<T>::wait`,
monomorphized per `T`:

1. If the packed handle is zero (`Status::Returned` synchronously), skip the
   wait.
2. Otherwise, create a `WaitableSet`, join the subtask, loop on `wait()` until
   `Status::Returned`, drop both.
3. Lift `T` from outptr using the per-import lift function.
4. `realloc(outptr, size, align, 0)` to free.

### `wado-from-idl` automation

WIT `async func foo(...) -> T` ⇒ Wado `async fn foo(...) -> AsyncCall<T>`.
`wado-from-idl` tracks `is_async` in its IR and emits both the `async` marker
and the `AsyncCall<ReturnType>` result.

World exports (entry points like `run`, `handle`) are `async fn` too, returning
`T` itself: they are the CM lifting boundary, not an import adapter.

### User code patterns

GET or body-less request (no stream parameter):

```wado
let resp = Client::send(req).wait();
```

POST with body stream (mirror of `example/http_bin.wado`'s `task return`
pattern, but for the outbound direction):

```wado
let [body_rx, body_tx] = Stream::<u8>::new();
let [trailers_future, trailers_tx] = Future::<...>::new();
let [req, _transmit] = Request::new(headers, Option::Some(body_rx), trailers_future, null);
req.set_method(Method::Post);
// ... configure req ...

let task = Client::send(req);    // canon lower async, host subtask starts
body_tx.write(body);             // rendezvous with subtask reading body_rx
body_tx.drop();
let resp = task.wait();          // wait + lift + free; ends `task`
trailers_tx.write(Result::Ok(null));
```

## Consequences

### Breaking changes

- Every caller of a WIT `async func` import sees a signature change. Existing
  callers must add `.wait()` to retrieve the result. In practice this is only
  `Client::send`; no other async imports are currently used from Wado.
- The non-generic `Subtask` resource is _retained_ as a compiler-internal
  type, not replaced. `AsyncCall<T>` reaches the CM canonical operations
  (`subtask-drop`, `subtask-cancel`, `waitable-join`) by `as`-casting its
  packed handle to `Subtask` inside the stdlib. Those `as` casts between a raw
  `i32` and a resource are an internal escape hatch that bypasses ownership
  entirely; WEP 2026-05-21 tracks restricting them to `internal`-only code.

## Implementation status

- [x] `AsyncCall<T>` added to `prelude/types.wado`; non-generic `Subtask`
  retained as an internal resource.
- [x] Compiler representation: `AsyncCall<T>` is a struct-like type with the
  hidden `i32` fields plus the lift `fn` pointer; type elaborator,
  monomorphization, and WIR lowering construct and read it. Hardcoded
  under the name `"AsyncCall"` in `cm_binding/import_adapter.rs`,
  `cm_binding/types.rs`, `elaborator/call.rs`, and `component_model.rs`.
- [x] `AsyncCall<T>::wait` / `cancel` / `join` written in Wado in
  `prelude/types.wado`, reusing `wait_for_subtask`-style logic.
- [x] `needs_async_lower` branch in `cm_binding.rs` returns `AsyncCall<T>`
  without waiting.
- [x] `wado-from-idl` emits `AsyncCall<…>` for `is_async` functions.
- [x] Existing `Client::send` fixtures migrated to add `.wait()`.
- [x] A `wait`, `cancel` or `join` on an ended call, or on a copy of one,
  traps through the shared `__cm_ended` flag.
- [ ] Restrict `as` casts between `i32` and resources to `internal`-only
  code so user code cannot forge or alias resource handles. Tracked by
  WEP 2026-05-21.

## Known gaps

- No way to start a task. A component gets a new task only when its export is
  called. The CM threading built-ins (🧵, `thread.new-indirect` and the rest)
  add threads within a task, but they are gated outside WASI 0.3, and wasmtime
  calls its support for them very incomplete. So both ends of a stream cannot
  live in one component: one end goes to an import's subtask or to the caller.
- No structured concurrency over several calls (`join`, `race`). A caller
  combines them by hand with `WaitableSet::wait`.
- TLS trust store configuration for outgoing HTTPS requests from `wado run`.
  wasmtime-wasi-http 43.0.1's `WasiHttpHooks::send_request` signature uses
  `pub(crate)` types (`HttpResult`, `HttpError`, `body::UnsyncBoxBody`), so
  the host cannot override it without patching upstream.

## References

- [Resource Ownership and a Resource-Scoped Borrow Checker](./wep-2026-05-21-resource-ownership.md)
- [Redesign Wasm CM Builtins as Resource Canonical Attributes](./wep-2026-03-01-cm-resource-canonical-attrs.md)
- [TIR-Level CM Binding Synthesis](./wep-2026-02-15-cm-binding-synthesis.md)
- [WASI HTTP Integration](./wep-2026-02-21-wasi-http.md)
- [Component Model Concurrency](../vendor/component-model/design/mvp/Concurrency.md)
