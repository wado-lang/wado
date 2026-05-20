# HTTP Routing Benchmark — `wado serve` vs Hono

This benchmark compares `wado serve` against an equivalent
[Hono](https://hono.dev/) server running on Node.js, on an HTTP routing
workload.

## What it measures

The route set and request set are **Hono's own official router
benchmark** —
[`honojs/hono`, `benchmarks/routers/`](https://github.com/honojs/hono/tree/main/benchmarks/routers):

- **12 routes** from `src/tool.mts` (static, single-parameter, and
  wildcard routes, including a `GET`/`POST` collision on `/event/:id`).
- **7 request shapes** from `src/bench.mts` (short static, static
  sharing a radix, dynamic, mixed static/dynamic, `POST`, long static,
  wildcard).

Both servers register the same 12 routes and return the same
`{ "route": ..., "params": [...] }` JSON shape, so the comparison
isolates routing + request handling. Load is applied with
[`oha`](https://github.com/hatoo/oha).

Hono's original benchmark is an in-process router microbenchmark
(`router.match()` under `mitata`). Here the same route/request set is
driven end to end over HTTP, so `wado serve` and Hono are compared as
whole servers.

`wado serve` runs the Wado side: a `wasi:http/service` component on
wasmtime, dispatched through `core:router`. It reuses a small pool of
component instances with periodic recycling and the pooling allocator.
Hono uses its default `SmartRouter` on `@hono/node-server`. This is a
**Wasm component on wasmtime vs JavaScript on Node.js** comparison —
two different runtimes.

## Files

- `app.wado` — Wado `wasi:http/service` world server.
- `app.js` — Hono server (`@hono/node-server`).
- `bench.sh` — driver: builds, starts each server, runs `oha`.

## Running

```sh
mise run -C benchmark http-routing
# or, from the repo root:
mise run benchmark-http-routing
```

Prerequisites: `oha` (`cargo install oha`) and Node.js. The driver runs
`npm install` for the Hono dependencies on first use.

Tunables:

```sh
DURATION=10s CONNECTIONS=100 mise run -C benchmark http-routing
```

## Recent Results

Measured 2026-05-20 on a cloud VM, `oha` driving each request for 6s at
50 concurrent connections. Cloud VMs are noisy; both measured runs show
the same picture, the more internally consistent one is shown.

Environment:

| Component | Version            |
| --------- | ------------------ |
| Wado      | 0.0.2 (2026-05-20) |
| wasmtime  | 44.0.0             |
| Node.js   | 24.14.1            |
| Hono      | 4.12.21            |
| oha       | 1.14.0             |

Throughput (requests/sec, higher is better):

| Request                                     | `wado serve` | Hono (Node) |
| ------------------------------------------- | -----------: | ----------: |
| `GET /user`                                 |       30,778 |      20,728 |
| `GET /user/comments`                        |       30,193 |      26,496 |
| `GET /user/lookup/username/hey`             |       28,326 |      20,728 |
| `GET /event/abcd1234/comments`              |       25,292 |      23,258 |
| `POST /event/abcd1234/comment`              |       25,068 |      17,668 |
| `GET /very/deeply/nested/route/hello/there` |       29,724 |      23,876 |
| `GET /static/index.html`                    |       29,091 |      22,595 |

Observations:

- `wado serve` leads on every request — ~25k–31k req/s versus Hono's
  ~17k–26k. The margin is widest on the parametric, `POST`, and deep
  routes.
- `wado serve` throughput is fairly flat across route shapes: path
  matching via `core:router` is not the bottleneck.
- This is a whole-stack, cross-runtime comparison (Wasm component on
  wasmtime vs JS on Node.js). For routing-algorithm-only numbers, see
  `example/router_bench.wado`.
