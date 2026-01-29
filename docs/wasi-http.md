# WASI HTTP Implementation Notes

This document tracks the implementation of `wasi:http/service` world support in Wado.

## Current Status

- HTTP server compiles and runs with `--world wasi:http/service`
- Returns 500 errors correctly with error message payload
- HTTP 200 response creation is blocked (see below)

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

## What Doesn't Work

### HTTP 200 Response Creation

Creating a successful response requires calling `[static]response.new`:

```
response.new(headers, body, trailers) -> [Response, Future<Result<(), ErrorCode>>]
```

The `trailers` parameter is `Future<Result<Option<Fields>, ErrorCode>>`.

#### Attempted Approaches

| Approach | Code | Result |
|----------|------|--------|
| Null handle | `trailers = 0` | trap: "channel closed" at response.new |
| Drop writable end | `future-new` → `future-drop-writable(tx)` → `response.new(rx)` | trap: "channel closed" at future-drop-writable |
| Write None before response.new | `future-new` → `future-write(tx, None)` → `response.new(rx)` | Hangs (blocks indefinitely) |
| Write None after response.new | `future-new` → `response.new(rx)` → `future-write(tx, None)` | Hangs (blocks indefinitely) |
| Leave future pending | `future-new` → `response.new(rx)` → return (tx leaks) | trap: "channel closed" after handler returns |

#### Analysis

1. `future-write` blocks because it waits for the reader to be ready
2. `future-drop-writable` signals an error to the reader (channel closed)
3. Passing null (0) is not allowed for future handles
4. The runtime expects the trailers future to be properly fulfilled

The Component Model async semantics require:
- A future to be fulfilled (written) OR explicitly cancelled
- The sender must not be dropped without writing

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

- [ ] Study wasmtime-wasi-http source code for trailers handling
- [ ] Find test cases that show correct usage of `response.new`
- [ ] Understand Component Model async boundary semantics
- [ ] Check if `future-write` requires a specific caller context

### Implementation

- [ ] Implement correct trailers future lifecycle
- [ ] Handle transmission future from response.new
- [ ] Return Ok(response) via task-return

### Testing

- [ ] Add E2E tests using wasmtime API (not CLI)
- [ ] Test HTTP 200 response with empty body
- [ ] Test HTTP 200 response with body content

## References

- WASI HTTP WIT: `wasi:http/types@0.3.0-rc-2026-01-06`
- wasmtime P3 support: `wasmtime serve -S p3=y -S http=y`
- Component Model async: task.return, future.new, future.write
