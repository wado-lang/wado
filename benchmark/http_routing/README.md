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

## Recent Results

Measured 2026-05-22 on a 4-core cloud VM: servers pinned to 3 cores, the
`oha` load generator to 1, `SLICE=3 ROUNDS=3`.

Environment:

| Component | Version            |
| --------- | ------------------ |
| Wado      | 0.0.2 (2026-05-22) |
| wasmtime  | 44.0.0             |
| Node.js   | 24.14.1            |
| Bun       | 1.3.14             |
| Hono      | 4.12.22            |
| Axum      | 0.8.9              |
| rustc     | 1.95.0             |
| oha       | 1.14.0             |

Throughput (requests/sec, higher is better):

| Request                                     | `wado serve` | Hono (Node) | Hono (Bun) | Axum (native) |
| ------------------------------------------- | -----------: | ----------: | ---------: | ------------: |
| `GET /user`                                 |       54,106 |      40,486 |     71,401 |        93,103 |
| `GET /user/comments`                        |       50,214 |      44,214 |     73,380 |        92,831 |
| `GET /user/lookup/username/hey`             |       51,752 |      42,260 |     64,678 |        95,552 |
| `GET /event/abcd1234/comments`              |       50,771 |      42,425 |     66,314 |        92,374 |
| `POST /event/abcd1234/comment`              |       50,780 |      33,452 |     66,137 |        92,109 |
| `GET /very/deeply/nested/route/hello/there` |       50,786 |      45,021 |     71,736 |        92,523 |
| `GET /static/index.html`                    |       50,553 |      41,972 |     66,981 |        94,257 |

Observations:

- **`wado serve` leads Hono on Node on every request** — ~50k–54k req/s
  versus Node's ~33k–45k.
- **Hono on Bun is ~1.35x faster than `wado serve`** — ~65k–73k req/s.
  Bun's HTTP server is markedly faster than Node's, so the fastest-JS
  baseline still leads `wado serve`.
- **Axum (native Rust)** is the native-compiled ceiling. Its figure here
  is load-generator-limited: with `oha` pinned to a single core, Axum
  saturates the generator (~92k–96k, flat across requests) rather than
  itself. Its true throughput is higher.
- `wado serve` throughput is flat across route shapes: path matching via
  `core:router` is not the bottleneck.
- A whole-stack, cross-runtime comparison (Wasm component on wasmtime vs
  JS on Node.js/Bun vs native Rust). For routing-algorithm-only numbers,
  see `example/router_bench.wado`.
