# HTTP Routing Benchmark Performance Analysis

Date: 2026-07-27

Guest-side analysis of `benchmark/http_routing` (`wado serve` vs Axum), following
the `wado-performance` method: decompose the per-request cost by ablation, read
the WIR for what each request allocates, then A/B one change.

## Summary

The guest handler's _compute_ is not the problem — it is ~5 µs per request. The
GC work induced by the ~20 short-lived objects that request allocates is ~40 µs.
Allocation churn, not routing or serialization logic, is what separates
`wado serve` from Axum on the guest side.

Two changes applied here — a byte-string literal for the `content-type` value and
`write_raw` for the body — remove 4 of those objects and measure +2 to +6%. The
rest of the list is optimizer work.

## Method

4-core cloud VM, release `wado`, servers pinned to cores 0-2 (`--workers 3`),
`oha` on core 3, 50 connections, best of 3 rounds of 3 s slices, all servers up
for the whole run and rotated slice by slice (the `bench.sh` methodology). Only
ratios within one run are meaningful; absolute numbers are machine-specific and
lower than `benchmark/README.md`, which was measured elsewhere.

Ablation apps (`app.wado` minus one layer at a time):

| Variant   | What it does                                                 |
| --------- | ------------------------------------------------------------ |
| `base`    | `app.wado` as committed                                      |
| `hoisted` | `base` with the constant `content-type` value in a global    |
| `router`  | `hoisted` minus JSON and param extraction (constant body)    |
| `floor`   | no router, no JSON: constant body, identical wasi:http calls |
| `no-gc`   | `base` with `--collector null` (allocates, never collects)   |

## Cost decomposition

Throughput converted to core-µs per request (3 cores ÷ req/s), lower is better:

| Request                        | base | hoisted | no-gc | floor | axum |
| ------------------------------ | ---: | ------: | ----: | ----: | ---: |
| `GET /user`                    |  103 |      96 |    80 |    66 |   40 |
| `GET /event/abcd1234/comments` |  121 |     116 |    76 |    71 |   41 |
| `GET /static/index.html`       |  120 |     114 |    79 |    68 |   39 |

Staged build-up of the guest path (all with the header hoisted, separate run):

| Stage                     | `GET /user` | `GET /event/:id/comments` |
| ------------------------- | ----------: | ------------------------: |
| floor                     |          69 |                        68 |
| + router match + dispatch |          92 |                        91 |
| + JSON body + params      |          99 |                       110 |

Two conclusions:

- Guest work is ~50 µs of the ~120 µs, not a rounding error. The remaining
  ~68 µs is the `wado serve` HTTP stack itself, which is 28 µs above Axum's 40 µs
  — a separate, host-side track (hyper → mpsc → fiber spawn → frame channel per
  request).
- Almost all of that 50 µs is GC. `--collector null` removes collection and
  nothing else, and it recovers 40-45 µs; the compute left over the floor is
  ~5-15 µs. `--collector drc` and `--allocator bump` both measured the same as
  the default, so this is collection cost, not linear-memory allocator cost.

The same guest code measured in a CLI loop (`match_path` + handler, release
build) costs 0.86 µs for the match, 3.9 µs including the JSON body — i.e. the
compute agrees with `no-gc − floor`. In the CLI loop the garbage dies
immediately and the live set stays tiny, so the copying collector barely
notices; under `wado serve` the same allocation rate against a store that holds
the router tables and in-flight request state costs ~2 µs per object churned.

## What a request allocates

From `wado dump -O2 --world wasi:http/service app.wado`, GC objects on the happy
path of one dynamic-route request:

| Source                                                          | Objects | Note                                                |
| --------------------------------------------------------------- | ------: | --------------------------------------------------- |
| `"application/json".bytes().collect()`                          |       5 | `List` + `array_new(0)` grown 4 → 8 → 16            |
| `Request::get_path_with_query` lift                             |       2 | fresh array from linear memory, then cloned again   |
| `match_dynamic` (`ranges`, `PathParams`, `RouteMatch`)          |      ~5 | escape-promoted locals; static routes allocate none |
| `p["id"]` substring + `[p["id"]]` list                          |       4 | `String` + array, `List` + array                    |
| `json::to_bytes` (128-byte buffer, `Option<char>`, `ByteSlice`) |       4 |                                                     |
| `ByteSlice::to_list()`                                          |       2 | copy of the serialized bytes                        |
| `RouteResponse` + `resp.body` copy in `handle`                  |       3 | second copy of the same bytes                       |

Linear memory is touched separately: `cm_lower_string` / `cm_lower_list_u8` /
CM out-pointers do roughly ten `malloc`/`free` pairs per request, which is why
the freelist allocator's `fl_unlink` is the most-called guest function.

Four of these are avoidable without changing what the benchmark measures. Items 1
and 4 are now applied to `app.wado`; 2 and 3 are compiler work.

1. Constant `collect()` in the hot path: `"literal".bytes().collect()` allocated
   a `List` and four arrays every evaluation, because `FromIterator` for a list
   starts at capacity 0 and pushes one element at a time and the `Iterator` trait
   has no size hint to reserve from. The app now writes `b"application/json"`,
   which lowers to one `struct.new` over `array.new_data`. The two objects left
   per request are what constant globalization should remove; see below.
2. Redundant clone when lifting a CM string: the generated binding calls
   `core:rt/memory_to_gc_array`, which allocates a fresh array and copies the
   bytes in, and then clones that fresh array into another one before wrapping it
   in a `String` — `core:rt/memory_to_gc_string` already does the right thing.
   Freshness analysis does not see through the helper. One extra array per lifted
   string, per request.
3. `resp.body` copied on last use: `body_tx.write(resp.body)` compiles to an
   `array_new` + `array_copy` of the body even though `resp` is dead afterwards;
   a last-use move would drop it.
4. `StreamWritable::write` takes `List<u8>` by value, so the body was deep-copied
   into the CM lowering. The app now calls `write_raw(resp.body.as_slice())`,
   which lowers the slice directly. The value copy of `resp.body` itself (item 3)
   survives it, because the field read copies before `as_slice()` sees anything.

## Measured levers

Items 1 and 4 together, interleaved A/B against the previous `app.wado`, best of
3 rounds (this run's machine state is faster than the ablation run above, so
compare within the table only):

| Request                        |   base | `b"…"` + `write_raw` | delta |
| ------------------------------ | -----: | -------------------: | ----: |
| `GET /user`                    | 35,801 |               38,026 | +6.2% |
| `GET /event/abcd1234/comments` | 31,953 |               32,683 | +2.3% |
| `GET /static/index.html`       | 31,983 |               33,423 | +4.5% |

Hoisting the byte literal further into a `global` — what constant globalization
would do — adds another +1 to +8% on top (39,862 / 35,176 / 33,790 in the same
run), for +6 to +11% over the base.

The payoff is proportional to objects removed, not to instructions removed: an
earlier A/B that removed all five header objects by hand measured +8.2% / +9.7%
on the two routes, of which only ~1 µs was compute and the rest GC that no longer
runs.

## The constant is no longer rebuilt per request

`const_object_globalization` (WEP 2026-05-31) matched a qualifying constant in a
`let` binding or behind a `&`, but not in a by-value argument position — which is
where the header value sits, so it was rebuilt per request. The pass now matches
that shape too, gated on the callee's parameter being read-only in the callee's
own body, and the header value compiles to an eager `array.new_fixed` global read
straight into `Fields::append`: zero per-request allocations for the header.

Two related copies remain, both in the WIR:

- The body still goes through `resp.body`'s last-use copy (item 3 above).
- A constant handed to a callee that writes its parameter still gets a
  caller-side defensive copy of a value that was already fresh — an
  `array.new_data` cloned into another array. That copy is also what currently
  blocks hoisting for such callees, which is why the callee gate above has to
  stand on its own.

## Profiler caveat

The guest profiler's self counts are weighted by call frequency, not by time. On
this workload it reports `fl_unlink` at 24.3% of samples with the freelist
allocator, and `realloc` — a pointer bump — at 24.1% with `--allocator bump`,
while throughput is the same for both. Treat the guest profile as a call-count
map; take timing from ablation A/Bs.

## Next steps, in payoff order

1. Optimizer: globalize a constant aggregate in argument position, and stop the
   value-copy pass from copying a read-only global back into a local. Then the
   last-use copy of `resp.body` (item 3) and the CM-lift clone (item 2).
2. Router: `match_dynamic` allocates `ranges`, `PathParams`, and `RouteMatch` per
   hit while `match_static` returns a pre-built shell. A `ranges` buffer owned by
   the router (or a fixed inline capacity) would make dynamic hits allocation-free
   in the common case.
3. Host side, separately: the 68 µs floor against Axum's 40 µs is `wado serve`'s
   own per-request path, not guest code.
