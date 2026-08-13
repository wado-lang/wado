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

- **`wado`** — a `wasi:http/service` component on wasmtime, dispatched
  through `core:router`, with pooled instance reuse + periodic recycling.
- **Node** — Hono on Node.js (`@hono/node-server`), default
  `SmartRouter`.
- **Bun** — the same Hono app on Bun (`Bun.serve`); the fastest-JS
  reference point.
- **Axum** — native Rust on Tokio; the native-compiled reference point.

### Protocols

Every row names its protocol, and each server is measured over both
HTTP/1.1 and h2c wherever it can serve them. h2c is not a curiosity
here: a reverse proxy talks h2c to its upstream by default, so it is the
shape these servers are usually deployed in. The two are kept as
separate rows rather than folded together because they are not
interchangeable at this response size — h2c pays framing and
flow control per request that a single-stream-per-connection load
pattern never amortizes.

How a server gets its h2c row differs by runtime:

- `wado` and Axum sniff the connection preface, so one process answers
  both rows on one port. (Axum needs its `http2` feature, which is off
  by default.)
- Node's `node:http` and `node:http2` are separate servers with no
  upgrade path between them, so Hono on Node needs a second process on
  its own port (`app.h2c.js`), and that row does not answer HTTP/1.1 at
  all.
- `Bun.serve` has no h2c server, so Bun is HTTP/1.1 only.

Rows rotate slice by slice, so two rows sharing a process never load it
at the same time.

## Files

- `app.wado` — Wado `wasi:http/service` world server.
- `app.routes.js` — shared Hono route definitions.
- `app.js` — Hono server entry point for Node.js (`@hono/node-server`).
- `app.h2c.js` — the same, over `node:http2` (h2c).
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
