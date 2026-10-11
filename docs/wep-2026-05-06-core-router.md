# WEP: HTTP Path Router (`core:router`)

## Context

A Wado program serving `wasi:http` dispatches each request to a handler by its
method and path. Without a standard router, every author writes their own: a
`match` ladder, an `if` chain, or a hand-rolled trie. Each gets parameters,
405 responses, and precedence slightly differently. A standard router gives
every Wado HTTP service one fast, correct matcher with parameters.

## Decision

### One router, generic over the handler

`core:router` provides `Router<H>`. `H` is whatever the program dispatches on:
a handler id, a function, or a struct or variant of its own. The router never
calls a handler, so it takes no position on sync versus async, or on effects.

A match is a `RouteMatch<H>`: the handler and the captured `PathParams`.
`Method` is `wasi:http`'s type, re-exported, so a router feeds from a request
with no conversion. `match_request` reads the method and path from a
`wasi:http` request and strips the query and fragment.

### Pattern syntax

Wado follows the syntax of Go's `httprouter` and Node's `find-my-way`: `:name`
captures one segment, and `*name` captures the rest of the path. Authors coming
from those routers, or from `gin`, already know it.

| Router                 | Algorithm                      | Param style |
| ---------------------- | ------------------------------ | ----------- |
| Go `httprouter`, `gin` | Radix tree                     | `:name`     |
| Rust `matchit` (axum)  | Radix tree                     | `{name}`    |
| Rust `actix-web`       | Compiled state machine + regex | `{name}`    |
| Node `find-my-way`     | Radix tree                     | `:name`     |

A malformed pattern panics at registration rather than matching nothing. A
pattern is a constant in the program, so a malformed one is a bug in the
program, and the panic reports it the first time the router is built. These
are malformed:

- a pattern that does not start with `/`
- an empty segment: a doubled `/`, or a trailing `/`
- a `:` or `*` with no name after it
- a wildcard that is not the last segment
- a name used twice in one pattern, since `params["id"]` could then mean
  either segment
- a parameter or wildcard named differently from one registered before at the
  same place (`/users/:id`, then `/users/:name/posts`), since that place would
  then capture under two names

### Matching is strict about the path

A path matches exactly as written. `/users` and `/users/` are different paths,
and a path without a leading `/` matches nothing. Parameters hold the raw text
of the path, with no percent-decoding. A router that normalizes or decodes
makes a choice the service may disagree with, and that cannot be undone after
the match. The strict form leaves those choices to the service.

### Precedence: a literal wins, and does not give way

Where a literal and a parameter could both take a segment, the literal takes
it. A path that then finds no route does not go back to try the parameter.
With `/users/list` and `/users/:id`, the path `/users/list/extra` matches
neither. Going back would let `/users/:id/extra` capture `list` as an id: the
literal wins at one level, then silently loses at the next.

### A wildcard catches what finds no route

A path that finds no route falls back to the deepest wildcard it passed that
takes the request's method. So does a path that finds a route without a
handler for its method. `/*rest` therefore catches everything no other route
takes, and `/files/*rest` keeps matching `/files/a` after `/files/:id/meta` is
registered.

Without the fallback, adding a deeper route would silently disable a
shallower wildcard. Rejecting the overlap at registration, as `httprouter`
does, would forbid the most common wildcard there is: a catch-all beside other
routes. The fallback is cheap: the walk keeps the last wildcard it passed and
never goes back over the path.

### Method dispatch, and 405

A route keeps one handler per method, and optionally one `any` handler. A
handler for the request's method wins over `any`. `any` takes every method,
including `Method::Other(_)`, so an `httpbin`-style `/anything` needs no list
of methods.

`allowed_methods(path)` lists the methods `match_path` would match the path
under. A `None` from `match_path` with a non-empty list is a 405, and the list
is its `Allow:` header. An empty list is a 404. A method that only an `any`
handler takes is not listed, since `any` has no finite list to give.

### Parameters are views, typed through serde

`PathParams` refers to the path and holds a byte range per parameter. Looking a
value up copies it out, and a route with no parameters builds nothing.

`params.deserialize::<T>()` reads the parameters into a struct, matching each
parameter to the field of the same name. It goes through `core:serde`, so
`#[wire]` renames work as they do for every other format. A scalar field parses
from the path's bytes without an intermediate `String`. An invalid value
fails with `InvalidValue` and its byte offset in the path. A field of a
compound type fails with `UnexpectedType`, since a segment holds text alone.

### Algorithm: segment-level DFA

The router is a deterministic automaton over whole segments: each segment is
one transition. A route without parameters stays out of the automaton: its
pattern is a key in a hash table, which a path is looked up in first.

`example/router_*.wado` implements three engines against one fixture of 100
routes: 70 static, 25 parametric, and 5 wildcard, clustered under
`/api/v1/...`. Each runs a 10-path access pattern of 9 hits and 1 miss. Run it
with `cargo run --bin wado -- run example/router_bench.wado`. Times below are
relative to the segment DFA.

| Engine                   | Lookup time |
| ------------------------ | ----------- |
| Linear (mixed)           | 14.8×       |
| Linear (best: 1st route) | 0.25×       |
| Linear (worst: miss)     | 21.2×       |
| Radix tree               | 1.04×       |
| Segment DFA              | 1×          |

Radix and DFA match on speed, so capability and upkeep decided:

| Aspect                           | Radix tree                        | Segment DFA                    |
| -------------------------------- | --------------------------------- | ------------------------------ |
| Build cost (100 routes)          | 3.7×                              | 1×                             |
| Hot path                         | recursive, with backtracking      | iterative, never revisits path |
| State-graph readability          | byte-level edges, hard to dump    | segment-level, easy to dump    |
| Future per-state instrumentation | awkward (edges may span segments) | natural (segment = transition) |

The segment model is how authors think about routes, and it is the easier
shape to extend with typed parameters, introspection, or a generated
automaton.

## Known gaps

- Constrained or typed parameters: `:id<\d+>`, `:slug<[a-z0-9-]+>`.
- A parameter inside a segment: `/users-:id`.
- Routing by host name.
- `HEAD` does not fall back to `GET`: a service serves `HEAD` only where it
  registers it.
- `allowed_methods` takes a path, while `match_request` takes a request, so a
  service answering a 405 strips the query from the request's path itself.
- Routes are checked when the router is built, not at compile time. A typo in
  a pattern panics on the first run that builds the router.
- Nothing checks that the struct given to `params.deserialize::<T>()` names
  the parameters of the route that matched.
- Routes cannot be built at compile time. Every program builds its router at
  run time, though the routes are constants.
- The route table cannot be dumped for documentation tooling such as OpenAPI.

## See Also

- [WEP: URL Standard Library (`core:url`)](./wep-2026-04-10-url-stdlib.md)
- [WEP: WASI HTTP Integration](./wep-2026-02-21-wasi-http.md)
- `example/router_*.wado`: the benchmark of competing algorithms
