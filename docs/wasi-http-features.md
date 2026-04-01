# WASI HTTP Implementation Status

Tracks whether each `wasi:http` feature is implemented and working, verified by passing e2e tests.

- [x] = implemented and verified (all `Option`/`Result` variants pass)
- [ ] = not yet working (bug, timeout, or missing implementation)

Items without e2e test coverage are listed in [Untested Features](#untested-features).

## Effects

### Handler

- [x] `async fn handle(request: Request) -> Result<Response, ErrorCode>` — Ok and Err both verified

### Client

- [x] `async fn send(request: Request) -> Result<Response, ErrorCode>` — Ok verified

## Resources

### Fields

- [x] `fn new() -> Fields`
- [x] `fn from_list(entries) -> Result<Fields, HeaderError>` — Ok and Err(InvalidSyntax) verified
- [x] `fn get(name) -> Array<FieldValue>` — existing and non-existing names
- [x] `fn has(name) -> bool` — true and false
- [x] `fn set(name, value) -> Result<(), HeaderError>` — Ok and Err(immutable) verified
- [x] `fn delete(name) -> Result<(), HeaderError>` — Ok and Err(immutable) verified
- [x] `fn get_and_delete(name) -> Result<Array<FieldValue>, HeaderError>` — Ok and Err(immutable) verified
- [x] `fn append(name, value) -> Result<(), HeaderError>` — Ok and Err(immutable) verified
- [x] `fn copy_all() -> Array<[FieldName, FieldValue]>`
- [x] `fn clone() -> Fields`

### Request

- [ ] `fn new(headers, contents, trailers, options)` — contents=Some(Stream) times out at O0/O2; other combinations work
- [x] `fn get_method() -> Method`
- [x] `fn set_method(method) -> Result<(), ()>` — Ok verified; Err not triggerable (wasmtime accepts all methods)
- [x] `fn get_path_with_query() -> Option<String>` — Some and None verified
- [x] `fn set_path_with_query(path) -> Result<(), ()>` — Ok(Some) and Ok(None) verified; Err not triggerable
- [x] `fn get_scheme() -> Option<Scheme>` — Some and None verified
- [x] `fn set_scheme(scheme) -> Result<(), ()>` — Ok(Some) and Ok(None) verified; Err not triggerable
- [x] `fn get_authority() -> Option<String>` — Some and None verified
- [x] `fn set_authority(authority) -> Result<(), ()>` — Ok(Some) and Ok(None) verified; Err not triggerable
- [x] `fn get_options() -> Option<RequestOptions>` — Some and None verified
- [x] `fn get_headers() -> Headers`
- [x] `fn consume_body(this, res) -> [Stream<u8>, Future<...>]`

### RequestOptions

- [x] `fn new() -> RequestOptions`
- [x] `fn get_connect_timeout() -> Option<Duration>` — Some and None verified
- [x] `fn set_connect_timeout(duration) -> Result<(), RequestOptionsError>` — Ok(Some), Ok(None), Err(Immutable) verified
- [x] `fn get_first_byte_timeout() -> Option<Duration>` — Some and None verified
- [x] `fn set_first_byte_timeout(duration) -> Result<(), RequestOptionsError>` — Ok verified; Err covered via Immutable path
- [x] `fn get_between_bytes_timeout() -> Option<Duration>` — Some and None verified
- [x] `fn set_between_bytes_timeout(duration) -> Result<(), RequestOptionsError>` — Ok verified; Err covered via Immutable path
- [x] `fn clone() -> RequestOptions`

### Response

- [x] `fn new(headers, contents, trailers)` — contents=None and contents=Some(Stream) both verified
- [x] `fn get_status_code() -> StatusCode`
- [x] `fn set_status_code(status_code) -> Result<(), ()>` — Ok and Err (invalid codes 0, 65535) verified
- [x] `fn get_headers() -> Headers`
- [x] `fn consume_body(this, res) -> [Stream<u8>, Future<...>]`

## Variants

### Method

- [x] Get, Head, Post, Put, Delete, Connect, Options, Trace, Patch, Other(String)

### Scheme

- [x] Http, Https, Other(String)

### ErrorCode

- [x] All 40 variants — construction, matching, and Err return from handler verified

### HeaderError

- [x] All 5 variants constructed
- [x] Returned as Err from immutable Fields operations
- [ ] Variant matching (`e matches { Immutable }`) — panics at O0/O2; `Err(_)` works

### RequestOptionsError

- [x] All 3 variants constructed
- [x] Returned as Err from immutable RequestOptions set operations

## Structs

- [x] DnsErrorPayload — both Some/None fields
- [x] TlsAlertReceivedPayload — both Some/None fields
- [x] FieldSizePayload — both Some/None fields

## Type Aliases

- [x] FieldName, FieldValue, Headers, Trailers, StatusCode

## Patterns

### Server

- [x] Minimal response, status codes, path routing, method routing
- [x] Header echo, error code return
- [x] Stream body write (single/multi chunk, string conversion)
- [x] Request body read, echo handler
- [x] Response with actual trailers

### Client

- [x] Simple send + forward, read upstream body, reverse proxy
- [x] Multiple sequential requests, response header reading
- [x] Send with RequestOptions
- [ ] Send with request body stream — times out at O0/O2

### Worlds

- [x] `wasi:http/service`
- [ ] `wasi:http/middleware` — compiler panics when Handler effect is used in service world

## Untested Features

These features have no e2e test due to test infrastructure limitations:

- `Client::send` Err(ErrorCode) variant — mock system traps on unmatched paths instead of returning ErrorCode
- `Request.set_method` Err variant — wasmtime accepts all method values including empty strings
- `Request.set_path_with_query` / `set_scheme` / `set_authority` Err variants — wasmtime accepts all values tested

## Known Bugs

- [ ] `Request.new` with `contents=Some(Stream<u8>)` for outgoing requests times out at O0/O2 (`http-client-send-with-body`)
- [ ] `HeaderError` variant matching (`e matches { Immutable }`) panics at O0/O2; workaround: use `Err(_)` (`http-fields-immutable-errors`)
- [ ] `Handler` effect in service world causes compiler panic instead of proper error (`http-middleware-forward`)
