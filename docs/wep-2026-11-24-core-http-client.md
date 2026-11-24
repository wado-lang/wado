# WEP: HTTP Client (`core:http_client`)

## Context

`wasi:http` is the only way out to an HTTP server, and it is low-level. A
request is a resource assembled field by field. Its body is a stream that must
be written only after the send has started, or the two sides deadlock. The
response body is a stream whose close does not say whether the body arrived
whole: a future resolved after the close says that. Eddy wraps all of this in
`package-eddy/src/http.wado`, and every other client would write it again.

Eddy is the first user. The next is an OpenAPI client generator, so the module
serves an API described per origin, not one call site. LLM APIs often stream
their answers as Server-Sent Events, so streaming is part of the first version, not a later
addition.

## Decision

### A client per origin, not a global `fetch`

An API is used through settings that belong to its origin: the base URL, the
headers every request carries (authentication, version), and the timeouts. A
global `fetch` has nowhere to keep them, so each call would repeat them, and a
generated OpenAPI client would build the struct anyway. `HttpClient` holds
them, and `request` is its method.

```wado
use { HttpClient, ClientInit, RequestInit, Body, HttpError } from "core:http_client";
use { Method } from "wasi:http";

struct Message { id: String }

fn create(key: String, payload: ByteList) -> Result<Message, HttpError> with _ {
    let client = HttpClient::new("https://api.anthropic.com", ClientInit {
        headers: [["x-api-key", key], ["anthropic-version", "2023-06-01"]],
    })?;
    let res = client.request("/v1/messages", RequestInit {
        method: Method::Post,
        body: Body::bytes(payload),
    })?;
    return res.json::<Message>();
}
```

`HttpClient` holds settings only. The host owns connections, pooling, the
protocol version (HTTP/1.1, 2 or 3) and TLS, and `wasi:http` exposes none of
them. So the client offers no field for them, since one would be a setting that
does nothing. A client is cheap to make and to drop.

The rest borrows Node's `fetch` where the meaning is the same: `RequestInit`
with defaults, `res.ok()`, `text()`, `bytes()`, and `json()`, which here decodes
into a type through `core:serde`. It differs where Wado has its own way:
failure is a `Result`, not an exception or a rejected promise; the method is
`wasi:http`'s `Method`; a timeout is a `Duration`, not an `AbortSignal`.

### The request target

`HttpClient::new` parses the base URL with `core:url` and accepts only `http`
and `https` with no query or fragment. `request` appends its path to the base
path as text. RFC 3986 resolution would let `/v1/messages` drop the base path
`/api`, which is never what a client of one API means.

`RequestInit.query` takes name-value pairs, and the client percent-encodes
them. Building a query by hand is where an OpenAPI client would most often get
encoding wrong.

Headers in `RequestInit` are appended after the client's. A name in both keeps
both values, as `Fields::append` does; replacing would need the client to know
which headers are single-valued.

### Status is not failure

A response with status 4xx or 5xx is `Ok`. An OpenAPI operation declares a
body type per status, so the caller needs the response, not an error. `Err`
means no response, or a body that did not arrive whole.

The client does not follow redirects; a 3xx comes back as it is. Following one
means sending the body again, and a streamed body cannot be sent twice.

### Errors

`HttpError` is one variant:

- An invalid URL, from `HttpClient::new` or from a path or query that makes one.
- A transport failure, with the `wasi:http` `ErrorCode` and the stage it
  happened in: before the response headers, or while reading the response
  body. Before the headers, no part of the response was seen; during the body,
  part of it was already handed to the caller. A retry policy needs the
  difference.
- A body or header value that could not be decoded: not UTF-8 for `text()` or
  `header()`, or not the type for `json()`.

### Streaming bodies

The request body is one of: none, bytes, or a writer. A writer is a function
that `request` calls with the body stream's writable end once the send has
started, and the body ends when the function returns. The caller cannot write
too early, and the deadlock above cannot happen.

The response body is read in one of three ways, each consuming the response:

- whole: `bytes()`, `text()`, `json()`;
- as chunks: `body()`, an iterator of `Result<ByteList, HttpError>`;
- as events: `events()`, an iterator of `Result<SseEvent, HttpError>`.

A body cut short by the connection ends the stream just as a complete one does;
only the future resolved after it tells them apart. So every way of reading
awaits that future at the end. A whole read returns its error. An iterator
yields it as its last item, so a caller cannot see the end without seeing the
failure. The trailers arrive in the same future, and the response offers them
once the body has been read to the end.

A response dropped before its body is read to the end tells the host so through
the `res` future `consume-body` takes.

### Server-Sent Events

The event parser is built into `core:http_client` and follows the
`text/event-stream` interpretation in WHATWG HTML § Server-sent events:

- An empty line dispatches the event. A line starting with `:` is a comment.
  A line with no colon is a field name with an empty value. One space after the
  colon is removed.
- `data` lines are joined with LF. An event with no data is not dispatched.
  `event` defaults to `message`. `id` carries over to later events and is
  ignored when it contains NUL. `retry` is taken only when it is all digits.
  Other fields are ignored.
- CR, LF and CRLF each end a line, including a CR that ends one chunk followed
  by an LF that starts the next.
- An event not closed by an empty line when the stream ends is discarded.

`SseEvent` gives `event`, `data` and `id` as `StrSlice`, and `retry` as a
number. Reconnecting with `Last-Event-ID` is EventSource behavior, not
parsing, and it belongs to retry. Protocols layered on the events (a `[DONE]`
sentinel, a JSON schema per event type) belong to the application.

### Copies

Each stream read hands over a fresh `List<u8>`, and the parser never writes to
memory a view it handed out can see. So a view stays valid for as long as the
caller holds it, and the parser copies only in these cases:

- A line split across chunks is copied once, to join it.
- An event with more than one `data` line is copied once, to join the lines
  with LF. The common case in LLM APIs, one JSON document on one `data` line, is
  a view into the chunk.
- A line that is not valid UTF-8 is decoded with U+FFFD in place of each bad
  sequence, as the standard requires, and that needs a copy.

A response header value is always a view over the bytes the host handed over,
never a copy. `header(name)` gives it as a `StrSlice`, and a value that is not
UTF-8 is an error, not a decoded copy. A copy made only for some values would
make the cost depend on what the server sent. RFC 9110 lets a value hold any
octet from 0x80 up (`obs-text`), and Fetch decodes those as Latin-1. Values in
practice are ASCII or UTF-8, so a client that must read the rest asks for the
octets with `header_bytes(name)`, a `ByteSlice`.

## Roadmap

- [ ] `HttpClient`, `RequestInit`, `Body`, `HttpError`, and the whole-body
  reads, tested against a local `wasi:http/service` server.
- [ ] Streaming request and response bodies, with a test that cuts a body short
  and expects `Err`.
- [ ] `events()`, with parser tests that split the input at every byte.
- [ ] Eddy uses `core:http_client`, and `package-eddy/src/http.wado` goes away.

## Known gaps

- Retry. A request that can be repeated exactly is rarer than it looks; a retry
  that matters widens to POST or moves to another endpoint, so another client.
  That is policy for a separate module.
- A header name sent more than once has several values. RFC 9110 reads them as
  one, joined with `, `, but joining is a copy.
- Cookies, multipart form bodies, and compression on the guest side.
- Writing Server-Sent Events from a `wasi:http/service` handler.
