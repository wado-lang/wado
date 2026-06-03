# WEP: WASI HTTP Integration

## Context

WASI P3 defines an HTTP model built on the Component Model's native async primitives — `future<T>`, `stream<T>`, and `async func`. The CM async calling convention differs fundamentally from regular Wasm function calls: an `async func` export delivers its result through a `task.return` instruction rather than a Wasm return value, allowing the function to keep executing after delivery.

HTTP handlers are required to use this convention because `Response::new` takes a `Future<Result<Option<Trailers>, ErrorCode>>` for its trailers parameter. The trailers future must be fulfilled **after** the response is delivered to the CM runtime. A synchronous export terminates the moment it returns; there is no opportunity to fulfill the future. Only an async export can deliver the response and then continue executing to close the trailers future.

The `wasi:http` package defines three worlds:

| World                  | Role                                                        | Status                                     |
| ---------------------- | ----------------------------------------------------------- | ------------------------------------------ |
| `wasi:http/service`    | Inbound HTTP handler — exports `handle`                     | Implemented                                |
| `wasi:http/client`     | Outbound HTTP client — imports `Client::send`               | Defined; not yet callable from Wado        |
| `wasi:http/middleware` | Handler chain — exports `handle`, imports `Handler::handle` | Defined; not yet supported as target world |

## Decision

### 1. Handler Signature (`wasi:http/service`)

The handler is a single `export async fn` named `handle`:

```wado
use { Request, Response, ErrorCode } from "wasi:http";

export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    // ...
}
```

- The name `handle` is mandated by the `wasi:http/service` world.
- `async` selects the CM async calling convention, which is required for post-return execution (trailers).
- `return` is forbidden inside `async fn` bodies; use `task return` instead.

### 2. `task return` — CM Boundary Result Delivery

`task return expr;` is a statement valid only inside `export async fn` bodies. It calls the CM `task.return` instruction to deliver the function's result to the CM runtime without terminating the Wasm function.

```wado
export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    // ... build response ...

    task return Result::<Response, ErrorCode>::Ok(response);  // deliver result

    // Function is still alive here. Fulfill outstanding futures.
    trailers_tx.write(Result::<Option<Trailers>, ErrorCode>::Ok(null));
}
```

**Why `task return` only at the CM boundary:** Wado's internal async model is colorless — async suspension and resumption happen transparently at the WASI call level. There is no user-visible async/await inside Wado. `task return` is a CM-specific mechanism and has no meaning inside Wado-to-Wado function calls.

Rules:

- `task return expr` is only valid in `export async fn` bodies.
- `return` is forbidden in `async fn` bodies.
- The expression is type-checked against the declared return type of the enclosing `export async fn`.

The compiler expands `task return` during the synthesis phase (CM binding generation) into a sequence that lowers the Wado value to flat CM ABI values and calls `builtin::task_return`. See [WEP: TIR-Level CM Binding Synthesis](wep-2026-02-15-cm-binding-synthesis.md) for implementation details.

### 3. Trailers Future Pattern

Every successful response requires a trailers future:

```
1. Create trailers future pair   Future::<Result<Option<Trailers>, ErrorCode>>::new()
                                 → [trailers_rx, trailers_tx]
2. Create response headers       Fields::new()
3. Construct response            Response::new(headers, null, trailers_rx)
                                 → [response, _body_done_future]
4. Deliver response              task return Ok(response)
5. Fulfill trailers              trailers_tx.write(Ok(null))   ← post-return
```

Step 5 is mandatory. The CM runtime waits for the trailers future to resolve before completing the HTTP transaction. Dropping `trailers_tx` without writing causes the runtime to receive a cancelled future, which is a protocol error.

The most common case — no trailers — uses:

```wado
trailers_tx.write(Result::<Option<Trailers>, ErrorCode>::Ok(null));
```

`Response::new` also returns a `Future<Result<(), ErrorCode>>` representing body completion. For bodyless responses (second argument `null`), this future resolves immediately and can be discarded.

### 4. Error Responses

Return `Err(ErrorCode)` to indicate an HTTP-level error. No `Response` object or trailers future is needed:

```wado
export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    task return Result::<Response, ErrorCode>::Err(
        ErrorCode::InternalError(Option::<String>::Some("something went wrong"))
    );
    // No trailers_tx to fulfill — handler is done
}
```

The CM runtime maps `ErrorCode` variants to appropriate HTTP error responses.

### 5. Outbound HTTP (`wasi:http/client`)

`Client::send` is the outbound HTTP function, available as an import in the `wasi:http/service` and `wasi:http/middleware` worlds:

```wado
// Signature (from wasi/http.wado)
async fn send(request: Request) -> Result<Response, ErrorCode>;
```

`send` is an `async fn` import, meaning the CM runtime suspends the caller while the outbound request is in flight and resumes it with the result. This integrates naturally with Wado's colorless async model.

Currently, calling `Client::send` from Wado is not yet implemented. The type and import are generated into `wasi/http.wado`, but the CM import adapter for async function imports is not yet synthesized.

### 6. Middleware World

`wasi:http/middleware` exports `handle` (same signature as the service world) and additionally imports `Handler::handle` — allowing a middleware component to forward requests to the next handler in the chain.

This world is not yet supported as a compilation target. The `wasi:http/service` world is used for all current HTTP programs.

### 7. Deployment Model and Threat Boundary

`wado serve` targets managed serverless platforms — Google Cloud Run, AWS Lambda, and equivalents. It is not designed to be exposed directly at the network edge.

On these platforms the client TCP connection is terminated by the platform's front layer, not by `wado serve`:

- AWS Lambda (API Gateway / Function URL): the front layer buffers the request and response, so the guest never sees a raw client socket. The invocation is additionally bounded by a hard execution timeout (15 minutes maximum).
- Cloud Run: the Google Front End terminates the client connection and enforces a per-request timeout (5 minutes default, 60 minutes maximum).

Slow-client attacks (Slowloris, slow-read) are therefore outside `wado serve`'s threat model. The platform fronts the connection and its hard timeout is an unconditional backstop, so a slow client cannot pin a worker resource for longer than the platform allows; defending against slow clients is the platform's responsibility. A throughput floor and a per-IP connection limit were both considered for `wado serve` itself and rejected: against an attacker operating just above any safely-settable threshold they are mitigation only, and the platform front already owns this defense.

Liveness, however, is a separate property that `wado serve` must guarantee on its own — a worker stuck forever is a correctness bug, not a tolerated denial of service. The response body pump can stall in exactly one way: a consumer that holds the connection open but stops reading it. The connection is never closed, so hyper never drops the body channel, so `frame_tx.send` blocks forever (issue #1138). The pump already exits when the connection _closes_ — that drops the channel and the send fails — but nothing ends a connection that simply stays open and undrained. So the pump bounds every `frame_tx.send` with the per-request `--timeout`: a stalled consumer is reclaimed within that window and the worker self-heals. This is a liveness guarantee, not a DoS countermeasure. It is load-bearing when `wado serve` runs standalone — `wado serve file.wado` is a plain `0.0.0.0:8080` server with no platform front and no external timeout, so the per-frame timeout is the only thing that ends the hang. On the managed platforms it is defense-in-depth: the platform timeout would eventually close the connection and free the worker, but `--timeout` reclaims it sooner and on a value the operator controls.

`--max-concurrency` independently bounds total resource use: it caps concurrently in-flight requests and sizes the pooling allocator's fiber-stack pool, so worst-case resource use is bounded by construction rather than growing without limit.

Operator guidance: set `--timeout` at or below the platform's request/execution timeout. `wado serve` then returns a clean `504` and self-heals the worker before the platform cuts the connection.

## Type Reference

All types are imported from `"wasi:http"`.

### Resources

| Type             | Description                                                            |
| ---------------- | ---------------------------------------------------------------------- |
| `Request`        | Incoming HTTP request (method, path, scheme, authority, headers, body) |
| `Response`       | HTTP response (status code, headers, body, trailers)                   |
| `Fields`         | HTTP header or trailer fields (mutable key-value map)                  |
| `RequestOptions` | Request configuration (connect timeout, byte timeouts)                 |

### Type Aliases

| Type         | Base       | Description                            |
| ------------ | ---------- | -------------------------------------- |
| `FieldName`  | `String`   | Header/trailer field name              |
| `FieldValue` | `List<u8>` | Header/trailer field value (raw bytes) |
| `Headers`    | `Fields`   | Request or response headers            |
| `Trailers`   | `Fields`   | HTTP trailers                          |
| `StatusCode` | `u16`      | HTTP status code                       |

### Variants

| Type                  | Description                                                                                                   |
| --------------------- | ------------------------------------------------------------------------------------------------------------- |
| `Method`              | HTTP methods: `Get`, `Head`, `Post`, `Put`, `Delete`, `Connect`, `Options`, `Trace`, `Patch`, `Other(String)` |
| `Scheme`              | URI scheme: `Http`, `Https`, `Other(String)`                                                                  |
| `ErrorCode`           | HTTP error (42 cases: `DnsTimeout`, `ConnectionRefused`, `InternalError(Option<String>)`, etc.)               |
| `HeaderError`         | Header mutation errors: `Immutable`, `InvalidSyntax`                                                          |
| `RequestOptionsError` | Timeout configuration errors                                                                                  |

### `Fields` Methods

| Method                        | Signature                                  | Description                             |
| ----------------------------- | ------------------------------------------ | --------------------------------------- |
| `Fields::new()`               | `-> Fields`                                | Create empty mutable fields             |
| `Fields::from_list(e)`        | `-> Result<Fields, HeaderError>`           | Create from `[[FieldName, FieldValue]]` |
| `fields.get(name)`            | `-> List<FieldValue>`                      | Get all values for a name               |
| `fields.has(name)`            | `-> bool`                                  | Check if name exists                    |
| `fields.set(name, vals)`      | `-> Result<(), HeaderError>`               | Replace all values for a name           |
| `fields.append(name, v)`      | `-> Result<(), HeaderError>`               | Append a value to a name                |
| `fields.delete(name)`         | `-> Result<(), HeaderError>`               | Remove all values for a name            |
| `fields.get_and_delete(name)` | `-> Result<List<FieldValue>, HeaderError>` | Get and remove                          |
| `fields.copy_all()`           | `-> List<[FieldName, FieldValue]>`         | Return all entries as a flat list       |
| `fields.clone()`              | `-> Fields`                                | Deep copy                               |

### `Request` Methods

| Method                                           | Signature                                                      | Description                 |
| ------------------------------------------------ | -------------------------------------------------------------- | --------------------------- |
| `Request::new(headers, body, trailers, options)` | `-> [Request, Future<Result<(), ErrorCode>>]`                  | Create outbound request     |
| `request.get_method()`                           | `-> Method`                                                    | Get HTTP method             |
| `request.set_method(method)`                     | `-> Result<(), ()>`                                            | Set HTTP method             |
| `request.get_path_with_query()`                  | `-> Option<String>`                                            | Get path + query string     |
| `request.set_path_with_query(p)`                 | `-> Result<(), ()>`                                            | Set path + query string     |
| `request.get_scheme()`                           | `-> Option<Scheme>`                                            | Get URI scheme              |
| `request.set_scheme(scheme)`                     | `-> Result<(), ()>`                                            | Set URI scheme              |
| `request.get_authority()`                        | `-> Option<String>`                                            | Get authority (`host:port`) |
| `request.set_authority(a)`                       | `-> Result<(), ()>`                                            | Set authority               |
| `request.get_headers()`                          | `-> Headers`                                                   | Get request headers         |
| `request.get_options()`                          | `-> Option<RequestOptions>`                                    | Get request options         |
| `Request::consume_body(req, res)`                | `-> [Stream<u8>, Future<Result<Option<Trailers>, ErrorCode>>]` | Move body out of request    |

### `Response` Methods

| Method                                   | Signature                                                      | Description               |
| ---------------------------------------- | -------------------------------------------------------------- | ------------------------- |
| `Response::new(headers, body, trailers)` | `-> [Response, Future<Result<(), ErrorCode>>]`                 | Create response           |
| `response.get_status_code()`             | `-> StatusCode`                                                | Get HTTP status code      |
| `response.set_status_code(c)`            | `-> Result<(), ()>`                                            | Set HTTP status code      |
| `response.get_headers()`                 | `-> Headers`                                                   | Get response headers      |
| `Response::consume_body(res, done)`      | `-> [Stream<u8>, Future<Result<Option<Trailers>, ErrorCode>>]` | Move body out of response |

### `Client` Functions

| Function                | Signature                              | Description                |
| ----------------------- | -------------------------------------- | -------------------------- |
| `Client::send(request)` | `async -> Result<Response, ErrorCode>` | Send outbound HTTP request |

### CM Async Primitives

`Future<T>`, `FutureWritable<T>`, `Stream<T>`, and `StreamWritable<T>` are Component Model
primitives used throughout the HTTP API. Their resource declarations, canonical attributes,
and error handling semantics are defined in
[WEP: Redesign Wasm CM Builtins as Resource Canonical Attributes](wep-2026-03-01-cm-resource-canonical-attrs.md).

## E2E Test Fixtures

HTTP test fixtures are in `wado-compiler/tests/fixtures/http-*.wado`. Each has a `__DATA__` section with `"wasi:http/service": {...}`.

| Fixture                        | Description                                              |
| ------------------------------ | -------------------------------------------------------- |
| `http_200.wado`                | 200 OK with empty body and no trailers                   |
| `http_400.wado`                | 400 Bad Request via `set_status_code`                    |
| `http_500.wado`                | 500 Internal Server Error via `set_status_code`          |
| `http_error_code.wado`         | `Err(ErrorCode::InternalError(null))`                    |
| `http_error_code_payload.wado` | `Err(ErrorCode::InternalError(Some("...")))`             |
| `http_fields.wado`             | `Fields`: `new`, `has`, `append`, `delete`, `clone`      |
| `http_future_new.wado`         | `Future::<T>::new()` returns `[rx, tx]` pair             |
| `http_request_method.wado`     | `request.get_method()` returns injected method           |
| `http_request_path.wado`       | `request.get_path_with_query()` returns injected path    |
| `http_response_headers.wado`   | Response headers visible to caller via `headers_contain` |
| `http_response_ops.wado`       | `Response::new`, `get_status_code`, `set_status_code`    |

Stream body fixtures are in `wado-compiler/tests/fixtures/stream-http-*.wado`:

| Fixture                                | Description                                                        |
| -------------------------------------- | ------------------------------------------------------------------ |
| `stream_http_response_body.wado`       | Response body via `Stream<u8>` with `StreamWritable::write()`      |
| `stream_http_response_body_multi.wado` | Multi-chunk response body                                          |
| `stream_http_read_request_body.wado`   | Read request body via `Request::consume_body` and `Stream::read()` |
| `stream_http_echo.wado`                | Echo handler: read request body, write it back as response body    |

## Not Yet Implemented

- **Outbound requests**: The CM import adapter for `async fn` imports is not yet synthesized; calling `Client::send` will fail to compile.
- **Middleware world**: `wasi:http/middleware` is not yet supported as a target world.
- **Request construction**: Building a `Request` for outbound use requires `Request::new`.
- **Custom trailers**: Sending non-empty trailers requires constructing a `Fields` and passing it via the trailers future.

## Consequences

### Language

- `export async fn` is a new function modifier. It is only meaningful at the CM boundary — Wado's internal model is colorless and needs no async annotation.
- `task return expr;` is a new statement that is only valid in `export async fn` bodies. Regular `return` is forbidden in `async fn` bodies.
- The only current use case for `export async fn` and `task return` is the `wasi:http/service` handler. Future worlds that require CM async exports will use the same mechanism.

### Compiler

- The synthesis phase (CM binding generation) synthesizes a different adapter for `export async fn` vs `export fn`. The async adapter lifts parameters only; it does not wrap the return value. Instead, `task return` statements in the user function body are expanded inline into `task.return` calls. See [WEP: TIR-Level CM Binding Synthesis](wep-2026-02-15-cm-binding-synthesis.md).
- `return` in `async fn` is a compile error, detected during desugaring or type-checking.

### Runtime

- The `wasi:http/service` world requires wasmtime with `wasi-http` P3 support (v41+).
- Trailers futures must always be fulfilled. Dropping `FutureWritable<T>` without writing causes a runtime protocol error.

## References

- WASI HTTP WIT: `vendor/wasmtime/crates/wasi-http/src/p3/wit/deps/http.wit`
- Wado HTTP types: `wado-compiler/lib/wasi/http.wado`
- [WEP: TIR-Level CM Binding Synthesis](wep-2026-02-15-cm-binding-synthesis.md)
- [WEP: Redesign Wasm CM Builtins as Resource Canonical Attributes](wep-2026-03-01-cm-resource-canonical-attrs.md)
- [WEP: Target WASI P3 Only](wep-2026-01-11-wasi-p3-only.md)
- WASI HTTP types: `wasi:http/types@0.3.0-rc-2026-01-06`
- WASI HTTP handler: `wasi:http/handler@0.3.0-rc-2026-01-06`
