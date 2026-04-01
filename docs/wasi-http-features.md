# WASI HTTP E2E Test Coverage

Tracks e2e fixture coverage for every method, type, and variant in `wasi:http`.

Criteria: a method is checked only when (1) the method is called in an e2e fixture AND (2) all `Option`/`Result` variants in its signature are exercised.

## Effects

### Handler

- [x] `async fn handle(request: Request) -> Result<Response, ErrorCode>` — Ok: many fixtures, Err: `http-error-code*.wado`

### Client

- [ ] `async fn send(request: Request) -> Result<Response, ErrorCode>` — Ok: `http-client-send-*.wado`, Err: not tested

## Resources

### Fields

- [x] `fn new() -> Fields`
- [ ] `fn from_list(entries) -> Result<Fields, HeaderError>` — Ok: `http-fields-from-list`, Err: not tested
- [x] `fn get(name) -> Array<FieldValue>` — existing and non-existing names tested
- [x] `fn has(name) -> bool` — true and false tested
- [ ] `fn set(name, value) -> Result<(), HeaderError>` — Ok: `http-fields-set`, Err: not tested
- [ ] `fn delete(name) -> Result<(), HeaderError>` — Ok: `http-fields`, Err: not tested
- [ ] `fn get_and_delete(name) -> Result<Array<FieldValue>, HeaderError>` — Ok: `http-fields-get-and-delete`, Err: not tested
- [ ] `fn append(name, value) -> Result<(), HeaderError>` — Ok: many fixtures, Err: not tested
- [x] `fn copy_all() -> Array<[FieldName, FieldValue]>` — `http-fields-copy-all`
- [x] `fn clone() -> Fields` — `http-fields`

### Request

- [ ] `fn new(headers, contents, trailers, options)` — contents=None: tested, contents=Some(Stream): not tested, options=None: tested, options=Some(RequestOptions): not tested
- [x] `fn get_method() -> Method` — `http-request-method*.wado`, `http-method-routing`
- [ ] `fn set_method(method) -> Result<(), ()>` — not called anywhere
- [ ] `fn get_path_with_query() -> Option<String>` — Some: `http-request-path`, None: not tested
- [ ] `fn set_path_with_query(path) -> Result<(), ()>` — Ok: `http-client-*.wado`, Err: not tested
- [ ] `fn get_scheme() -> Option<Scheme>` — Some: `http-request-scheme`, None: not tested
- [ ] `fn set_scheme(scheme) -> Result<(), ()>` — Ok: `http-client-*.wado`, Err: not tested
- [ ] `fn get_authority() -> Option<String>` — Some: `http-request-authority`, None: not tested
- [ ] `fn set_authority(authority) -> Result<(), ()>` — Ok: `http-client-*.wado`, Err: not tested
- [ ] `fn get_options() -> Option<RequestOptions>` — not called anywhere
- [x] `fn get_headers() -> Headers` — `http-request-headers`, `http-echo-headers`, `http-client-advanced`
- [x] `fn consume_body(this, res) -> [Stream<u8>, Future<...>]` — `stream-http-echo`, `stream-http-read-request-body`

### RequestOptions

- [x] `fn new() -> RequestOptions`
- [ ] `fn get_connect_timeout() -> Option<Duration>` — Some: tested, None: tested (but through set/get round-trip only, never from incoming request)
- [ ] `fn set_connect_timeout(duration) -> Result<(), RequestOptionsError>` — Ok(Some): tested, Ok(None): tested, Err: not tested
- [ ] `fn get_first_byte_timeout() -> Option<Duration>` — Some: tested, None: tested
- [ ] `fn set_first_byte_timeout(duration) -> Result<(), RequestOptionsError>` — Ok: tested, Err: not tested
- [ ] `fn get_between_bytes_timeout() -> Option<Duration>` — Some: tested, None: tested
- [ ] `fn set_between_bytes_timeout(duration) -> Result<(), RequestOptionsError>` — Ok: tested, Err: not tested
- [x] `fn clone() -> RequestOptions`

### Response

- [x] `fn new(headers, contents, trailers)` — contents=None and contents=Some(Stream) both tested
- [x] `fn get_status_code() -> StatusCode` — `http-response-ops`, `http-client-send-proxy`
- [ ] `fn set_status_code(status_code) -> Result<(), ()>` — Ok: many fixtures, Err: not tested
- [x] `fn get_headers() -> Headers` — `http-response-get-headers`, `http-client-advanced`
- [x] `fn consume_body(this, res) -> [Stream<u8>, Future<...>]` — `http-client-send-body-read`

## Variants

### Method

- [x] Get, Head, Post, Put, Delete, Connect, Options, Trace, Patch — `http-method-and-scheme-variants`, `http-request-method*.wado`
- [x] Other(String) — `http-method-and-scheme-variants`

### Scheme

- [x] Http, Https — `http-method-and-scheme-variants`, `http-request-scheme`
- [x] Other(String) — `http-method-and-scheme-variants`

### ErrorCode

- [x] All 40 variants constructed and matched — `http-error-code-comprehensive`
- [x] Returned as Err from handler — `http-error-code`, `http-error-code-variants`, `http-error-code-payload`

### HeaderError

- [x] All 5 variants constructed — `http-error-code-comprehensive`
- [ ] Returned as Err from Fields operations — not tested (would need immutable fields from `request.get_headers()`)

### RequestOptionsError

- [x] All 3 variants constructed — `http-error-code-comprehensive`
- [ ] Returned as Err from set operations — not tested

## Structs

- [x] DnsErrorPayload — both Some/None fields: `http-error-code-comprehensive`
- [x] TlsAlertReceivedPayload — both Some/None fields: `http-error-code-comprehensive`
- [x] FieldSizePayload — both Some/None fields: `http-error-code-comprehensive`

## Type Aliases

- [x] FieldName = String — used in all Fields operations
- [x] FieldValue = Array\<u8\> — used in all Fields operations
- [x] Headers = Fields — used as type annotation
- [x] Trailers = Fields — used as type annotation
- [x] StatusCode = u16 — used in set/get status code

## Patterns

### Server Patterns

- [x] Minimal 200 response — `http-200`
- [x] Status code setting (400, 500) — `http-400`, `http-500`
- [x] Path-based routing — `http-routing`, `http-routing-created`
- [x] Method-based routing — `http-method-routing`
- [x] Header echo (read request header → write response header) — `http-echo-headers`
- [x] Error code return — `http-error-code`, `http-error-code-variants`, `http-error-code-payload`
- [x] Stream body write (single chunk) — `stream-http-response-body`
- [x] Stream body write (multi chunk) — `stream-http-response-body-multi`
- [x] Stream body write (string conversion) — `stream-http-response-body-string`
- [x] Request body read — `stream-http-read-request-body`
- [x] Echo handler (read body → write body) — `stream-http-echo`
- [x] Response with actual trailers — `http-response-trailers`

### Client Patterns

- [x] Simple send + forward — `http-client-send-simple`
- [x] Send + read upstream body — `http-client-send-body-read`
- [x] Reverse proxy (verify + forward) — `http-client-send-proxy`
- [x] Multiple sequential requests + header reading — `http-client-advanced`
- [ ] Send with request body stream — not tested
- [ ] Send with RequestOptions — not tested
- [ ] Handle upstream error (Err from Client::send) — not tested
