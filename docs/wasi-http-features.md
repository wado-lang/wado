# WASI HTTP Implementation Status

Tracks whether each `wasi:http` feature is implemented and working, verified by passing e2e tests.

- [x] = implemented and verified (all `Option`/`Result` variants pass)
- [ ] = not yet working (bug, timeout, or missing implementation)

Items without e2e test coverage are listed in [Untested Features](#untested-features).

## Effects

### Handler

- [x] `async fn handle(request: Request) -> Result<Response, ErrorCode>` — Ok and Err both verified (`http-200`, `http-error-code`)

### Client

- [x] `async fn send(request: Request) -> Result<Response, ErrorCode>` — Ok verified (`http-client-send-simple`, `http-client-send-proxy`, `http-client-advanced`)

## Resources

### Fields

- [x] `fn new() -> Fields` (`http-fields`, `http-200`)
- [x] `fn from_list(entries) -> Result<Fields, HeaderError>` — Ok and Err(InvalidSyntax) verified (`http-fields-from-list`, `http-fields-from-list-error`)
- [x] `fn get(name) -> List<FieldValue>` — existing and non-existing names (`http-fields-set`, `http-client-advanced`)
- [x] `fn has(name) -> bool` — true and false (`http-fields`, `http-request-headers`, `http-echo-headers`)
- [x] `fn set(name, value) -> Result<(), HeaderError>` — Ok and Err(immutable) verified (`http-fields-set`, `http-fields-immutable-errors`)
- [x] `fn delete(name) -> Result<(), HeaderError>` — Ok and Err(immutable) verified (`http-fields`, `http-fields-immutable-errors`)
- [x] `fn get_and_delete(name) -> Result<List<FieldValue>, HeaderError>` — Ok and Err(immutable) verified (`http-fields-get-and-delete`, `http-fields-immutable-errors`)
- [x] `fn append(name, value) -> Result<(), HeaderError>` — Ok and Err(immutable) verified (`http-fields`, `http-echo-headers`, `http-fields-immutable-errors`)
- [x] `fn copy_all() -> List<[FieldName, FieldValue]>` (`http-fields-copy-all`, `http-fields-immutable-errors`)
- [x] `fn clone() -> Fields` (`http-fields`, `http-fields-immutable-errors`)

### Request

- [ ] `fn new(headers, contents, trailers, options)` — contents=Some(Stream) times out at O0/O2; other combinations work (`http-client-request-full`, `http-client-send-with-body`)
- [x] `fn get_method() -> Method` (`http-request-method`, `http-method-routing`, `http-method-and-scheme-variants`)
- [x] `fn set_method(method) -> Result<(), ()>` — Ok verified; Err not triggerable (wasmtime accepts all methods) (`http-client-request-full`, `http-request-setter-errors`)
- [x] `fn get_path_with_query() -> Option<String>` — Some and None verified (`http-request-path`, `http-routing`, `http-client-request-full`)
- [x] `fn set_path_with_query(path) -> Result<(), ()>` — Ok(Some) and Ok(None) verified; Err not triggerable (`http-client-request-full`, `http-request-setter-errors`)
- [x] `fn get_scheme() -> Option<Scheme>` — Some and None verified (`http-request-scheme`, `http-method-and-scheme-variants`, `http-client-request-full`)
- [x] `fn set_scheme(scheme) -> Result<(), ()>` — Ok(Some) and Ok(None) verified; Err not triggerable (`http-client-request-full`, `http-request-setter-errors`)
- [x] `fn get_authority() -> Option<String>` — Some and None verified (`http-request-authority`, `http-client-request-full`)
- [x] `fn set_authority(authority) -> Result<(), ()>` — Ok(Some) and Ok(None) verified; Err not triggerable (`http-client-request-full`, `http-request-setter-errors`)
- [x] `fn get_options() -> Option<RequestOptions>` — Some and None verified (`http-client-request-full`, `http-request-options`)
- [x] `fn get_headers() -> Headers` (`http-request-headers`, `http-echo-headers`, `http-fields-immutable-errors`)
- [x] `fn consume_body(this, res) -> [Stream<u8>, Future<...>]` (`http-client-send-body-read`, `http-client-advanced`)

### RequestOptions

- [x] `fn new() -> RequestOptions` (`http-request-options`, `http-request-options-timeouts`)
- [x] `fn get_connect_timeout() -> Option<Duration>` — Some and None verified (`http-request-options-timeouts`, `http-client-request-full`)
- [x] `fn set_connect_timeout(duration) -> Result<(), RequestOptionsError>` — Ok(Some), Ok(None), Err(Immutable) verified (`http-request-options-timeouts`, `http-client-request-full`)
- [x] `fn get_first_byte_timeout() -> Option<Duration>` — Some and None verified (`http-request-options-timeouts`)
- [x] `fn set_first_byte_timeout(duration) -> Result<(), RequestOptionsError>` — Ok verified; Err covered via Immutable path (`http-request-options-timeouts`)
- [x] `fn get_between_bytes_timeout() -> Option<Duration>` — Some and None verified (`http-request-options-timeouts`)
- [x] `fn set_between_bytes_timeout(duration) -> Result<(), RequestOptionsError>` — Ok verified; Err covered via Immutable path (`http-request-options-timeouts`)
- [x] `fn clone() -> RequestOptions` (`http-request-options-timeouts`)

### Response

- [x] `fn new(headers, contents, trailers)` — contents=None and contents=Some(Stream) both verified (`http-200`, `http-body-contains`, `http-response-trailers`)
- [x] `fn get_status_code() -> StatusCode` (`http-response-ops`, `http-client-advanced`, `http-response-set-status-invalid`)
- [x] `fn set_status_code(status_code) -> Result<(), ()>` — Ok and Err (invalid codes 0, 65535) verified (`http-400`, `http-500`, `http-response-set-status-invalid`)
- [x] `fn get_headers() -> Headers` (`http-response-get-headers`, `http-client-advanced`)
- [x] `fn consume_body(this, res) -> [Stream<u8>, Future<...>]` (`http-client-send-body-read`, `http-client-advanced`)

## Variants

### Method

- [x] Get, Head, Post, Put, Delete, Connect, Options, Trace, Patch, Other(String) (`http-method-routing`, `http-method-and-scheme-variants`, `http-request-method-*`)

### Scheme

- [x] Http, Https, Other(String) (`http-method-and-scheme-variants`, `http-request-scheme`)

### ErrorCode

- [x] All 40 variants — construction, matching, and Err return from handler verified (`http-error-code-comprehensive`, `http-error-code`, `http-error-code-variants`, `http-error-code-payload`)

### HeaderError

- [x] All 5 variants constructed (`http-error-code-comprehensive`)
- [x] Returned as Err from immutable Fields operations (`http-fields-immutable-errors`)
- [ ] Variant matching (`e matches { Immutable }`) — panics at O0/O2; `Err(_)` works (`http-fields-immutable-errors`)

### RequestOptionsError

- [x] All 3 variants constructed (`http-error-code-comprehensive`)
- [x] Returned as Err from immutable RequestOptions set operations (`http-client-request-full`)

## Structs

- [x] DnsErrorPayload — both Some/None fields (`http-error-code-comprehensive`)
- [x] TlsAlertReceivedPayload — both Some/None fields (`http-error-code-comprehensive`)
- [x] FieldSizePayload — both Some/None fields (`http-error-code-comprehensive`)

## Type Aliases

- [x] FieldName, FieldValue, Headers, Trailers, StatusCode (`http-fields-from-list`, `http-response-set-status-invalid`, `http-response-trailers`)

## Patterns

### Server

- [x] Minimal response, status codes, path routing, method routing (`http-200`, `http-400`, `http-500`, `http-routing`, `http-method-routing`)
- [x] Header echo, error code return (`http-echo-headers`, `http-error-code`)
- [x] Stream body write (single/multi chunk, string conversion) (`http-body-contains`, `http-response-trailers`)
- [x] Request body read, echo handler (`http-client-send-body-read`)
- [x] Response with actual trailers (`http-response-trailers`)

### Client

- [x] Simple send + forward, read upstream body, reverse proxy (`http-client-send-simple`, `http-client-send-proxy`, `http-client-send-body-read`)
- [x] Multiple sequential requests, response header reading (`http-client-advanced`)
- [x] Send with RequestOptions (`http-client-request-full`)
- [ ] Send with request body stream — times out at O0/O2 (`http-client-send-with-body`)

### Worlds

- [x] `wasi:http/service` (`http-200`, `http-routing`, etc.)
- [ ] `wasi:http/middleware` — compiler panics when Handler effect is used in service world (`http-middleware-forward`)

## Untested Features

These features have no e2e test due to test infrastructure limitations:

- `Client::send` Err(ErrorCode) variant — mock system traps on unmatched paths instead of returning ErrorCode (`http-client-send-error` documents this with `trapped: true`)
- `Request.set_method` Err variant — wasmtime accepts all method values including empty strings
- `Request.set_path_with_query` / `set_scheme` / `set_authority` Err variants — wasmtime accepts all values tested

## Known Bugs

- [ ] `Request.new` with `contents=Some(Stream<u8>)` for outgoing requests times out at O0/O2 (`http-client-send-with-body`)
- [ ] `HeaderError` variant matching (`e matches { Immutable }`) panics at O0/O2; workaround: use `Err(_)` (`http-fields-immutable-errors`)
- [ ] `Handler` effect in service world causes compiler panic instead of proper error (`http-middleware-forward`)
