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

Two source changes (a byte-string literal for the `content-type` value,
`write_raw` for the body) and one optimizer change (globalizing a constant passed
by value) remove 6 of those objects. Each is verified in the WIR; end to end they
measure a few percent, which is this host's noise floor — see below.

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
and 4 are applied to `app.wado`, item 3 to the value-copy planner; item 2 is the
compiler work left.

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
3. `resp.body` copied on last use: `body_tx.write(resp.body)` compiled to an
   `array_new` + `array_copy` of the body even though `resp` is dead afterwards.
   Two freshness gaps caused it — an indirect (closure) call counted as borrowed,
   and so did the value of an `if` / `if let` expression — and `resp` is bound
   from exactly that shape. Both are fixed; the body now reaches
   `cm_lower_list_u8` straight out of the handler's struct.
4. `StreamWritable::write` takes `List<u8>` by value, so the body was deep-copied
   into the CM lowering. The app now calls `write_raw(resp.body.as_slice())`,
   which lowers the slice directly. On its own that left the value copy of
   `resp.body` (item 3) in place — the field read copies before `as_slice()` sees
   anything — so the two only pay off together.

## Measured levers

The payoff is proportional to objects removed, not to instructions removed:
removing all five header objects by hand measured +8.2% / +9.7% on the two
routes (28,950 → 31,325 and 25,207 → 27,651 req/s, best of 5 rounds), of which
only ~1 µs is compute and the rest is GC that no longer runs.

Items 1 and 4 together — the byte literal plus `write_raw` — measured +2.3% to
+6.2% across three request shapes, and a further hand-hoist of the literal into a
`global` measured +1% to +8% on top of that.

## Noise floor

Those single-digit deltas need a scale: an A/B of two servers running the _same_
binary on the _same_ app, interleaved the same way over 5 rounds, spreads -4.2%
to +0.4% on this host. So anything under ~5% here is a hint, not a result —
including the +2 to +6% above. What is certain is the WIR: the objects are gone.

For a change this size, isolate it instead. A CLI loop over the by-value
constant argument alone (1000 calls per iteration, `#[inline(never)]` callee)
goes from 33.6k to 735.6k iterations/s once the constant is globalized — 29 ns to
1.3 ns per call. End to end that saving is ~2 of ~18 objects per request: the
same-binary-different-compiler A/B measured +2.3% and +1.3% over 8 s slices,
which is the right order of magnitude and still inside the floor.

## The constant is no longer rebuilt per request

`const_object_globalization` (WEP 2026-05-31) matched a qualifying constant in a
`let` binding or behind a `&`, but not in a by-value argument position — which is
where the header value sits, so it was rebuilt per request. The pass now matches
that shape too, gated on the callee's parameter being read-only in the callee's
own body, and the header value compiles to an eager `array.new_fixed` global read
straight into `Fields::append`: zero per-request allocations for the header.

One related copy remains: a constant handed to a callee that writes its
parameter still gets a caller-side defensive copy of a value that was already
fresh — an `array.new_data` cloned into another array. That copy is also what
currently blocks hoisting for such callees, which is why the callee gate above
has to stand on its own.

## Profiler caveat

The guest profiler's self counts are weighted by call frequency, not by time. On
this workload it reports `fl_unlink` at 24.3% of samples with the freelist
allocator, and `realloc` — a pointer bump — at 24.1% with `--allocator bump`,
while throughput is the same for both. Treat the guest profile as a call-count
map; take timing from ablation A/Bs.

## Next steps, in payoff order

1. Optimizer: stop the value-copy planner from copying a read-only global back
   into a local, and drop the CM-lift clone (item 2).
2. Router: `match_dynamic` allocates `ranges`, `PathParams`, and `RouteMatch` per
   hit while `match_static` returns a pre-built shell. A `ranges` buffer owned by
   the router (or a fixed inline capacity) would make dynamic hits allocation-free
   in the common case.
3. Host side, separately: the 68 µs floor against Axum's 40 µs is `wado serve`'s
   own per-request path, not guest code.
