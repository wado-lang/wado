# HTTP Routing Benchmark — `wado serve` vs Hono vs Axum

This benchmark compares `wado serve` against an equivalent
[Hono](https://hono.dev/) server on Node.js and on Bun, and an
equivalent [Axum](https://github.com/tokio-rs/axum) server compiled as
native Rust, on an HTTP routing workload.

## What it measures

The route set and request set are **Hono's own official router
benchmark** —
[`honojs/hono`, `benchmarks/routers/`](https://github.com/honojs/hono/tree/main/benchmarks/routers):

- **12 routes** from `src/tool.mts` (static, single-parameter, and
  wildcard routes, including a `GET`/`POST` collision on `/event/:id`).
- **4 request shapes** from `src/bench.mts` — static, dynamic, `POST` over a
  mixed static/dynamic path, and wildcard. The other three only vary depth or
  a covered case's radix sibling.

All four servers register the same 12 routes and return the same
`{ "route": ..., "params": [...] }` JSON shape, so the comparison
isolates routing + request handling. Load is applied with
[`oha`](https://github.com/hatoo/oha).

Hono's original benchmark is an in-process router microbenchmark
(`router.match()` under `mitata`). Here the same route/request set is
driven end to end over HTTP, so the four are compared as whole servers.

### Equal core budgets

Throughput scales with worker count, so a table compares servers only when every
server gets the same one. `SHAPES` names them: a 1-core container scaled out
horizontally, and a small VM running one instance.

Node scales out with `node:cluster` (`SCHED_NONE`), Bun with one `SO_REUSEPORT`
process per worker, `wado serve` with `--workers`, Axum with
`TOKIO_WORKER_THREADS`.

### Keeping the load generator off the critical path

A saturated `oha` caps the fastest servers and compresses every ratio. It takes
a fixed `OHA_CORE_COUNT`, not the cores left over: more generator threads thin
each one's batch and the server pays for the extra wakeups.

`HEADROOM_CHECK=1` re-runs each server at twice the connections; a gain there
means `oha` set that number. It passes at the settings above, so it is off.

Only the measured server runs, and its port is asserted free first — a survivor
keeps serving under `SO_REUSEPORT` and inflates the row. Each server is warmed
over every route, then measured for `ROUNDS` slices with the fastest kept.

The four servers span four runtimes:

- **`wado serve`** — a `wasi:http/service` component on wasmtime,
  dispatched through `core:router`, with pooled instance reuse +
  periodic recycling.
- **Hono (Node)** — JavaScript on Node.js (`@hono/node-server`), default
  `SmartRouter`.
- **Hono (Bun)** — the same Hono app on Bun (`Bun.serve`); the
  fastest-JS reference point.
- **Axum** — native Rust on Tokio; the native-compiled reference point.

### Why HTTP/1.1 only

Every server is driven over HTTP/1.1. h2c looks like the fairer choice —
a reverse proxy speaks it to its upstream by default — but `oha` opens
one stream per connection, which hands HTTP/2 all of its per-request
framing and flow-control cost and none of its multiplexing benefit. The
resulting spread (measured: `wado serve` -2 to -9%, Axum -30 to -37%,
Hono on Node -57 to -64%) ranks each runtime's HTTP/2 stack under a load
shape nobody deploys, in a benchmark whose job is to isolate routing.
Restoring the h2c rows needs a load generator driving many streams per
connection first; until then the numbers would invite a wrong reading.

## Files

- `app.wado` — Wado `wasi:http/service` world server.
- `app.routes.js` — shared Hono route definitions.
- `app.js` — Hono server entry point for Node.js (`@hono/node-server`).
- `app.bun.js` — Hono server entry point for Bun (`Bun.serve`).
- `axum_server.rs` + `Cargo.toml` — Axum server (native Rust).
- `bench.sh` — driver: builds, starts each server, runs `oha`.

## Running

```sh
mise run -C benchmark http-routing
# or, from the repo root:
mise run benchmark-http-routing
```

Prerequisites: `oha` (`cargo install oha`), Node.js, Bun, and a Rust
toolchain. The driver runs `npm install` for the Hono dependencies and
`cargo build` for the Axum server on first use. The Bun step is skipped
gracefully if `bun` is not on `PATH`.

Tunables (env vars):

```sh
# SLICE: seconds per measurement slice (default 10)
# ROUNDS: slices per request; the max is kept (default 3)
# SHAPES: worker counts to measure (default "1 4")
# CONNECTIONS_PER_WORKER: offered concurrency per worker (default 200)
# OHA_CORE_COUNT: cores for the load generator (default 4)
# HEADROOM_CHECK: 1 to verify oha is not the ceiling (default 0)
SLICE=10 ROUNDS=3 SHAPES="1 4" mise run -C benchmark http-routing
```

## Results

Throughput numbers live in [`benchmark/README.md`](../README.md), the
single source of truth for all benchmark results. For
routing-algorithm-only numbers (no HTTP stack), see
`example/router_bench.wado`.
