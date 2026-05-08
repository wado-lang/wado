# WEP: Generic `Subtask<T>` for CM async imports

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

Expose the CM `subtask` primitive to Wado user code by generalizing the existing
`Subtask` resource to `Subtask<T>` and changing the synthesized signature of
CM `async func` imports to return `Subtask<T>` instead of `T` directly. The
user explicitly `.wait()`s when they want the result.

### Wado type

```wado
// core/prelude/types.wado
/// A handle to an in-flight async CM import call. Internally represents both
/// the CM subtask handle and the result buffer; `wait()` blocks until the
/// subtask returns, then lifts the result and frees the buffer.
pub resource Subtask<T> {
    /// Wait for the subtask to return and take its value. Consumes self
    /// (drops the subtask handle and frees the result buffer).
    fn wait(self) -> T;

    /// Cancel the in-flight subtask. Consumes self.
    fn cancel(self);

    /// Join this subtask to a waitable set for manual polling (used when the
    /// caller wants to wait on multiple subtasks or streams simultaneously).
    fn join(&self, set: &WaitableSet) -> Waitable;
}
```

`Subtask<T>` is a Wado-level struct (not a direct CM handle resource) wrapping
`(subtask_handle: i32, outptr: i32)`. Size and alignment of the result buffer
are baked in at monomorphization time based on `T`.

### Synthesis of async imports

For each WIT `async func` import, the compiler synthesizes a Wado function
whose return type is `Subtask<T>` where `T` is the CM return type lifted to
Wado. The body:

1. Allocates outptr via `realloc` with size/align computed from `T`.
2. Calls the import via `canon lower async`, receiving the packed subtask
   handle / status.
3. Wraps `(subtask_handle, outptr)` in a `Subtask<T>` GC struct and returns
   it.

The existing eager wait + lift + free pipeline moves into the synthesized
`Subtask<T>::wait` method, monomorphized per `T`:

1. If the packed handle is zero (Status::Returned synchronously), skip the
   wait.
2. Otherwise, create a `WaitableSet`, join the subtask, loop on `wait()` until
   `Status::Returned`, drop both.
3. Lift `T` from outptr using `synthesize_lift_with_context`.
4. `realloc(outptr, size, align, 0)` to free.

### `wado-from-idl` automation

WIT `async func foo(...) -> T` ⇒ Wado `fn foo(...) -> Subtask<T>` (no `async`
keyword, matching the spec's rule that "effect declarations never use the
`async` keyword"). `wado-from-idl` already tracks `is_async` in its IR
(`transform.rs:282`); the change is in `codegen.rs:248` to emit
`Subtask<ReturnType>` instead of adding the `async` keyword.

World exports (entry points like `run`, `handle`) continue to use `async fn`
since they represent the CM lifting boundary, not a CM import adapter.

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

let subtask = Client::send(req);    // canon lower async, host subtask starts
body_tx.write(body);                 // rendezvous with subtask reading body_rx
body_tx.drop();
let resp = subtask.wait();           // wait + lift + free
trailers_tx.write(Result::Ok(null));
```

## Consequences

### Breaking changes

- The existing non-generic `Subtask` resource is replaced by the new
  `Subtask<T>`. Call sites (`internal::wait_for_subtask`,
  `tests/fixtures/cm_subtask_join.wado`) migrate. The CM canonical operations
  `subtask-drop`, `subtask-cancel`, `waitable-join` remain internally
  accessible to the compiler but no longer directly exposed as methods on a
  user-facing resource.
- Every caller of a WIT `async func` import sees a signature change. Existing
  callers must add `.wait()` to retrieve the result. In practice this is only
  `Client::send`; no other async imports are currently used from Wado.

### Out of scope

- Generic structured concurrency primitives (`join`, `race`). Spec §
  Concurrency Model shows `join` syntax that is not yet implemented. Once
  `Subtask<T>` is in place, `join` can be built as a stdlib library that
  combines multiple `Subtask<T>` values via `WaitableSet::wait`.
- TLS trust store configuration for outgoing HTTPS requests from `wado run`.
  wasmtime-wasi-http 43.0.1's `WasiHttpHooks::send_request` signature uses
  `pub(crate)` types (`HttpResult`, `HttpError`, `body::UnsyncBoxBody`),
  making external override impossible without patching upstream or using a
  local vendor fork. Tracked separately.

## Implementation plan

1. **Add `Subtask<T>` type to `prelude/types.wado`**. Keep the existing
   non-generic `Subtask` temporarily as an internal type for migration.
2. **Teach the compiler about the new representation**. `Subtask<T>` is a
   struct-like type with two hidden `i32` fields. Type resolver, monomorphization,
   and WIR lowering need to know how to construct and destructure it.
3. **Synthesize `Subtask<T>::wait` and `Subtask<T>::cancel`** per `T`, reusing
   the existing `synthesize_lift_with_context` helper for the lift step and
   `wait_for_subtask` logic for the wait loop.
4. **Rewrite the `needs_async_lower` branch in `cm_binding.rs`**. The adapter
   now returns `Subtask<T>` without waiting. Move the wait/lift/free into the
   synthesized `Subtask<T>::wait` body instead.
5. **Update `wado-from-idl/src/codegen.rs`**. For `func.is_async`, drop the
   `async` keyword and wrap the return type in `Subtask<…>`. World exports
   keep the `async` keyword as today.
6. **Regenerate stdlib**: `mise run update-stdlib-wasi`.
7. **Migrate existing fixtures**. Only `http_client_send_simple.wado`,
   `http_client_advanced.wado`, `http_client_send_body_read.wado` and a few
   others use `Client::send`; add `.wait()` to each call. `cm_subtask_join.wado`
   migrates to the new type.
8. **Add a new fixture** `http_client_send_with_body.wado` that exercises
   spawn → write → wait → done end-to-end with an `outgoing_mocks` endpoint,
   replacing the current TODO comment.
9. **Update `example/llm.wado`** to use the new pattern.
10. **Update `docs/wasi-http-features.md`** to remove the
    `Request.new with contents=Some(Stream<u8>)` TODO.

Step 2 (compiler representation of `Subtask<T>`) is the most substantial; the
rest is plumbing. Estimated at 1–2k lines of compiler changes + fixtures.
