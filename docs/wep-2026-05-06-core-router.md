# WEP: HTTP Path Router (`core:router`)

## Context

Wado programs that handle HTTP requests via `wasi:http/service` need to dispatch incoming requests to handlers based on `(method, path)`. Today every author rolls their own — a `match` ladder, a hand-written `if`/`else if` chain, or an inline trie — and gets to choose between unacceptably slow linear scans and writing several hundred lines of routing infrastructure. A standard router lets every Wado HTTP service ship with a fast, correct, parameterized matcher out of the box.

### Design Goals

1. **Wasm/WASI native**: zero non-Wado dependencies, no I/O, no FFI; pure computation usable in any world
2. **Fast match path**: per-lookup cost competitive with mainstream native HTTP routers (`httprouter`, `matchit`, `axum`); no per-segment allocations on the hot path
3. **Path-parameter capture by name**: results expose params as `TreeMap<String, String>` so handlers read `params["id"]` rather than tracking positional indices
4. **Generic over handler type**: route to whatever the user wants — handler ids, function references, structs — via a `Router<H>` parameter
5. **Algorithm-neutral surface**: the public API does not commit to a specific data structure, so future implementation swaps (precompiled DFAs, hybrid hash + DFA, etc.) ship without breaking source compatibility
6. **`wasi:http` ergonomics**: provide a one-call helper that takes a `Request` and returns the match result, hiding path-vs-query splitting

### Scope

#### In Scope

- Static segment matching (`/api/v1/health`)
- Single-segment parameters (`/users/:id`)
- Trailing wildcard / catch-all (`/static/*path`)
- HTTP method dispatch (multiple methods per path with different handlers)
- 404 and 405 distinction (`allowed_methods(path)` returns the methods registered for a matching path)
- `Eq`-based duplicate-route handling: last write wins
- `wasi:http::Request` adapter

#### Out of Scope (for v1)

- Constrained / typed parameters (`:id<\d+>`, `:slug<[a-z0-9-]+>`)
- Regex routes
- Subdomain / hostname routing
- Mid-segment splits (`/users-:id`)
- Streaming / partial match
- Route precedence configuration knobs (the engine fixes literal > param > wildcard)

These are deferred until a real Wado service hits the limit; the structure of the v1 API leaves room to add them.

### Prior Art

| Router                 | Algorithm                      | Param style | Notes                                        |
| ---------------------- | ------------------------------ | ----------- | -------------------------------------------- |
| Go `httprouter`, `gin` | Radix tree                     | `:name`     | Industry baseline, byte-level prefix tree    |
| Rust `matchit`         | Radix tree                     | `{name}`    | Faster `httprouter` clone, used by axum      |
| Rust `actix-web`       | Compiled state machine + regex | `{name}`    | Slower than radix in practice, more flexible |
| Node `find-my-way`     | Radix tree                     | `:name`     | Used by Fastify                              |
| Rails `journey`        | Code-generated NFA             | `:name`     | Fast lookup, slow build, generates Ruby code |
| Express.js             | Linear scan                    | `:name`     | Simple, slow at scale                        |

Wado follows the same pattern syntax convention as `httprouter` / `matchit` / `find-my-way` (`:name`, `*name`) so users coming from these ecosystems do not need to relearn anything.

### Experimental Validation

The choice of algorithm in `core:router` is informed by a benchmark living under `example/router-*.wado`. Three engines — Linear scan / Radix tree / segment-level tagged DFA — were implemented against an identical 100-route fixture (70 static, 25 parametric, 5 wildcard, clustered under `/api/v1/...`) and run through a 10-element access pattern (9 hits + 1 miss).

Final numbers (debug `wasmtime`, Wado `-O2`, 5000 outer iterations × 10 lookups = 50000 lookups):

| Engine                   | Total time     | Per-lookup | vs Linear  |
| ------------------------ | -------------- | ---------- | ---------- |
| Linear (mixed)           | 34027 ms       | 681 µs     | 1×         |
| Linear (best, 1st route) | 571 ms / 50K   | 11 µs      | 60× faster |
| Linear (worst, miss)     | 48649 ms / 50K | 973 µs     | 0.7×       |
| **Radix tree**           | **2376 ms**    | **48 µs**  | **14×**    |
| **Segment DFA**          | **2293 ms**    | **46 µs**  | **15×**    |

Radix and DFA are within measurement noise of each other after both received their respective hot-path optimizations (radix: removing a defensive `params.keys()` snapshot; DFA: replacing per-state `TreeMap<String, i32>` with a sorted `Array<Edge>` plus a byte-range binary-search comparator that avoids `substr_bytes` allocation).

### Why Segment DFA Wins (Capability View, Not Speed)

Given speed parity, the choice is driven by capability and maintenance characteristics:

| Aspect                           | Radix tree                         | Segment DFA                     |
| -------------------------------- | ---------------------------------- | ------------------------------- |
| Build cost (100 routes, debug)   | 77 ms                              | 21 ms (~3.5× faster)            |
| Code size                        | ~180 lines                         | ~150 lines                      |
| Hot-path branches                | recursive, with backtracking       | iterative loop, no backtracking |
| State-graph readability          | byte-level edges, hard to dump     | segment-level, dump-friendly    |
| Mid-segment prefix sharing       | yes (`/list` / `/listed` collapse) | no (separate edges)             |
| Future `:id<\d+>` constraints    | easy                               | easy                            |
| Future per-state instrumentation | awkward (edges may span segments)  | natural (segment = transition)  |

The segment-level model matches "how authors think about routes" (segment → segment → handler) and is therefore the easier shape to extend in upcoming features (typed params, debug introspection, code-gen DFA). Mid-segment prefix sharing — the one strict win of byte-level radix — does not occur in real HTTP route sets often enough to justify the additional complexity.

Radix tree is still recommended as a separate `core:collections::PrefixTree<V>` for general byte-prefix indexing, where its byte-level granularity is genuinely useful.

## Decision

### Module: `core:router`

```wado
use { Router, RouteMatch } from "core:router";
use { Method } from "wasi:http";   // re-exported convenience: also available as core:router::Method
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
```

Rules:

- A leading `/` is required.
- A `:` segment must contain a non-empty parameter name.
- A `*` segment must contain a non-empty parameter name and be the last segment.
- Path matching is strict on trailing slashes: `/users` and `/users/` are different routes.

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

`Method` is re-exported from `wasi:http` so the typical service does not need a second `use`:

```wado
pub use { Method } from "wasi:http";
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

    /// Matches a method/path pair. Returns `Some(RouteMatch)` on a hit and
    /// `None` on a miss. The path argument must be the URL path only (no
    /// query string, no fragment).
    pub fn match_path(&self, method: Method, path: &String) -> Option<RouteMatch<H>>

    /// If `path` matches a registered route under any method, returns the
    /// list of methods registered for that path. Useful for emitting the
    /// `Allow:` header on a 405 response. Returns an empty array on a
    /// genuine 404.
    pub fn allowed_methods(&self, path: &String) -> Array<Method>
}
```

### `wasi:http` Adapter

```wado
use { Request } from "wasi:http";

impl<H> Router<H> {
    /// Matches against a `wasi:http` `Request`, splitting the path from the
    /// query string internally. Equivalent to:
    ///
    ///   let pq = request.get_path_with_query().unwrap_or("/");
    ///   let path = match pq.find("?") {
    ///       Some(i) => pq.substr_bytes(0, i),
    ///       None => pq,
    ///   };
    ///   self.match_path(request.get_method(), &path)
    pub fn match_request(&self, request: &Request) -> Option<RouteMatch<H>>
}
```

This adapter is the recommended entry point for `wasi:http/service` programs.

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
type Handler = fn(&Request, &TreeMap<String, String>) -> Response with EffectsHere;

let mut r = Router::<Handler>::new();
r.route(Method::Get, "/health", |_req, _params| Response::ok("ok"));
r.route(Method::Get, "/users/:id", |_req, params| {
    Response::ok(`user {params["id"]}`)
});
```

(For the CM-async case, the same shape works with `async fn(...) -> ...` once Wado closures support async — orthogonal to this WEP.)

### Behavior Specification

#### Lookup precedence

At each state during traversal, transitions are tried in this fixed order; there is no backtracking:

1. Literal — the segment exactly equals one of the registered literal edges
2. Parameter — there is a `:name` child; the current segment is captured and traversal descends
3. Wildcard — there is a `*name` child; the entire remainder of the path is captured

This means that `/users/list` registered alongside `/users/:id` will route exactly `/users/list` to the literal handler and any other single-segment value to the param handler. A path like `/users/list/extra` is **not** automatically re-tried against the param branch; it is a 404 unless `/users/:id/extra` is also registered.

This is the same rule used by `httprouter`, `gin`, `echo`, and `matchit`. The alternative — backtracking from a literal dead-end into the param branch — is rejected because it produces surprising matches (a literal that "wins" at one level but later fails redirects to a `:id` capture that swallows the literal name as the id).

#### Method handling

Each terminal stores a small list of `(Method, H)` entries. Registering `(Get, "/users", h1)` and `(Post, "/users", h2)` produces two entries at the same terminal; `match_path(Get, ...)` returns `h1`, `match_path(Post, ...)` returns `h2`, and `match_path(Put, ...)` returns `None` while `allowed_methods("/users")` returns `[Get, Post]`.

#### Trait Implementations

```wado
impl<H: Inspect> Inspect for Router<H>          // for `{router:?}`
impl<H: Inspect> Inspect for RouteMatch<H>      // for `{m:?}`
```

`Router<H>` is not `Eq` (route order during build can produce structurally distinct DFAs that match identically; defining equality is more confusing than helpful).

### Implementation Notes

The implementation lives in `wado-compiler/lib/core/router.wado` and is ported almost verbatim from `example/router-dfa.wado`, with the following changes:

- `RouteMatch` is generic over `H`; the example uses a concrete `i32 handler_id`.
- A `handlers: Array<H>` arena is added to `Router<H>`; terminal states reference handlers by `i32` index. This keeps `DfaState` non-generic, which avoids the monomorphization blow-up of including `H` inside every state.
- `route()` validates patterns and panics on malformed input.
- Method dispatch supports multiple methods per terminal via a sorted `Array<MethodEntry>` per terminal, binary-searched on lookup. (The example assumes one method per terminal.)
- A `match_request` adapter handles path/query split.

```
wado-compiler/lib/core/router.wado        Implementation (~250 lines)
wado-compiler/lib/core/router_test.wado   Unit tests (re-uses cases from the example)
example/router-*.wado                     Retained as a competing-algorithm benchmark
```

### Migration & Out-of-Tree Impact

- `example/http-server.wado` and any future `wasi:http/service` example can use `core:router` instead of inline `match path` ladders.
- No language changes are required.
- No breaking changes to `core:url`, `wasi:http`, or other modules.

## Consequences

### Pros

- A single canonical, fast HTTP router ships with the language.
- Uniform pattern syntax across the Wado ecosystem (no third-party divergence on `:name` vs `{name}`).
- The DFA structure is amenable to a future `wado-from-routes` build-time codegen: a constant set of routes can be lowered into a generated `match` cascade with zero runtime construction. The runtime API stays the same.
- Shared between `wasi:http` (server side) and HTTP client SPAs (e.g., reactive routers built on `core:url` + `core:router`).

### Cons / Trade-offs

- Generic `Router<H>` adds one more monomorphization axis. Cost is bounded — most services have at most one or two `H` types — and `H` is held in `Array<H>`, not in every state, so the bulk of the DFA is not re-monomorphized per `H`.
- Routes are not validated at compile time. A typo in a pattern string only surfaces at `Router::new` time. A future macro / `#routes!` form could lift this to compile time.
- Introducing `Router<H>` discourages users from writing their own routers; if the v1 API is too narrow (e.g., no constraint syntax), users either fall back to manual matching or wait for v2.

### Future Work

- [ ] Constrained parameters: `:id<\d+>`, `:slug<[a-z0-9-]+>`
- [ ] Build-time route compilation: `#![generated]` DFA tables produced from a TOML/Wado route manifest
- [ ] `core:collections::PrefixTree<V>` for the byte-level radix algorithm, separated from HTTP routing
- [ ] OpenAPI / Swagger introspection: dump the route table as JSON for documentation tooling
- [ ] Route precedence customization (priority hints) — only if a real use case appears

## See Also

- [WEP: URL Standard Library (`core:url`)](./wep-2026-04-10-url-stdlib.md)
- [WEP: WASI HTTP Integration](./wep-2026-02-21-wasi-http.md)
- `example/router-*.wado` — competing-algorithm benchmark
