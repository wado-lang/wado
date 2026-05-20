# HTTP Routing Benchmark — `wado serve` vs Hono

This benchmark compares `wado serve` against an equivalent
[Hono](https://hono.dev/) server running on Node.js, on a routing-heavy
HTTP workload.

## What it measures

Both servers expose the **same 32-route set** and return the **same
`{ "route": ..., "params": [...] }` JSON shape**. The route set mixes:

- shallow and deep static routes sharing an `/api/v1` prefix,
- single- and multi-parameter routes (`:id`, `:id/.../:pid/.../:cid`),
- a wildcard route (`/static/*path`),
- two `POST` routes that collide on a path with `GET`.

The Wado side dispatches through `core:router` (a segment-level tagged
DFA matcher). Hono uses its default `SmartRouter`. Load is applied with
[`oha`](https://github.com/hatoo/oha).

This is a **Wasm component on wasmtime vs JavaScript on Node.js**
comparison — two different runtimes — so absolute numbers reflect the
whole stack (HTTP server, routing, JSON serialization), not routing
alone.

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

Measured 2026-05-20 on a cloud VM. `oha` drove each route for 5s at 50
concurrent connections. Cloud VMs are noisy — absolute numbers vary
between runs, but the Wado side is steady at ~3.6k req/s regardless of
route shape, which is the point of interest below.

Environment:

| Component | Version            |
| --------- | ------------------ |
| Wado      | 0.0.2 (2026-05-20) |
| wasmtime  | 44.0.0             |
| Node.js   | 24.14.1            |
| Hono      | 4.12.21            |
| oha       | 1.14.0             |

Throughput (requests/sec, higher is better):

| Route                                    | Shape       | `wado serve` | Hono (Node) |
| ---------------------------------------- | ----------- | -----------: | ----------: |
| `/health`                                | static      |        3,605 |      24,234 |
| `/api/v1/users/list`                     | static      |        3,699 |      16,430 |
| `/api/v1/admin/system/cache/stats`       | deep static |        3,598 |      22,634 |
| `/api/v1/users/4242`                     | 1 param     |        3,562 |      14,851 |
| `/api/v1/users/4242/posts/77`            | 2 params    |        3,628 |      21,046 |
| `/api/v1/users/4242/posts/77/comments/9` | 3 params    |        3,786 |      18,088 |
| `/static/css/site/main.css`              | wildcard    |        3,716 |      22,588 |
| `/no/such/route`                         | miss (404)  |        3,630 |      20,627 |

Observations:

- `wado serve` throughput is **flat across every route shape** — static,
  deep static, multi-parameter, wildcard, and 404 all land within ~6% of
  each other. Path matching via `core:router` is not the bottleneck;
  per-request overhead (Wasm component HTTP plumbing, body streaming,
  allocation) dominates.
- Hono on Node.js is currently ~4–6x faster end to end, and noisier
  (its numbers swing more between runs).
- This is a whole-stack, cross-runtime comparison (Wasm component on
  wasmtime vs JS on Node.js), not an isolated routing microbenchmark.
  For routing-algorithm-only numbers, see `example/router_bench.wado`.
