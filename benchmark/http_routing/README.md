# HTTP Routing Benchmark — `wado serve` vs Hono vs Axum

This benchmark compares `wado serve` against an equivalent
[Hono](https://hono.dev/) server on Node.js and an equivalent
[Axum](https://github.com/tokio-rs/axum) server compiled as native Rust,
on an HTTP routing workload.

## What it measures

The route set and request set are **Hono's own official router
benchmark** —
[`honojs/hono`, `benchmarks/routers/`](https://github.com/honojs/hono/tree/main/benchmarks/routers):

- **12 routes** from `src/tool.mts` (static, single-parameter, and
  wildcard routes, including a `GET`/`POST` collision on `/event/:id`).
- **7 request shapes** from `src/bench.mts` (short static, static
  sharing a radix, dynamic, mixed static/dynamic, `POST`, long static,
  wildcard).

All three servers register the same 12 routes and return the same
`{ "route": ..., "params": [...] }` JSON shape, so the comparison
isolates routing + request handling. Load is applied with
[`oha`](https://github.com/hatoo/oha).

Hono's original benchmark is an in-process router microbenchmark
(`router.match()` under `mitata`). Here the same route/request set is
driven end to end over HTTP, so the three are compared as whole servers.

The three servers span three runtimes:

- **`wado serve`** — a `wasi:http/service` component on wasmtime,
  dispatched through `core:router`, with pooled instance reuse +
  periodic recycling.
- **Hono** — JavaScript on Node.js (`@hono/node-server`), default
  `SmartRouter`.
- **Axum** — native Rust on Tokio; the native-compiled reference point.

## Files

- `app.wado` — Wado `wasi:http/service` world server.
- `app.js` — Hono server (`@hono/node-server`).
- `axum_server.rs` + `Cargo.toml` — Axum server (native Rust).
- `bench.sh` — driver: builds, starts each server, runs `oha`.

## Running

```sh
mise run -C benchmark http-routing
# or, from the repo root:
mise run benchmark-http-routing
```

Prerequisites: `oha` (`cargo install oha`), Node.js, and a Rust
toolchain. The driver runs `npm install` for the Hono dependencies and
`cargo build` for the Axum server on first use.

Tunables:

```sh
DURATION=10s CONNECTIONS=100 mise run -C benchmark http-routing
```

## Recent Results

Measured 2026-05-20 on a cloud VM, `oha` driving each request for 6s at
50 concurrent connections. Cloud VMs are noisy; runs were repeated and
the more internally consistent one is shown.

Environment:

| Component | Version            |
| --------- | ------------------ |
| Wado      | 0.0.2 (2026-05-20) |
| wasmtime  | 44.0.0             |
| Node.js   | 24.14.1            |
| Hono      | 4.12.21            |
| Axum      | 0.8.9              |
| rustc     | 1.95.0             |
| oha       | 1.14.0             |

Throughput (requests/sec, higher is better):

| Request                                     | `wado serve` | Hono (Node) | Axum (native) |
| ------------------------------------------- | -----------: | ----------: | ------------: |
| `GET /user`                                 |       30,728 |      25,701 |       138,236 |
| `GET /user/comments`                        |       29,489 |      28,094 |       124,284 |
| `GET /user/lookup/username/hey`             |       29,608 |      21,466 |       119,565 |
| `GET /event/abcd1234/comments`              |       28,862 |      24,815 |       124,284 |
| `POST /event/abcd1234/comment`              |       28,721 |      18,772 |       132,484 |
| `GET /very/deeply/nested/route/hello/there` |       29,121 |      21,668 |       131,430 |
| `GET /static/index.html`                    |       30,326 |      20,907 |       119,672 |

Observations:

- **`wado serve` leads Hono on every request** — ~29k–30k req/s versus
  Hono's ~19k–28k.
- **Axum (native Rust) is ~4–5x faster than `wado serve`** — ~120k–138k
  req/s. This is the native-compiled ceiling: no Wasm component
  instantiation, no component-model boundary, no recycling.
- `wado serve` throughput is flat across route shapes: path matching via
  `core:router` is not the bottleneck.
- A whole-stack, cross-runtime comparison (Wasm component on wasmtime vs
  JS on Node.js vs native Rust). For routing-algorithm-only numbers, see
  `example/router_bench.wado`.
