# WASI HTTP Implementation

This document describes the `wasi:http/service` world implementation in Wado.

## Handler Signature

An HTTP handler is an `export async fn` that takes a `Request` and returns `Result<Response, ErrorCode>`:

```wado
use {
    Request,
    Response,
    ErrorCode,
    Fields,
    Trailers,
} from "wasi:http";

export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    let [trailers_future, trailers_tx] = Future::<Result<Option<Trailers>, ErrorCode>>::new();
    let headers = Fields::new();
    let [response, _tx_future] = Response::new(headers, null, trailers_future);

    task return Result::<Response, ErrorCode>::Ok(response);
    trailers_tx.write(Result::<Option<Trailers>, ErrorCode>::Ok(null));
}
```

### Why `export async fn`

The `async` modifier selects the CM async calling convention. The difference from a regular `export fn`:

| Aspect            | `export fn` (sync)                                 | `export async fn`                                        |
| ----------------- | -------------------------------------------------- | -------------------------------------------------------- |
| Result delivery   | Wasm function return value                         | `task.return` CM instruction                             |
| Function lifetime | Ends when result is returned                       | Continues after `task return`                            |
| Adapter strategy  | Adapter wraps return value and calls `task-return` | Adapter lifts params only; user code calls `task return` |
| Post-return work  | Not possible                                       | Futures/streams can be fulfilled after result delivery   |

HTTP handlers require `async` because `Response::new` takes a `Future<Result<Option<Trailers>, ErrorCode>>` for the trailers parameter. The trailers future must be fulfilled **after** the response is delivered to the runtime. With a sync export, the function would terminate before the trailers future could be resolved, causing a runtime hang or trap.

### `task return`

`task return expr;` calls the CM `task.return` instruction to deliver the handler result without terminating the function. Code after `task return` continues executing normally — this is where the trailers future is fulfilled.

```wado
export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    // ... build response ...

    // Deliver the response to the CM runtime. Function continues.
    task return Result::<Response, ErrorCode>::Ok(response);

    // Post-return: fulfill the trailers future.
    trailers_tx.write(Result::<Option<Trailers>, ErrorCode>::Ok(null));
}
```

Rules:

- `task return` is only valid inside `export async fn` bodies.
- Regular `return` is forbidden in `async` function bodies.
- The expression is type-checked against the declared return type.

## Response Lifecycle

A successful HTTP response follows this sequence:

```
1. Create trailers future      Future::<...>::new() → [rx, tx]
2. Create response headers     Fields::new()
3. Construct response           Response::new(headers, body, rx) → [response, tx_future]
4. Deliver response             task return Ok(response)
5. Fulfill trailers future     tx.write(Ok(null))     ← post-return
```

Step 5 is critical. The CM runtime waits for the trailers future to resolve before completing the HTTP response. Dropping the writable end without writing is a runtime error.

### `FutureWritable<T>`

`FutureWritable<T>` is the writable end of a CM future. Obtained from `Future::<T>::new()`, which returns a `[Future<T>, FutureWritable<T>]` tuple.

| Method            | Description                          |
| ----------------- | ------------------------------------ |
| `write(value: T)` | Resolves the future with a value     |
| `drop()`          | Cancels the future without resolving |

For HTTP trailers, the convention is:

```wado
// No trailers (the common case)
trailers_tx.write(Result::<Option<Trailers>, ErrorCode>::Ok(null));
```

### Error Responses

Return `Err(ErrorCode)` for error responses. No `Response` object or trailers future is needed:

```wado
export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    task return Result::<Response, ErrorCode>::Err(
        ErrorCode::InternalError(Option::<String>::Some("something went wrong"))
    );
}
```

## Available Types

All types are imported from `"wasi:http"`:

### Resources

| Type             | Description                      |
| ---------------- | -------------------------------- |
| `Request`        | Incoming HTTP request            |
| `Response`       | HTTP response                    |
| `Fields`         | HTTP header/trailer fields       |
| `RequestOptions` | Request configuration (timeouts) |

### Type Aliases

| Type         | Base        | Description                    |
| ------------ | ----------- | ------------------------------ |
| `FieldName`  | `String`    | Header field name              |
| `FieldValue` | `Array<u8>` | Header field value (raw bytes) |
| `Headers`    | `Fields`    | Request/response headers       |
| `Trailers`   | `Fields`    | HTTP trailers                  |
| `StatusCode` | `u16`       | HTTP status code               |

### Variants

| Type          | Description                                                                |
| ------------- | -------------------------------------------------------------------------- |
| `ErrorCode`   | HTTP error (42 cases: `DNSTimeout`, `InternalError(Option<String>)`, etc.) |
| `Method`      | HTTP methods (`Get`, `Post`, `Put`, etc.)                                  |
| `Scheme`      | URI scheme (`HTTP`, `HTTPS`, `Other(String)`)                              |
| `HeaderError` | Header operation errors (`Immutable`, `InvalidSyntax`, etc.)               |

### Key Methods

`Fields`:

| Method                | Signature                        |
| --------------------- | -------------------------------- |
| `new()`               | `-> Fields`                      |
| `from_list(entries)`  | `-> Result<Fields, HeaderError>` |
| `has(name)`           | `-> bool`                        |
| `get(name)`           | `-> Array<FieldValue>`           |
| `append(name, value)` | `-> Result<(), HeaderError>`     |
| `delete(name)`        | `-> Result<(), HeaderError>`     |
| `clone()`             | `-> Fields`                      |

`Response`:

| Method                         | Signature                                      |
| ------------------------------ | ---------------------------------------------- |
| `new(headers, body, trailers)` | `-> [Response, Future<Result<(), ErrorCode>>]` |
| `get_status_code()`            | `-> StatusCode`                                |
| `set_status_code(code)`        | `-> Result<(), ()>`                            |
| `get_headers()`                | `-> Headers`                                   |

`Request`:

| Method            | Signature           |
| ----------------- | ------------------- |
| `get_method()`    | `-> Method`         |
| `get_path()`      | `-> String`         |
| `get_authority()` | `-> Option<String>` |
| `get_headers()`   | `-> Headers`        |

## CM Adapter Generation

The compiler synthesizes three kinds of export adapters based on the function signature:

| Signature                                     | Adapter Strategy                       | Generated Code                                              |
| --------------------------------------------- | -------------------------------------- | ----------------------------------------------------------- |
| `export fn run()`                             | `synthesize_void_export_adapter`       | Call user fn, then `task-return(0)`                         |
| `export fn f() -> T`                          | `synthesize_non_result_export_adapter` | Call user fn, lower return value, `task-return(0, flat...)` |
| `export async fn handle(req) -> Result<R, E>` | `synthesize_async_export_adapter`      | Lift flat params, call user fn (user calls `task return`)   |

For async exports, the adapter only handles parameter lifting. The `task return` statement in the user function body is expanded by `expand_task_returns_in_func` during the CM Adapter phase into:

1. Lower the Wado value to flat CM ABI values (`synthesize_lower_to_flat`)
2. Call `builtin::task_return(0, flat0, flat1, ...)` where `0` is the Ok discriminant

For the `Result<Response, ErrorCode>` return type, the flattening produces 8 i32/i64 values:

```
Ok:  task-return(0, response_handle, padding...)
Err: task-return(1, error_case, has_payload, payload_ptr, payload_len, padding...)
```

## E2E Test Fixtures

HTTP test fixtures are in `wado-compiler/tests/fixtures/http-*.wado`. Each has a `__DATA__` section with `"world": "wasi:http/service"`:

| Fixture                        | Status | Description                                        |
| ------------------------------ | ------ | -------------------------------------------------- |
| `http-200.wado`                | Pass   | 200 OK with empty body                             |
| `http-400.wado`                | Pass   | 400 Bad Request via `set_status_code`              |
| `http-500.wado`                | Pass   | 500 Internal Server Error via `set_status_code`    |
| `http-error-code.wado`         | Pass   | `Err(ErrorCode::InternalError(null))`              |
| `http-error-code-payload.wado` | Pass   | `Err(ErrorCode::InternalError(Some("...")))`       |
| `http-fields.wado`             | Pass   | Fields operations: new, has, append, delete, clone |
| `http-future-new.wado`         | Pass   | `Future::<T>::new()` handle pair creation          |
| `http-response-ops.wado`       | Pass   | Response: new, get/set status code                 |

## Not Yet Implemented

- Response body content via `Stream<u8>`
- Reading request body
- Sending outbound requests (`wasi:http/client`)
- Middleware world (`wasi:http/middleware`)
- Custom trailers (currently always `Ok(null)`)

## References

- WASI HTTP types: `wasi:http/types@0.3.0-rc-2026-01-06`
- WASI HTTP handler: `wasi:http/handler@0.3.0-rc-2026-01-06`
- Wado HTTP types: `wado-compiler/lib/wasi/http.wado`
- Core future types: `wado-compiler/lib/core/prelude/types.wado`
