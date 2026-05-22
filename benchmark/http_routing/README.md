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
- **7 request shapes** from `src/bench.mts` (short static, static
  sharing a radix, dynamic, mixed static/dynamic, `POST`, long static,
  wildcard).

All four servers register the same 12 routes and return the same
`{ "route": ..., "params": [...] }` JSON shape, so the comparison
isolates routing + request handling. Load is applied with
[`oha`](https://github.com/hatoo/oha).

Hono's original benchmark is an in-process router microbenchmark
(`router.match()` under `mitata`). Here the same route/request set is
driven end to end over HTTP, so the four are compared as whole servers.

### Stable measurement on a noisy host

Cloud VMs throttle and steal CPU, so a naive "measure each server for N
seconds in turn" run yields ratios that drift with the host. `bench.sh`
counters this:

- **CPU pinning** — servers run on one core set, the `oha` load
  generator on a disjoint set (`taskset`), so the two never contend.
- **Round-robin interleaving** — every server stays up for the whole
  run; each request is measured in short slices that rotate across
  servers, repeated for `ROUNDS` rounds. A throttling episode hits every
  server within the same time window, so ratios survive it.
- **Max aggregation** — contention and throttling only ever lower
  throughput, so the fastest slice across rounds is the cleanest
  estimate of true capacity. Round 1 also serves as a warmup.

Absolute numbers are not comparable across machines or runs — only the
ratios between servers within one run are.

The four servers span four runtimes:

- **`wado serve`** — a `wasi:http/service` component on wasmtime,
  dispatched through `core:router`, with pooled instance reuse +
  periodic recycling.
- **Hono (Node)** — JavaScript on Node.js (`@hono/node-server`), default
  `SmartRouter`.
- **Hono (Bun)** — the same Hono app on Bun (`Bun.serve`); the
  fastest-JS reference point.
- **Axum** — native Rust on Tokio; the native-compiled reference point.

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
# SLICE: seconds per measurement slice (default 3)
# ROUNDS: rotation rounds; the per-server max is kept (default 3)
# CONNECTIONS: concurrent connections per slice (default 50)
SLICE=5 ROUNDS=5 CONNECTIONS=100 mise run -C benchmark http-routing
```

## Results

Throughput numbers live in [`benchmark/README.md`](../README.md), the
single source of truth for all benchmark results. For
routing-algorithm-only numbers (no HTTP stack), see
`example/router_bench.wado`.
