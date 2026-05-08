# WEP: HTTP Path Router (`core:router`)

## Context

Wado programs that handle HTTP requests via `wasi:http/service` need to dispatch incoming requests to handlers based on `(method, path)`. Today every author rolls their own — a `match` ladder, a hand-written `if`/`else if` chain, or an inline trie. A standard router lets every Wado HTTP service ship with a fast, correct, parameterized matcher out of the box.

### Scope

In:

- Static segments (`/api/v1/health`)
- Single-segment parameters (`/users/:id`)
- Trailing wildcard (`/static/*path`)
- HTTP method dispatch with 404 vs. 405 distinction
- `wasi:http::Request` adapter

Out (deferred until a real Wado service hits the limit):

- Constrained / typed parameters (`:id<\d+>`, `:slug<[a-z0-9-]+>`)
- Subdomain / hostname routing
- Mid-segment splits (`/users-:id`)
- Streaming / partial match

### Prior Art

| Router                 | Algorithm                      | Param style |
| ---------------------- | ------------------------------ | ----------- |
| Go `httprouter`, `gin` | Radix tree                     | `:name`     |
| Rust `matchit` (axum)  | Radix tree                     | `{name}`    |
| Rust `actix-web`       | Compiled state machine + regex | `{name}`    |
| Node `find-my-way`     | Radix tree                     | `:name`     |

Wado follows the `httprouter` / `matchit` syntax (`:name`, `*name`).

### Experimental Validation

The algorithm choice is informed by `example/router-*.wado`, which implements three engines (Linear / Radix tree / segment-level tagged DFA) against an identical 100-route fixture (70 static, 25 parametric, 5 wildcard, clustered under `/api/v1/...`) and runs them through a 10-element access pattern (9 hits + 1 miss).

Final numbers (debug `wasmtime`, Wado `-O2`, 50000 lookups):

| Engine                   | Total time     | Per-lookup |
| ------------------------ | -------------- | ---------- |
| Linear (mixed)           | 34027 ms       | 681 µs     |
| Linear (best, 1st route) | 571 ms / 50K   | 11 µs      |
| Linear (worst, miss)     | 48649 ms / 50K | 973 µs     |
| **Radix tree**           | **2376 ms**    | **48 µs**  |
| **Segment DFA**          | **2293 ms**    | **46 µs**  |

Radix and DFA are within measurement noise of each other.

### Why Segment DFA

Given speed parity, the choice is driven by capability and maintenance:

| Aspect                           | Radix tree                        | Segment DFA                     |
| -------------------------------- | --------------------------------- | ------------------------------- |
| Build cost (100 routes, debug)   | 77 ms                             | 21 ms (~3.5× faster)            |
| Code size                        | ~180 lines                        | ~150 lines                      |
| Hot-path branches                | recursive, with backtracking      | iterative loop, no backtracking |
| State-graph readability          | byte-level edges, hard to dump    | segment-level, dump-friendly    |
| Future per-state instrumentation | awkward (edges may span segments) | natural (segment = transition)  |

The segment-level model matches "how authors think about routes" (segment → segment → handler) and is the easier shape to extend (typed params, debug introspection, code-gen DFA).

Radix tree is still recommended as a separate `core:collections::PrefixTree<V>` for general byte-prefix indexing, where its byte-level granularity is genuinely useful.

## Decision

### Module: `core:router`

```wado
use { Router, RouteMatch } from "core:router";
use { Method } from "wasi:http";   // also re-exported as core:router::Method
```

The router is generic over the handler type `H`. Typical choices:

- `i32` (handler id, user-managed dispatch table)
- `fn(Request) -> Result<Response, ErrorCode>` (synchronous handler)
- `async fn(Request) -> Result<Response, ErrorCode>` (CM-async handler)
- A user-defined struct or variant

### Pattern Syntax

```
"/api/v1/users"                  static
"/api/v1/users/:id"              parameter
"/api/v1/users/:id/posts/:pid"   multiple parameters (each captures one segment)
"/static/*path"                  trailing wildcard (must be last)
"/"                              the root / home route
```

Rules:

- A leading `/` is required. Patterns without one panic at registration.
- A `:` segment must contain a non-empty parameter name.
- A `*` segment must contain a non-empty parameter name and be the last segment.
- Empty segments are rejected: a trailing `/` (e.g. `/foo/`) or doubled `/` (e.g. `/a//b`) panics at registration. Path matching is correspondingly strict on trailing slashes — `/users` and `/users/` are different routes — and `/` is the canonical root, matching the empty-segment pattern `"/"`.
- Param and wildcard names must agree across registrations that share the same node. `/users/:id` followed by `/users/:name/profile` panics; reuse the same name (`:id` in both) instead.

### Types

```wado
/// Result of a successful match. `params` is the captured parameter values
/// keyed by parameter name; for a wildcard route the captured tail is
/// stored under the wildcard name.
pub struct RouteMatch<H> {
    pub handler: H,
    pub params: TreeMap<String, String>,
}

/// Generic HTTP path router.
pub struct Router<H> { /* private */ }
```

### `Router<H>` Methods

```wado
impl<H> Router<H> {
    /// Constructs an empty router.
    pub fn new() -> Router<H>

    /// Registers a handler for the given (method, pattern).
    /// Last-write-wins: re-registering the same (method, pattern) replaces
    /// the previous handler. Panics on a malformed pattern (empty,
    /// missing leading `/`, trailing-wildcard not last, empty param/wildcard
    /// name).
    pub fn route(&mut self, method: Method, pattern: String, handler: H) with stores[handler]

    /// Method-specific shorthands. Each delegates to `route(Method::X, ...)`.
    /// Provided for `Get`, `Post`, `Put`, `Patch`, `Delete`, `Options`.
    /// `Head`, `Connect`, `Trace`, and custom `Other(_)` methods go through
    /// `route()`.
    pub fn get(&mut self, pattern: String, handler: H)     with stores[handler]
    pub fn post(&mut self, pattern: String, handler: H)    with stores[handler]
    pub fn put(&mut self, pattern: String, handler: H)     with stores[handler]
    pub fn patch(&mut self, pattern: String, handler: H)   with stores[handler]
    pub fn delete(&mut self, pattern: String, handler: H)  with stores[handler]
    pub fn options(&mut self, pattern: String, handler: H) with stores[handler]

    /// Registers a handler that matches any method (including `Method::Other(_)`)
    /// at `pattern`. Specific-method handlers at the same pattern take
    /// precedence; `any` is the fallback when no specific entry matched.
    /// Same panic rules as `route()`.
    pub fn any(&mut self, pattern: String, handler: H) with stores[handler]

    /// Matches a method/path pair. Returns `Some(RouteMatch)` on a hit and
    /// `None` on a miss. The path argument must be the URL path only (no
    /// query string, no fragment).
    pub fn match_path(&self, method: Method, path: &String) -> Option<RouteMatch<H>>

    /// If `path` matches a registered route under any specific method, returns
    /// the list of those methods. `any` handlers do not contribute (they are
    /// uncountable). Useful for emitting the `Allow:` header on a 405
    /// response. Returns an empty array on a genuine 404 or when only an
    /// `any` handler is registered (in which case 405 is unreachable).
    pub fn allowed_methods(&self, path: &String) -> Array<Method>

    /// Matches against a `wasi:http` `Request`, splitting the path from the
    /// query string internally. Recommended entry point for HTTP services.
    pub fn match_request(&self, request: &Request) -> Option<RouteMatch<H>>
}
```

### Usage Examples

#### Handler-id style

```wado
use { Router } from "core:router";
use { Method, Request, Response, ErrorCode } from "wasi:http";

global ROUTER: Router<i32> = build_router();

fn build_router() -> Router<i32> {
    let mut r = Router::<i32>::new();
    r.route(Method::Get,    "/health",                 0);
    r.route(Method::Get,    "/api/v1/users",           1);
    r.route(Method::Post,   "/api/v1/users",           2);
    r.route(Method::Get,    "/api/v1/users/:id",       3);
    r.route(Method::Delete, "/api/v1/users/:id",       4);
    r.route(Method::Get,    "/static/*path",           5);
    return r;
}

export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    if let Some(m) = ROUTER.match_request(&request) {
        return match m.handler {
            0 => respond_health(),
            1 => respond_users_list(),
            2 => respond_user_create(&request),
            3 => respond_user_get(m.params["id"]),
            4 => respond_user_delete(m.params["id"]),
            5 => respond_static(m.params["path"]),
            _ => unreachable(),
        };
    }
    let allowed = ROUTER.allowed_methods(&request.get_path_with_query().unwrap_or("/"));
    if !allowed.is_empty() {
        return Result::Ok(method_not_allowed(allowed));   // 405 with Allow:
    }
    return Result::Ok(not_found());                       // 404
}
```

#### Closure-handler style

```wado
type Handler = fn(&Request, &TreeMap<String, String>) -> Response with Stdout;

let mut r = Router::<Handler>::new();
r.route(Method::Get, "/health",   |_req, _params| Response::ok("ok"));
r.route(Method::Get, "/users/:id", |_req, params| Response::ok(`user {params["id"]}`));
```

(For the CM-async case, the same shape works with `async fn(...) -> ...` once Wado closures support async — orthogonal to this WEP.)

### Lookup Precedence

At each state during traversal, transitions are tried in this fixed order; there is no backtracking:

1. Literal — the segment exactly equals one of the registered literal edges
2. Parameter — there is a `:name` child; the current segment is captured and traversal descends
3. Wildcard — there is a `*name` child; the entire remainder of the path is captured

Once a terminal or wildcard is reached, method dispatch follows this order:

1. Specific method — the request's method is found in the terminal's specific-method list
2. `any` — the terminal has an `any`-registered handler (registered via `Router::any`)
3. Miss — return `None` (caller emits 405 if `allowed_methods(path)` is non-empty, otherwise 404)

This means that `/users/list` registered alongside `/users/:id` will route exactly `/users/list` to the literal handler and any other single-segment value to the param handler. A path like `/users/list/extra` is **not** automatically re-tried against the param branch; it is a 404 unless `/users/:id/extra` is also registered.

This is the same rule used by `httprouter`, `gin`, `echo`, and `matchit`. The alternative — backtracking from a literal dead-end into the param branch — produces surprising matches where a literal "wins" at one level but later fails and silently falls into a `:id` capture that swallows the literal name as the id.

### Method Handling

Each terminal stores a small list of `(Method, H)` entries. Registering `(Get, "/users", h1)` and `(Post, "/users", h2)` produces two entries at the same terminal; `match_path(Get, ...)` returns `h1`, `match_path(Post, ...)` returns `h2`, and `match_path(Put, ...)` returns `None` while `allowed_methods("/users")` returns `[Get, Post]`.

In addition, each terminal carries a single optional `any` handler (likewise for wildcard nodes). `Router::any(pattern, h)` populates that slot. Specific-method entries are checked first; on a miss the `any` handler is dispatched. This lets services like a `httpbin`-style `/anything` accept every HTTP method, including non-standard ones (`Method::Other("PURGE")`), without enumerating them at registration time.

### Implementation Notes

The implementation lives in `wado-compiler/lib/core/router.wado` and is ported from `example/router_dfa.wado`, with the following changes:

- `RouteMatch` is generic over `H`; the example uses a concrete `i32 handler_id`.
- A `handlers: Array<H>` arena is added to `Router<H>`; terminal states reference handlers by `i32` index. This keeps `DfaState` non-generic, which avoids the monomorphization blow-up of including `H` inside every state.
- `route()` validates patterns and panics on malformed input.
- Method dispatch supports multiple methods per terminal via an `Array<MethodEntry>` per terminal, scanned linearly on lookup (per-terminal method count is in the single digits, so a sorted array + binary search is overkill). (The example assumes one method per terminal.)
- Each terminal and wildcard node carries an extra `any_handler_idx: i32` (`-1` when absent) for `Router::any` dispatch.
- A `match_request` adapter handles path/query split.

```
wado-compiler/lib/core/router.wado        Implementation (~250 lines)
wado-compiler/lib/core/router_test.wado   Unit tests (re-uses cases from the example)
example/router-*.wado                     Retained as a competing-algorithm benchmark
```

## Consequences

### Pros

- A single canonical, fast HTTP router ships with the language.
- Uniform pattern syntax across the Wado ecosystem (no third-party divergence on `:name` vs `{name}`).
- The DFA structure is amenable to a future build-time codegen: a constant set of routes can be lowered into a generated `match` cascade with zero runtime construction. The runtime API stays the same.
- Shared between `wasi:http` (server side) and HTTP client SPAs (e.g., reactive routers built on `core:url` + `core:router`).

### Cons

- Routes are not validated at compile time. A typo in a pattern string only surfaces at `Router::new` time. A future `#routes!` form could lift this to compile time.

### Future Work

- [ ] Constrained parameters: `:id<\d+>`, `:slug<[a-z0-9-]+>`
- [ ] Build-time route compilation: `#![generated]` DFA tables produced from a TOML/Wado route manifest
- [ ] `core:collections::PrefixTree<V>` for the byte-level radix algorithm, separated from HTTP routing
- [ ] OpenAPI / Swagger introspection: dump the route table as JSON for documentation tooling

## See Also

- [WEP: URL Standard Library (`core:url`)](./wep-2026-04-10-url-stdlib.md)
- [WEP: WASI HTTP Integration](./wep-2026-02-21-wasi-http.md)
- `example/router-*.wado` — competing-algorithm benchmark
