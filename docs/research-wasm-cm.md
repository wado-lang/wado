# Wasm Component Model Async Primitives — Research Reference

This document is a permanent reference for the Wasm Component Model (CM) async primitives
used by the Wado compiler. It covers `stream`, `future`, `error-context`, and the async
calling convention as specified by WASI P3 (0.3.0-rc-2026-01-06) and implemented in
wasmtime v41+.

> **Source**: `vendor/wasmtime/` (wasmtime implementation),
> `vendor/wasm-tools/` (wasm-encoder canonical opcodes),
> `vendor/wasi/` (WASI proposals).

---

## Table of Contents

1. [Core Concepts](#1-core-concepts)
2. [Stream Operations](#2-stream-operations)
3. [Future Operations](#3-future-operations)
4. [Task and Subtask Operations](#4-task-and-subtask-operations)
5. [Waitable Set Operations](#5-waitable-set-operations)
6. [Error Context Operations](#6-error-context-operations)
7. [Return Value Encoding](#7-return-value-encoding)
8. [Async Calling Convention](#8-async-calling-convention)
9. [WASI P3 Interface Patterns](#9-wasi-p3-interface-patterns)
10. [Stream/Future Lifecycle Patterns](#10-streamfuture-lifecycle-patterns)
11. [Pitfalls and Ordering Constraints](#11-pitfalls-and-ordering-constraints)
12. [Wado-Level Error Handling Mapping](#12-wado-level-error-handling-mapping)

---

## 1. Core Concepts

### stream\<T\>

A unidirectional, typed data channel between two ends:

- **Readable end** (rx): the consumer reads data from this handle.
- **Writable end** (tx): the producer writes data to this handle.

Created as a pair via `stream.new`. Closing the writable end signals EOF to readers.
Closing the readable end signals cancellation to writers.

Streams transfer data through **linear memory** — the CM runtime copies items between
the guest's memory and the host's internal buffers. The element type `T` determines the
layout (size, alignment) of each item in memory.

### future\<T\>

A one-shot async value:

- **Readable end** (rx): the consumer waits for the value.
- **Writable end** (tx): the producer fulfills the value exactly once.

Created as a pair via `future.new`. Dropping the writable end without writing cancels
the future.

### Subtask

When a guest calls an `async func` import, the CM runtime creates a **subtask** that
represents the in-flight async operation. The subtask handle is a waitable — it can be
joined to a waitable set to wait for completion.

### Waitable Set

A multiplexing primitive. Multiple waitables (streams, futures, subtasks) can be joined
to a set. `waitable-set.wait` blocks until any member becomes ready.

### Error Context

An opaque handle carrying debug information about errors. Used for structured error
propagation across component boundaries.

---

## 2. Stream Operations

All stream operations are **canonical built-in functions** emitted by the Wasm component
encoder. They do NOT appear in WIT — they are part of the canonical ABI.

### 2.1 stream.new

```
(canon stream.new $stream_type (core func $f))
```

- **Core signature**: `() -> i64`
- **Returns**: packed `(rx: u32, tx: u32)` — `rx` in low 32 bits, `tx` in high 32 bits.
- **Semantics**: creates a fresh stream pair of the given element type.

### 2.2 stream.read

```
(canon stream.read $stream_type $options (core func $f))
```

- **Core signature**: `(rx: i32, ptr: i32, count: i32) -> i32`
- **Parameters**:
  - `rx`: readable stream handle
  - `ptr`: guest memory address to write items into
  - `count`: maximum number of items to read
- **Returns**: `ReturnCode` (see [§7](#7-return-value-encoding))
  - `BLOCKED (0xFFFF_FFFF)`: no data available yet, stream still open
  - `COMPLETED | (n << 4)`: read `n` items successfully
  - `DROPPED | (n << 4)`: writer closed, last `n` items available
  - `CANCELLED | (n << 4)`: operation cancelled
- **Semantics**: non-blocking. Tries to read up to `count` items. If no data is
  available and the writer hasn't closed, returns BLOCKED. The caller must then
  use a waitable set to wait for readiness.

### 2.3 stream.write

```
(canon stream.write $stream_type $options (core func $f))
```

- **Core signature**: `(tx: i32, ptr: i32, count: i32) -> i32`
- **Parameters**:
  - `tx`: writable stream handle
  - `ptr`: guest memory address to read items from
  - `count`: number of items to write
- **Returns**: `ReturnCode`
  - `BLOCKED`: reader's buffer is full, try again later
  - `COMPLETED | (n << 4)`: wrote `n` items
  - `DROPPED | (n << 4)`: reader was dropped
  - `CANCELLED | (n << 4)`: operation cancelled
- **Semantics**: non-blocking. Writes items from memory to the stream.

### 2.4 stream.cancel-read / stream.cancel-write

```
(canon stream.cancel-read $stream_type async? (core func $f))
(canon stream.cancel-write $stream_type async? (core func $f))
```

- **Core signature**: `(handle: i32) -> i32`
- **Parameters**: the stream handle (rx for cancel-read, tx for cancel-write)
- **Returns**: `ReturnCode` — `CANCELLED | (n << 4)` or `BLOCKED`
- **`async` flag**: if set, may return BLOCKED; if unset, blocks until cancelled.

### 2.5 stream.close-readable / stream.close-writable

```
(canon stream.close-readable $stream_type (core func $f))
(canon stream.close-writable $stream_type (core func $f))
```

- **Core signature**: `(handle: i32)` → no return (trap on invalid handle)
- **Semantics**:
  - `close-writable`: signals EOF to readers. Future reads return DROPPED.
  - `close-readable`: signals cancellation to writers. Future writes return DROPPED.

> **Note on naming**: In Wado's `builtin.wado`, these are named `stream-drop-readable`
> and `stream-drop-writable` following the wasmtime naming. The canonical spec uses
> `stream.close-readable` / `stream.close-writable`.

---

## 3. Future Operations

### 3.1 future.new

```
(canon future.new $future_type (core func $f))
```

- **Core signature**: `() -> i64`
- **Returns**: packed `(rx: u32, tx: u32)` — same encoding as stream.new.

### 3.2 future.write

```
(canon future.write $future_type $options (core func $f))
```

- **Core signature**: `(tx: i32, ptr: i32) -> i32`
- **Parameters**:
  - `tx`: writable future handle
  - `ptr`: guest memory address containing the value to write
- **Returns**: `ReturnCode`
  - `COMPLETED | (0 << 4)`: value written successfully (count is always 0 for futures)
  - `BLOCKED`: reader not ready (rare; typically completes immediately)
  - `DROPPED`: reader was dropped
- **Semantics**: fulfills the future with the value at `ptr`. Can only be called once.

### 3.3 future.read

```
(canon future.read $future_type $options (core func $f))
```

- **Core signature**: `(rx: i32, ptr: i32) -> i32`
- **Parameters**:
  - `rx`: readable future handle
  - `ptr`: guest memory address to write the result into
- **Returns**: `ReturnCode`
  - `BLOCKED`: future not yet fulfilled
  - `COMPLETED`: value available at `ptr`
  - `DROPPED`: writer dropped without fulfilling

### 3.4 future.cancel-read / future.cancel-write

Same pattern as stream cancel operations.

### 3.5 future.close-readable / future.close-writable

Same pattern as stream close operations.

- `close-writable` without prior `future.write` = **cancel** the future.
- `close-readable` = stop waiting, signal disinterest to writer.

---

## 4. Task and Subtask Operations

### 4.1 task.return

```
(canon task.return $result_type $options (core func $f))
```

- **Core signature**: `(flat_args...) -> ()`
  The number and types of flat args depend on the result type.
- **Semantics**: delivers the async function's return value to the CM runtime
  **without terminating the Wasm function**. The function continues executing
  after `task.return` — this is essential for fulfilling outstanding futures
  and closing streams.
- **Constraints**:
  - Only valid inside an `async`-lifted export.
  - Must be called exactly once per task.
  - Regular `return` is forbidden in async function bodies.

### 4.2 subtask.drop

```
(canon subtask.drop (core func $f))
```

- **Core signature**: `(subtask: i32) -> ()`
- **Semantics**: drops a completed subtask handle. Must be called after the subtask
  reaches `Returned` status. Traps if called on a still-running subtask.

### 4.3 subtask.cancel

```
(canon subtask.cancel async? (core func $f))
```

- **Core signature**: `(subtask: i32) -> i32`
- **Returns**: status code or BLOCKED (if async flag set)

### 4.4 task.cancel

```
(canon task.cancel (core func $f))
```

- **Core signature**: `() -> ()`
- **Semantics**: cancels the current task (self-cancellation). No parameters because
  it implicitly applies to the task executing this call.

### 4.5 backpressure.inc / backpressure.dec

```
(canon backpressure.inc (core func $f))
(canon backpressure.dec (core func $f))
```

- **Core signature**: `() -> ()`
- **Semantics**: increment/decrement the backpressure counter for the current component
  instance. While the counter is > 0, the runtime will not schedule new calls into
  this instance, providing flow control for async exports.

---

## 5. Waitable Set Operations

### 5.1 waitable-set.new

```
(canon waitable-set.new (core func $f))
```

- **Core signature**: `() -> i32`
- **Returns**: waitable set handle.

### 5.2 waitable.join

```
(canon waitable.join (core func $f))
```

- **Core signature**: `(waitable: i32, set: i32) -> ()`
- **Parameters**:
  - `waitable`: handle to a stream, future, or subtask
  - `set`: waitable set handle (pass 0 to remove from current set)
- **Semantics**: adds the waitable to the set. When the waitable becomes ready,
  `waitable-set.wait` will return with that waitable's information.

### 5.3 waitable-set.wait

```
(canon waitable-set.wait (core func $f))
```

- **Core signature**: `(set: i32, payload_ptr: i32) -> i32`
- **Parameters**:
  - `set`: waitable set handle
  - `payload_ptr`: guest memory address (8 bytes, 4-byte aligned)
- **Writes to payload_ptr**:
  - `+0 (u32)`: waitable handle that became ready
  - `+4 (u32)`: event-specific payload (ReturnCode or Status)
- **Returns**: event code:
  - `0` = EVENT_NONE
  - `1` = EVENT_SUBTASK — payload is Status (see [§8.1](#81-status-encoding))
  - `2` = EVENT_STREAM_READ — payload is ReturnCode
  - `3` = EVENT_STREAM_WRITE — payload is ReturnCode
  - `4` = EVENT_FUTURE_READ — payload is ReturnCode
  - `5` = EVENT_FUTURE_WRITE — payload is ReturnCode
  - `6` = EVENT_CANCELLED
- **Semantics**: blocks until any waitable in the set is ready. This is the
  fundamental blocking primitive for async coordination.

### 5.4 waitable-set.poll

Same as `waitable-set.wait` but **non-blocking**. Returns EVENT_NONE if nothing ready.

### 5.5 waitable-set.drop

```
(canon waitable-set.drop (core func $f))
```

- **Core signature**: `(set: i32) -> ()`

---

## 6. Error Context Operations

### 6.1 error-context.new

```
(canon error-context.new $options (core func $f))
```

- **Core signature**: `(debug_msg_ptr: i32, debug_msg_len: i32) -> i32`
- **Returns**: error context handle
- **Semantics**: creates an error context with a UTF-8 debug message from memory.

### 6.2 error-context.debug-message

```
(canon error-context.debug-message $options (core func $f))
```

- **Core signature**: `(handle: i32, out_ptr: i32) -> ()`
- **Semantics**: writes the debug message's (ptr, len) pair to `out_ptr`.
  The runtime allocates via `realloc` and writes the string to memory.

### 6.3 error-context.drop

```
(canon error-context.drop (core func $f))
```

- **Core signature**: `(handle: i32) -> ()`

---

## 7. Return Value Encoding

All stream/future read/write operations return a 32-bit `ReturnCode`:

```
BLOCKED sentinel: 0xFFFF_FFFF (u32::MAX)
  → operation did not complete; caller must wait via waitable set.

Normal return: (count << 4) | status
  → count: number of items processed (28-bit, max 2^28-1)
  → status (4-bit):
      0 = COMPLETED  — operation succeeded
      1 = DROPPED    — the other end was closed/dropped
      2 = CANCELLED  — operation was explicitly cancelled
```

**Decoding example**:

```
result = stream_read(rx, ptr, 100);
if result == 0xFFFF_FFFF {
    // BLOCKED — no data available, use waitable set
} else {
    status = result & 0xF;      // 0, 1, or 2
    count  = result >> 4;       // number of items
    if status == 0 {
        // COMPLETED: read `count` items at ptr
    } else if status == 1 {
        // DROPPED: writer closed, `count` final items at ptr
    } else {
        // CANCELLED
    }
}
```

---

## 8. Async Calling Convention

### 8.1 Status Encoding

When an async function is called via `canon lower async`, the return value encodes
the subtask status:

```
Bits 0-3: status
    0 = Starting     — async guest task is being set up
    1 = Started      — async host task is in flight
    2 = Returned     — sync completion (task finished immediately)
    3 = StartCancelled
    4 = ReturnCancelled

Bits 4-31: subtask handle (28-bit)
    Non-zero if status is Starting or Started (task still in flight).
    Zero if status is Returned (task already completed).
```

**Decoding**:

```
packed = call_async_import(args...);
status = packed & 0xF;
handle = packed >> 4;

if handle != 0 {
    // Async: task in flight. Must wait for completion.
    waitable_join(handle, waitable_set);
    waitable_set_wait(waitable_set, payload_ptr);
    subtask_drop(handle);
} else {
    // Sync: task completed immediately (status == Returned == 2).
}
```

### 8.2 Event Codes

Events returned by `waitable-set.wait`:

| Code | Name               | Payload         |
| ---- | ------------------ | --------------- |
| 0    | EVENT_NONE         | —               |
| 1    | EVENT_SUBTASK      | Status (§8.1)   |
| 2    | EVENT_STREAM_READ  | ReturnCode (§7) |
| 3    | EVENT_STREAM_WRITE | ReturnCode (§7) |
| 4    | EVENT_FUTURE_READ  | ReturnCode (§7) |
| 5    | EVENT_FUTURE_WRITE | ReturnCode (§7) |
| 6    | EVENT_CANCELLED    | —               |

### 8.3 Async Export (canon lift async)

When the host calls an async-lifted export:

1. The host calls the core Wasm function (the "start" function).
2. The core function runs until it calls `task.return` or exits.
3. If `task.return` is called, the result is delivered to the host,
   but the core function **continues executing** (post-return phase).
4. The core function eventually returns (exits), completing the task.

This two-phase model (pre-return + post-return) is essential for HTTP handlers
that need to fulfill trailers futures after delivering the response.

### 8.4 Async Import (canon lower async)

When the guest calls an async-lowered import:

1. The guest calls the core import function with arguments.
2. The import returns a packed status (§8.1).
3. If the handle is non-zero, the import hasn't completed yet.
   The guest must wait via a waitable set.
4. When the subtask completes (EVENT_SUBTASK with Returned status),
   the result is available and the subtask handle must be dropped.

### 8.5 Constants

```
MAX_FLAT_ASYNC_PARAMS = 4    (vs 16 for sync calls)
MAX_FLAT_ASYNC_RESULTS = 4   (vs 16 for sync calls)
```

Async functions with more than 4 flat params/results use pointer-based
argument passing through linear memory.

---

## 9. WASI P3 Interface Patterns

### 9.1 CLI — stdout/stderr (write)

```wit
// wasi:cli/stdout@0.3.0-rc-2026-01-06
interface stdout {
    use types.{error-code};
    write-via-stream: async func(data: stream<u8>) -> result<_, error-code>;
}
```

**Pattern**: caller creates a stream, passes the **readable end** to `write-via-stream`,
writes data to the **writable end**, then closes the writable end. The async function
completes when all data is consumed.

**Sequence**:

```
1. [rx, tx] = stream.new<u8>()
2. subtask = call write-via-stream(rx)     ← pass readable end
3. stream.write(tx, data_ptr, data_len)    ← write data
4. stream.close-writable(tx)               ← signal EOF
5. wait for subtask completion             ← blocks until host consumes all data
6. subtask.drop(handle)
```

### 9.2 CLI — stdin (read)

```wit
// wasi:cli/stdin@0.3.0-rc-2026-01-06
interface stdin {
    use types.{error-code};
    read-via-stream: func() -> tuple<stream<u8>, future<result<_, error-code>>>;
}
```

**Pattern**: returns a stream of incoming bytes and a future for the completion result.
The stream is **created by the host** — the guest reads from it.

**Sequence**:

```
1. [stream_rx, done_future_rx] = call read-via-stream()
2. loop:
     result = stream.read(stream_rx, buf_ptr, buf_len)
     if result == BLOCKED → wait on waitable set
     if status == DROPPED → EOF, exit loop
     if status == COMPLETED → process `count` bytes
3. stream.close-readable(stream_rx)
4. check done_future_rx for error status
5. future.close-readable(done_future_rx)
```

### 9.3 Filesystem — read/write

```wit
// wasi:filesystem/types@0.3.0-rc-2026-01-06 (on descriptor resource)
read-via-stream: func(offset: filesize) -> tuple<stream<u8>, future<result<_, error-code>>>;
write-via-stream: async func(data: stream<u8>, offset: filesize) -> result<_, error-code>;
append-via-stream: async func(data: stream<u8>) -> result<_, error-code>;
read-directory: async func() -> tuple<stream<directory-entry>, future<result<_, error-code>>>;
```

Same patterns as CLI: reads return `tuple<stream, future>`, writes are `async func`
taking a stream.

### 9.4 HTTP — Request and Response

```wit
// wasi:http/types@0.3.0-rc-2026-01-06
resource request {
    new: static func(
        headers: headers,
        contents: option<stream<u8>>,
        trailers: future<result<option<trailers>, error-code>>,
        options: option<request-options>,
    ) -> tuple<request, future<result<_, error-code>>>;

    consume-body: static func(
        this: request,
        res: future<result<_, error-code>>,
    ) -> tuple<stream<u8>, future<result<option<trailers>, error-code>>>;
}

resource response {
    new: static func(
        headers: headers,
        contents: option<stream<u8>>,
        trailers: future<result<option<trailers>, error-code>>,
    ) -> tuple<response, future<result<_, error-code>>>;

    consume-body: static func(
        this: response,
        res: future<result<_, error-code>>,
    ) -> tuple<stream<u8>, future<result<option<trailers>, error-code>>>;
}
```

```wit
// wasi:http/handler@0.3.0-rc-2026-01-06
interface handler {
    handle: async func(request: request) -> result<response, error-code>;
}

// wasi:http/client@0.3.0-rc-2026-01-06
interface client {
    send: async func(request: request) -> result<response, error-code>;
}
```

### 9.5 Common WIT Patterns Summary

| Operation        | WIT Pattern                                                                          | Who creates stream? |
| ---------------- | ------------------------------------------------------------------------------------ | ------------------- |
| **Write to**     | `async func(data: stream<T>) -> result<_, E>`                                        | Guest (caller)      |
| **Read from**    | `func() -> tuple<stream<T>, future<result<_, E>>>`                                   | Host (callee)       |
| **HTTP new**     | `func(contents: option<stream<u8>>, trailers: future<...>) -> tuple<T, future<...>>` | Guest               |
| **HTTP consume** | `func(this: T, res: future<...>) -> tuple<stream<u8>, future<...>>`                  | Host (extracts)     |

---

## 10. Stream/Future Lifecycle Patterns

### 10.1 Producer-Writes-to-Consumer (stdout, filesystem write)

```
Guest (producer)                      Host (consumer)
─────────────────                     ─────────────────
[rx, tx] = stream.new()
                                      ← rx passed to async import
subtask = write-via-stream(rx)
stream.write(tx, data, len)           → host reads from internal buffer
stream.write(tx, more_data, len)      → host reads more
stream.close-writable(tx)             → host sees EOF
                                      → async import returns Ok/Err
wait for subtask (EVENT_SUBTASK)
subtask.drop(handle)
```

### 10.2 Consumer-Reads-from-Producer (stdin, filesystem read)

```
Guest (consumer)                      Host (producer)
─────────────────                     ─────────────────
[stream_rx, done_rx] = read-via-stream()
                                      → host begins writing to stream
result = stream.read(rx, buf, max)
  if BLOCKED → join to waitable set
            → wait → retry read
  if COMPLETED → got data
  if DROPPED → EOF
stream.close-readable(rx)
check done_rx for error
future.close-readable(done_rx)
```

### 10.3 HTTP Response (Service Handler)

```
export async fn handle(request) -> Result<Response, ErrorCode>:

1. Create trailers future pair:
   [trailers_rx, trailers_tx] = future.new<Result<Option<Trailers>, ErrorCode>>()

2. Optionally create body stream:
   [body_rx, body_tx] = stream.new<u8>()

3. Create response (passes ownership of rx handles to runtime):
   [response, done_rx] = Response::new(headers, Some(body_rx), trailers_rx)

4. task.return(Ok(response))
   ← response headers sent to client

5. POST-RETURN PHASE (function still alive):
   stream.write(body_tx, body_data, len)
   stream.close-writable(body_tx)
   ← body sent to client

6. Fulfill trailers:
   future.write(trailers_tx, Ok(None))   // or Ok(Some(trailers))
   future.close-writable(trailers_tx)

7. Function returns (task complete)
```

### 10.4 HTTP Request Body Reading (Service Handler)

```
export async fn handle(request) -> Result<Response, ErrorCode>:

1. Create error future for consume-body:
   [err_rx, err_tx] = future.new<Result<(), ErrorCode>>()

2. Extract body stream:
   [body_stream, trailers_future] = Request::consume-body(request, err_rx)

3. Read body:
   loop:
     result = stream.read(body_stream, buf, max)
     if BLOCKED → wait
     if DROPPED → done
     if COMPLETED → process bytes

4. Check trailers:
   result = future.read(trailers_future, buf)
   if BLOCKED → wait
   → process trailers

5. Signal success:
   future.write(err_tx, Ok(()))
   future.close-writable(err_tx)
```

---

## 11. Pitfalls and Ordering Constraints

### 11.1 Stream Write Order: Start Consumer First

**Critical**: When writing to stdout/stderr via `write-via-stream`, the consumer
(`write-via-stream`) must be started **before** writing to the stream.

```
✓ CORRECT:
  subtask = write-via-stream(rx)   // start consumer first
  stream.write(tx, data, len)      // then write

✗ WRONG:
  stream.write(tx, data, len)      // write to nobody → may block forever
  subtask = write-via-stream(rx)   // too late
```

If data is written before the consumer exists, `stream.write` may return BLOCKED
and there is nobody to drain the buffer.

### 11.2 Must Close Writable End

After writing all data to a stream, the **writable end must be closed**
(`stream.close-writable`). Without this:

- The consumer will never see EOF.
- `write-via-stream` will never return.
- The subtask will hang forever.

### 11.3 Must Fulfill Trailers Future (HTTP)

In HTTP handlers, the trailers future **must be fulfilled** after `task.return`:

```
✓ CORRECT:
  task.return(Ok(response))
  future.write(trailers_tx, Ok(None))

✗ WRONG:
  task.return(Ok(response))
  // function returns without fulfilling trailers_tx
  // → runtime gets cancelled future → protocol error
```

Even if there are no trailers, write `Ok(None)`.

### 11.4 Close Body Stream Before Trailers

HTTP protocol requires the body to complete before trailers. The body stream's
writable end must be closed before the trailers future is fulfilled:

```
stream.close-writable(body_tx)        // body complete
future.write(trailers_tx, Ok(None))   // then trailers
```

### 11.5 Handle BLOCKED Return Codes

Stream/future read/write operations are **non-blocking**. A return of BLOCKED
means the operation cannot proceed right now. The caller must:

1. Join the handle to a waitable set.
2. Call `waitable-set.wait` to block until ready.
3. Retry the operation.

Simply retrying in a busy loop without waiting wastes CPU and may never make progress.

### 11.6 Memory Lifetime for stream.write / future.write

The data at the memory pointer must remain valid until the operation completes.
For non-blocking operations that return BLOCKED, the memory must remain valid
until the operation is retried or cancelled.

In practice for Wado: since `stream.write` in the current implementation always
processes the entire buffer synchronously (wasmtime's current behavior), this is
not yet an issue — but it may become one with truly async I/O.

### 11.7 subtask.drop Requires Completion

`subtask.drop` must only be called after the subtask reaches `Returned` status.
Calling it on a running subtask will trap. Always wait for completion first:

```
waitable_set_wait(set, payload)   // blocks until EVENT_SUBTASK
subtask_drop(handle)              // safe after completion
```

### 11.8 future.write Is One-Shot

`future.write` can only be called once per future. A second call will trap.
This is unlike streams, which support multiple writes.

---

## 12. Wado-Level Error Handling Mapping

This section documents how CM ReturnCode values map to Wado-level types
in the resource method signatures.

### 12.1 ReturnCode → Wado Type Mapping

| Operation                  | CM Return       | Wado Mapping                              |
| -------------------------- | --------------- | ----------------------------------------- |
| `future.read` — COMPLETED  | Value at ptr    | `Option::Some(value)`                     |
| `future.read` — DROPPED    | No value        | `Option::None`                            |
| `future.read` — BLOCKED    | (internal)      | Synthesis waits via waitable-set, retries |
| `future.write` — COMPLETED | Value delivered | Returns normally                          |
| `future.write` — DROPPED   | Reader gone     | Returns normally (no-op, value discarded) |
| `future.write` — BLOCKED   | (internal)      | Synthesis waits via waitable-set, retries |
| `stream.read` — COMPLETED  | `count` items   | `List<T>` with `count` elements           |
| `stream.read` — DROPPED    | EOF, last items | Empty `List<T>` (EOF signal)              |
| `stream.read` — BLOCKED    | (internal)      | Synthesis waits via waitable-set, retries |
| `stream.write` — COMPLETED | `count` items   | Returns normally                          |
| `stream.write` — DROPPED   | Reader gone     | Returns normally (no-op)                  |
| `stream.write` — BLOCKED   | (internal)      | Synthesis waits via waitable-set, retries |

### 12.2 Future::read Returns Option<T>

`Future::read(&self) -> Option<T>`:

- `Some(value)` — writer fulfilled the future (COMPLETED)
- `None` — writer dropped without fulfilling (DROPPED)

DROPPED is a **normal condition**: the writer may be cancelled, or close the writable
end without calling `future.write`. Returning `Option<T>` rather than bare `T` avoids
trapping on a recoverable situation.

### 12.3 Application Errors vs Transfer Errors

Application-level errors are encoded in the **payload type**, not in ReturnCode:

```
Future<Result<Response, ErrorCode>>
       ~~~~~~~~~~~~~~~~~~~~~~~~
       This is the payload type T. ErrorCode is application-level.
```

ReturnCode only indicates the **transfer status** — did the CM transfer succeed?
A COMPLETED return with `Result::Err(ErrorCode)` means: "the future was fulfilled,
and the fulfillment value is an error."

`ErrorContext` is a separate CM resource for carrying debug messages across component
boundaries. It is not used for stream/future operational errors.

---

## Appendix A: Canonical Built-in Opcode Reference

From `vendor/wasm-tools/crates/wasm-encoder/src/component/canonicals.rs`:

| Category         | Operation      | Wasm Encoder Method                    |
| ---------------- | -------------- | -------------------------------------- |
| **Stream**       | new            | `CanonicalFunctionSection::stream_new` |
|                  | read           | `...::stream_read`                     |
|                  | write          | `...::stream_write`                    |
|                  | cancel-read    | `...::stream_cancel_read`              |
|                  | cancel-write   | `...::stream_cancel_write`             |
|                  | close-readable | `...::stream_drop_readable`            |
|                  | close-writable | `...::stream_drop_writable`            |
| **Future**       | new            | `...::future_new`                      |
|                  | read           | `...::future_read`                     |
|                  | write          | `...::future_write`                    |
|                  | cancel-read    | `...::future_cancel_read`              |
|                  | cancel-write   | `...::future_cancel_write`             |
|                  | close-readable | `...::future_drop_readable`            |
|                  | close-writable | `...::future_drop_writable`            |
| **Task**         | return         | `...::task_return`                     |
|                  | cancel         | `...::task_cancel`                     |
| **Subtask**      | drop           | `...::subtask_drop`                    |
|                  | cancel         | `...::subtask_cancel`                  |
| **Backpressure** | inc            | `...::backpressure_inc`                |
|                  | dec            | `...::backpressure_dec`                |
| **Waitable**     | set.new        | `...::waitable_set_new`                |
|                  | set.wait       | `...::waitable_set_wait`               |
|                  | set.poll       | `...::waitable_set_poll`               |
|                  | set.drop       | `...::waitable_set_drop`               |
|                  | join           | `...::waitable_join`                   |
| **Error Ctx**    | new            | `...::error_context_new`               |
|                  | debug-message  | `...::error_context_debug_message`     |
|                  | drop           | `...::error_context_drop`              |

## Appendix B: Flat Stream Variants

For element types with a "flat" representation (no pointers, no handles — just
plain scalars), wasmtime provides optimized `flat_stream.read` / `flat_stream.write`
variants that include explicit `payload_size` and `payload_align` parameters.

`stream<u8>` uses the flat variant since `u8` is a scalar type.

## Appendix C: WASI P3 Worlds

### wasi:cli/command

```wit
world command {
    import environment;
    import exit;
    import types;           // error-code enum
    import stdin;           // read-via-stream
    import stdout;          // write-via-stream (async)
    import stderr;          // write-via-stream (async)
    import terminal-*;
    import wasi:clocks/*;
    import wasi:filesystem/*;
    import wasi:sockets/*;
    import wasi:random/*;

    export run;             // async func() -> result
}
```

### wasi:http/service

```wit
world service {
    import wasi:cli/stdout;
    import wasi:cli/stderr;
    import wasi:cli/stdin;
    import types;           // fields, request, response, error-code, etc.
    import client;          // send: async func(request) -> result<response, error-code>
    import wasi:clocks/*;
    import wasi:random/*;

    export handler;         // handle: async func(request) -> result<response, error-code>
}
```

### wasi:http/middleware

Same as service but also imports `handler` (the upstream handler to forward to).

## Appendix D: File Locations in Vendor

| What                       | Path                                                                                      |
| -------------------------- | ----------------------------------------------------------------------------------------- |
| WASI P3 CLI WIT            | `vendor/wasmtime/crates/wasi/src/p3/wit/deps/cli.wit`                                     |
| WASI P3 Filesystem WIT     | `vendor/wasmtime/crates/wasi/src/p3/wit/deps/filesystem.wit`                              |
| WASI P3 HTTP WIT           | `vendor/wasmtime/crates/wasi-http/src/p3/wit/deps/http.wit`                               |
| WASI P3 Clocks WIT         | `vendor/wasmtime/crates/wasi/src/p3/wit/deps/clocks.wit`                                  |
| WASI P3 Sockets WIT        | `vendor/wasmtime/crates/wasi/src/p3/wit/deps/sockets.wit`                                 |
| WASI P3 Random WIT         | `vendor/wasmtime/crates/wasi/src/p3/wit/deps/random.wit`                                  |
| Canonical function sigs    | `vendor/wasmtime/crates/environ/src/component.rs`                                         |
| Stream/future runtime impl | `vendor/wasmtime/crates/wasmtime/src/runtime/component/concurrent/futures_and_streams.rs` |
| Canonical ABI encoder      | `vendor/wasm-tools/crates/wasm-encoder/src/component/canonicals.rs`                       |
| Async tests                | `vendor/wasmtime/tests/all/component_model/async.rs`                                      |
