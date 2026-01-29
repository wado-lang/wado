# WASI HTTP Implementation Notes

This document tracks the implementation of `wasi:http/service` world support in Wado.

## Current Status

- HTTP server compiles and runs with `--world wasi:http/service`
- Returns 500 errors correctly with error message payload
- HTTP 200 responses work correctly (as of 2026-01-29)

## What Works

### Error Response (500)

The handler can return `Result::Err(ErrorCode)` successfully:

```wado
export fn handle(request: Request) -> Result<Response, ErrorCode> {
    return Result::<Response, ErrorCode>::Err(
        ErrorCode::InternalError(Option::<String>::Some("error message"))
    );
}
```

This is implemented by calling `task-return` with the flattened error-code representation:

```wat
(call $task-return
  (i32.const 1)   ;; Err discriminant
  (i32.const 38)  ;; InternalError case
  (i32.const 1)   ;; Some payload
  (i64.const 0)   ;; string ptr (data segment)
  (i32.const 37)  ;; string length
  (i32.const 0)   ;; padding
  (i32.const 0)   ;; padding
  (i32.const 0))  ;; padding
```

### Component Structure

The generated component correctly:

- Imports `wasi:http/types@0.3.0-rc-2026-01-06`
- Exports `wasi:http/handler@0.3.0-rc-2026-01-06` with async `handle` function
- Defines the full `error-code` variant type with all 42 cases
- Lowers HTTP type functions (`[constructor]fields`, `[static]response.new`)
- Includes future intrinsics (`future.new`, `future.write`, `future.drop-writable`)

## HTTP 200 Response (Working)

Creating a successful response requires calling `[static]response.new`:

```
response.new(headers, body, trailers) -> [Response, Future<Result<(), ErrorCode>>]
```

The `trailers` parameter is `Future<Result<Option<Fields>, ErrorCode>>`.

### Solution (Implemented 2026-01-29)

The key insight was **post-return async execution**: Component Model allows code to run after `task.return`.

**Correct sequence:**

1. Create headers via `[constructor]fields`
2. Create trailers future via `future.new` → (rx, tx)
3. Call `response.new(headers, body=None, trailers=rx)`
4. Call `task.return(Ok(response))` - returns response to caller
5. Write `Ok(None)` to trailers future via `future.write(tx, payload_ptr)` (post-return execution)
6. Return from function

**Critical type fix:**

The `http-fields` type MUST be `(own $fields)`, NOT `u32`. Even though both are i32 at the core level, the Component Model type checker requires the correct owned handle type:

```wat
;; WRONG: (type $http-fields (;12;) u32)
;; CORRECT:
(type $http-fields (;12;) (own $fields))
```

### Failed Approaches (Historical)

| Approach                                           | Code                                                                         | Result                                         |
| -------------------------------------------------- | ---------------------------------------------------------------------------- | ---------------------------------------------- |
| Null handle                                        | `trailers = 0`                                                               | trap: "channel closed" at response.new         |
| Drop writable end                                  | `future-new` → `future-drop-writable(tx)` → `response.new(rx)`               | trap: "channel closed" at future-drop-writable |
| Write None before response.new                     | `future-new` → `future-write(tx, None)` → `response.new(rx)`                 | Hangs (BLOCKED return code)                    |
| Write None after response.new (before task.return) | `future-new` → `response.new(rx)` → `future-write(tx, None)` → `task.return` | Hangs (BLOCKED return code)                    |
| Leave future pending                               | `future-new` → `response.new(rx)` → return (tx leaks)                        | trap: "channel closed" after handler returns   |
| Wrong type for http-fields                         | `http-fields = u32` instead of `own<fields>`                                 | trap: "channel closed" at response.new         |

The Component Model async semantics require:

- A future to be fulfilled (written) OR explicitly cancelled
- The sender must not be dropped without writing
- Types must match exactly (own<resource> vs u32)

## Implementation Details

### Type Definitions

```
// Trailers future type
http-trailers-result = result<option<fields>, error-code>
http-trailers-future = future<http-trailers-result>

// Transmission future type (returned by response.new)
http-transmission-result = result<(), error-code>
http-transmission-future = future<http-transmission-result>
```

### Handler Result Type

The handler returns `result<response, error-code>` which is flattened to 8 i32/i64 arguments for `task-return`:

```
// Ok case
task-return(0, response_handle, padding...)

// Err case
task-return(1, error_case, has_payload, padding, payload_fields...)
```

### Files Modified

- `wado-compiler/src/codegen.rs` - HTTP response codegen, task-return handling
- `wado-compiler/src/optimize_dce.rs` - Future intrinsics for Service world
- `wado-compiler/lib/wasi/http.wado` - WASI HTTP type definitions

## TODO

### Investigation

- [x] Study wasmtime-wasi-http source code for trailers handling
- [x] Find test cases that show correct usage of `response.new`
- [x] Understand Component Model async boundary semantics
- [x] Check if `future-write` requires a specific caller context

### Implementation

- [x] Implement correct trailers future lifecycle (post-return execution pattern)
- [x] Fix type definition: `http-fields` must be `own<fields>` not `u32`
- [x] Return Ok(response) via task-return
- [ ] Handle transmission future from response.new (currently ignored)
- [ ] Support response body content via stream

### Testing

- [ ] Add E2E tests using wasmtime API (not CLI)
- [x] Test HTTP 200 response with empty body (manual test passed)
- [ ] Test HTTP 200 response with body content

## Investigation Notes (2026-01-29)

### wasmtime-wasi-http Source Analysis

#### Key Files

| File                                                                      | Purpose                                          |
| ------------------------------------------------------------------------- | ------------------------------------------------ |
| `crates/wasi-http/src/p3/host/types.rs:597-637`                           | `Response::new` host implementation              |
| `crates/wasi-http/src/p3/body.rs:404-439`                                 | `GuestTrailerConsumer` - consumes guest trailers |
| `crates/wasi-http/src/p3/body.rs:355-372`                                 | Trailers polling in `GuestBody::poll_frame`      |
| `crates/test-programs/src/bin/p3_http_outbound_request_response_build.rs` | Guest-side response building example             |

#### Guest-Side Response Building Pattern

From `p3_http_outbound_request_response_build.rs:32-43`:

```rust
let headers = Headers::from_list(&[(
    "Content-Type".to_string(),
    "application/text".to_string().into_bytes(),
)])
.unwrap();
let (mut contents_tx, contents_rx) = wit_stream::new();
let (_, trailers_rx) = wit_future::new(|| Ok(None));  // KEY: callback-based future!
let _ = Response::new(headers, Some(contents_rx), trailers_rx);
```

**Key insight:** `wit_future::new(|| Ok(None))` creates a future with a **callback** that returns `Ok(None)`. This is NOT the same as `future.new` + `future.write`.

#### Host-Side Response Creation

From `types.rs:597-637`:

```rust
fn new<T>(
    mut store: Access<T, Self>,
    headers: Resource<Headers>,
    contents: Option<StreamReader<u8>>,
    trailers: FutureReader<Result<Option<Resource<Trailers>>, ErrorCode>>,
) -> wasmtime::Result<(Resource<Response>, FutureReader<Result<(), ErrorCode>>)> {
    // ...
    let body = match contents {
        Some(Ok(mut producer)) => Body::Host { body, result_tx },
        Some(Err(rx)) => Body::Guest {
            contents_rx: Some(rx),
            trailers_rx: trailers,  // trailers passed directly
            result_tx,
        },
        None => Body::Guest {
            contents_rx: None,
            trailers_rx: trailers,  // trailers passed directly
            result_tx,
        },
    };
    // ...
}
```

The host stores `trailers_rx` in the `Body::Guest` struct and polls it when the body stream closes.

### Component Model Async Semantics

#### Return Code Encoding

From `futures_and_streams.rs:54-99`:

| Return Code    | Value             | Meaning                                    |
| -------------- | ----------------- | ------------------------------------------ |
| `Blocked`      | `0xffffffff`      | No reader ready, operation cannot complete |
| `Completed(n)` | `(n << 4) \| 0x0` | Data transferred successfully              |
| `Dropped(n)`   | `(n << 4) \| 0x1` | Other end dropped                          |
| `Cancelled(n)` | `(n << 4) \| 0x2` | Operation cancelled                        |

#### `future.write` Blocking Behavior

From `futures_and_streams.rs:3467-3483`:

```rust
// If read end is Open (no reader ready), set guest as ready and return BLOCKED
ReadState::Open => {
    set_guest_ready(concurrent_state)?;
    ReturnCode::Blocked
}

// For sync-lifted calls, wait synchronously
if result == ReturnCode::Blocked && !self.options(store.0, options).async_ {
    result = self.wait_for_write(store.0, transmit_handle)?;
}
```

**Key findings:**

1. For **async-lifted** exports: returns `BLOCKED` immediately, guest should suspend
2. For **sync-lifted** exports: blocks synchronously in `wait_for_write()`
3. Blocks when `ReadState::Open` (no reader has called `future.read` yet)

#### `future.drop-writable` Behavior

From `futures_and_streams.rs:3034-3059`:

```rust
pub(super) fn guest_drop_writable(...) -> Result<()> {
    match ty {
        TransmitIndex::Stream(_) => store.host_drop_writer(id, None),
        TransmitIndex::Future(_) => store.host_drop_writer(
            id,
            Some(|| {
                Err(format_err!(
                    "cannot drop future write end without first writing a value"
                ))
            }),
        ),
    }
}
```

**Key finding:** For futures, **dropping without writing is an error**. This explains why `future-drop-writable` fails with "channel closed".

#### `task.return` and Pending Futures

From `concurrent.rs:2927-3072`:

1. `task.return` sets task status to `Status::Returned`
2. **Pending futures are NOT automatically cancelled**
3. Post-return handler provides cleanup opportunity

### Root Cause Analysis

The problem is the **async/sync mismatch**:

1. Our handler is `async`-lifted (uses `canon lift async`)
2. When we call `future.write`, it returns `BLOCKED` because `response.new` hasn't started reading yet
3. But our handler doesn't know how to handle `BLOCKED` - it expects immediate completion
4. When we call `future.drop-writable` without writing, it's an error for futures

### Possible Solutions

#### Option 1: Use `wit_future::new` Pattern

The guest code uses `wit_future::new(|| Ok(None))` which likely:

- Creates a future with a callback
- The callback is invoked when the reader is ready
- Doesn't require explicit `future.write`

Need to investigate how this maps to canonical intrinsics.

#### Option 2: Handle BLOCKED Return Code

When `future.write` returns `BLOCKED`:

1. Save the write handle
2. Return from handler (but how?)
3. Resume when notified

This requires understanding the async task suspension model.

#### Option 3: Write After task.return

Since `task.return` doesn't cancel pending futures:

1. Call `response.new(rx)`
2. Call `task.return` with response
3. Call `future.write(tx, None)` after task.return

But: Can we execute code after `task.return`?

### Key Discovery: Post-Return Async Execution

From [component-async-demo](https://github.com/dicej/component-async-demo) http-echo example:

```rust
// Create channels
let (trailers_tx, trailers_rx) = stream_and_future_support::new_future();
let (mut pipe_tx, pipe_rx) = stream_and_future_support::new_stream();

// Spawn task that runs AFTER handler returns
spawn(async move {
    let mut body_rx = body.stream().unwrap();
    while let Some(chunk) = body_rx.next().await {
        pipe_tx.send(chunk).await.unwrap();
    }
    // Write trailers AFTER body completes
    if let Some(trailers) = Body::finish(body).await.unwrap() {
        trailers_tx.write(trailers).await;  // This writes to future!
    }
});

// Return immediately with future rx
Ok(Response::new(headers, Body::new(pipe_rx, Some(trailers_rx))))
```

**Key pattern:**

1. Create future (tx, rx)
2. **Spawn** async task that will write to tx later
3. Return response with rx immediately
4. Spawned task writes to tx **after task.return**

This is "post-return asynchronous execution" - the canonical ABI allows code to continue running after task.return!

### Callback-Based Future Pattern

From wasmtime test-programs:

```rust
let (_, trailers_rx) = wit_future::new(|| Ok(None));  // Callback resolves immediately
let _ = Response::new(headers, Some(contents_rx), trailers_rx);
```

`wit_future::new(|| Ok(None))` creates a future where:

- The callback `|| Ok(None)` is invoked when the reader is ready
- Returns `Ok(None)` immediately (no trailers)
- No explicit `future.write` needed

This is a **higher-level abstraction** over the canonical intrinsics.

### Next Steps

1. **Option A: Implement callback-based futures in Wado**
   - Add a way to create futures with immediate resolution
   - Needs compiler support for closure callbacks in futures

2. **Option B: Use spawn for post-return execution**
   - Implement `spawn` intrinsic for async tasks
   - Write to trailers future after task.return

3. **Option C: Check if wasmtime accepts pre-written futures**
   - Can we write to the future BEFORE calling response.new?
   - Need to test if reader becomes ready after response.new starts

4. **Option D: Check canon ABI for "eager" futures**
   - Is there a canonical option for immediate resolution?

### References

- [wit-bindgen](https://github.com/bytecodealliance/wit-bindgen) - Language binding generator
- [component-async-demo](https://github.com/dicej/component-async-demo) - Async demo (archived, upstreamed)
- [Wasmtime Component Model Async](https://docs.wasmtime.dev/api/wasmtime/component/index.html)

## References

- WASI HTTP WIT: `wasi:http/types@0.3.0-rc-2026-01-06`
- wasmtime P3 support: `wasmtime serve -S p3=y -S http=y`
- Component Model async: task.return, future.new, future.write
