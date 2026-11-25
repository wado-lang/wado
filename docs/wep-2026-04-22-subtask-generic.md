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
// core/prelude/async.wado
pub affine struct AsyncCall<affine T> {
    subtask: Option<Subtask>,       // None where the call completed synchronously
    buffer: Option<ResultBuffer>,   // the linear-memory result buffer, with its size and alignment
    lift: fn(i32) -> T,             // reads `T` out of the buffer
    args: Option<ArgBuffer>,        // what the arguments were lowered into
    promise: Option<ExternObject>,  // what an `async` web import answered in place of a subtask
}

impl<affine T> AsyncCall<T> {
    pub fn wait(self) -> T with Subtask;
    pub fn cancel(self) with Subtask;
}

impl<affine T> Waitable for AsyncCall<T> { ... }
```

`AsyncCall<T>` is a `struct`, not a `resource`. It is not a CM primitive but
the guest's own bookkeeping around one: the subtask handle, the result buffer
the subtask writes into, and the lift that reads `T` back out. That is what the
CM `subtask` lacks, and a resource stands for a CM handle. The buffer's size
and alignment are baked in at monomorphization time based on `T`.

The struct is `affine`, so a call is one value that moves and is never copied.
`wait` and `cancel` take `self`: each ends the call, dropping the subtask
handle and freeing the buffer, and the compiler refuses a second use. Dropping
a call nobody waited on cancels it, and lifts a result the callee already
returned so the handles it owns drop too. A call waits among others by moving
into a set with `WaitableSet::insert`, and the set's `wait` moves it back out
in a `Ready`.

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

1. If the call holds no subtask (it returned synchronously), skip the wait.
2. Otherwise, create a `WaitableSet<Subtask>`, insert the subtask, `wait()`
   until it resolves, and drop both.
3. Lift `T` from the buffer using the per-import lift function.
4. The buffer drops, which frees it: `realloc(ptr, size, align, 0)`.

The Component Model delivers a subtask's resolution once, through the first
`waitable-set.wait` that reports it. A call a user's set handed back after
that would wait for an event that never comes, and `subtask.cancel` traps on
it. So a set records each resolution it delivers by the subtask's handle, and
the record goes when the subtask drops, before the host reuses the handle. A
call reads it: `wait` skips the wait, a drop skips the cancel, and a set the
call is inserted into again reports it without the host. The record is keyed by handle
rather than kept in the `AsyncCall`, so it holds whatever a user's `Waitable`
wraps the call in.

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
- The non-generic `Subtask` resource is _retained_, not replaced. It is a
  `pub` resource `core:prelude` exports, so user code can name it, as the
  member type of a `WaitableSet<Subtask>` or as the effect `wait` and `cancel`
  perform. Its methods (`subtask-drop`, `subtask-cancel`) are private to the
  prelude: `AsyncCall<T>` holds the subtask and reaches them there.

## Implementation status

- [x] `AsyncCall<T>` added to `prelude/async.wado`; non-generic `Subtask`
  retained as a public resource with private methods.
- [x] Compiler representation: `AsyncCall<T>` is an ordinary prelude struct
  the compiler finds as the compiler item `async_call`, with `result_buffer`
  and `arg_buffer` beside it. The import adapter
  (`synthesis/cm_binding/import_adapter.rs`) builds one with a struct literal.
- [x] `AsyncCall<T>::wait` / `cancel` and its `Waitable` impl written in Wado
  in `prelude/async.wado`, over `WaitableSet<Subtask>`.
- [x] `needs_async_lower` branch in `import_adapter.rs` returns `AsyncCall<T>`
  without waiting.
- [x] `wado-from-idl` emits `AsyncCall<…>` for `is_async` functions.
- [x] Existing `Client::send` fixtures migrated to add `.wait()`.
- [x] A use of an ended call is a compile error: `AsyncCall<T>` is affine, and
  `wait` and `cancel` consume it.
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
- [TIR-Level CM Binding Synthesis](./wep-2026-02-15-cm-binding-synthesis.md)
- [WASI HTTP Integration](./wep-2026-02-21-wasi-http.md)
- [Component Model Concurrency](../vendor/component-model/design/mvp/Concurrency.md)
