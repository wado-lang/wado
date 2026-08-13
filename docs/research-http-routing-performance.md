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

All four are avoidable without changing what the benchmark measures. Items 1 and
4 are applied to `app.wado`, items 2 and 3 to the value-copy planner.

1. Constant `collect()` in the hot path: `"literal".bytes().collect()` allocated
   a `List` and four arrays every evaluation, because `FromIterator` for a list
   starts at capacity 0 and pushes one element at a time and the `Iterator` trait
   has no size hint to reserve from. The app now writes `b"application/json"`,
   which lowers to one `struct.new` over `array.new_data`. The two objects left
   per request are what constant globalization should remove; see below.
2. Redundant clone when lifting a CM string: the binding allocates a fresh array
   from linear memory and then clones it into another one before wrapping it in a
   `String`. The culprit is the newtype cast in `String { repr: bytes as
ByteArray, used: len }` — freshness read through a cast but the move side did
   not, so the cast alone forced the copy. Fixed, which also drops the second
   allocation in `String::substring`, the per-parameter path-capture cost.
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

For a change this size, isolate it instead — a CLI loop, best of three, is
stable to a few percent where the HTTP path is not:

- Globalizing the by-value constant argument (1000 calls per iteration,
  `#[inline(never)]` callee): 33.6k → 735.6k iterations/s, 29 ns to 1.3 ns per
  call. End to end that is ~2 of ~18 objects per request; the
  same-binary-different-compiler A/B measured +2.3% and +1.3% over 8 s slices,
  the right order of magnitude and still inside the floor.
- Moving the body field out of the dispatch result (1000 closure dispatches per
  iteration, each building a 64-byte body): 2.46k → 2.71k iterations/s, +10%,
  with every post-fix run above every pre-fix run. The loop pays for building
  the body too, so the removed copy is a large share of what is left.
- Moving through the newtype cast (1000 `substr_bytes` per iteration, the path
  parameter shape): 7.6k → 12.2k iterations/s. Halving the allocations per
  substring is worth ~1.5x on a loop that does nothing else.

## The constant is no longer rebuilt per request

`const_object_globalization` (WEP 2026-05-31) matched a qualifying constant in a
`let` binding or behind a `&`, but not in a by-value argument position — which is
where the header value sits, so it was rebuilt per request. The pass now matches
that shape too, gated on the callee's parameter being read-only in the callee's
own body, and the header value compiles to an eager `array.new_fixed` global read
straight into `Fields::append`: zero per-request allocations for the header.

A constant handed to a callee that writes its parameter used to get a
caller-side defensive copy of a value that was already fresh: `is_owned_value`
listed `StringLiteral` but not `BytesLiteral`, though both lower to the same
fresh aggregate over a packed array. That copy is gone too — which leaves the
callee gate above as the only thing standing between a hoisted constant and a
callee that writes it, exactly as the gate was written to be.

## Profiler caveat

The guest profiler's self counts are weighted by call frequency, not by time. On
this workload it reports `fl_unlink` at 24.3% of samples with the freelist
allocator, and `realloc` — a pointer bump — at 24.1% with `--allocator bump`,
while throughput is the same for both. Treat the guest profile as a call-count
map; take timing from ablation A/Bs.

## Next steps, in payoff order

1. Router: `match_dynamic` allocates `ranges`, `PathParams`, and `RouteMatch` per
   hit while `match_static` returns a pre-built shell. A `ranges` buffer owned by
   the router (or a fixed inline capacity) would make dynamic hits allocation-free
   in the common case.
2. Host side, separately: the 68 µs floor against Axum's 40 µs is `wado serve`'s
   own per-request path, not guest code.

# Follow-up, 2026-08-13

Same method, same host shape. Re-run of the `base` / `floor` ablation with Hono
on Bun as a fourth row, so the guest handler's cost is read against the
reference we are actually chasing rather than against Axum.

| Request                         |   base | byteslice |  floor |    Bun |
| ------------------------------- | -----: | --------: | -----: | -----: |
| `GET /user`                     | 13,910 |    14,325 | 19,849 | 18,419 |
| `GET /user/lookup/username/hey` | 13,147 |    13,379 | 20,556 | 18,336 |
| `GET /event/abcd1234/comments`  | 13,054 |    14,055 | 19,944 | 17,069 |
| `POST /event/abcd1234/comment`  | 12,419 |    12,990 | 20,573 | 17,294 |
| `GET /static/index.html`        | 13,403 |    14,247 | 19,053 | 16,508 |

Four servers share three cores here, so the absolute numbers sit well under a
run that hosts one server; only the ratios within the run mean anything.

**The floor is 8-19% _above_ Bun, and `base` is 20-28% below it.** The whole gap
to Bun is the guest handler — route match, JSON body, parameter capture — not
`wado serve`'s HTTP path. That reverses the priority the section above set:
until the guest handler is cheaper, host-side work on the 68 µs floor cannot
close anything, because the floor already wins.

`byteslice` is `base` with the response body kept as the `ByteSlice` that
`json::to_bytes` returns instead of `to_list()`-copied into a `List<u8>`
(applied to `app.wado`): faster on all five shapes, +1.8% to +7.7%.

Two more things settled:

- **`Config::gc_heap_initial_size` is not the lever.** The copying collector's
  semi-space stays small because growth only triggers when the post-GC live set
  crowds the heap, and this workload's live set is tiny — so the theory was that
  collections run constantly and a pre-sized heap would amortize them. Measured
  at 16 MB and 64 MB against an unset default, interleaved: every delta inside
  noise. Not applied.
- **A field's wire name was rebuilt on every serialize.** `wire_name` fed
  `apply_case` unconditionally; with a constant `policy` the optimizer folds the
  call's *result* to a constant but cannot delete the call, because `apply_case`
  allocates through bounds-checked writes and so carries `may_trap`. The
  leftover call re-allocated its `String` argument per field of every serialized
  struct. `wire_name` now tests `Identity` at the call site so a constant policy
  prunes the branch. This is a `core:serde` fix, not a benchmark one.

Where the guest cost sits, measured in a CLI loop (release JIT, per request):
route match 1.03 µs, JSON body build 2.70 µs, both plus handler dispatch
4.24 µs. `json::to_bytes` on a pre-built value is 0.87 µs of that, against
0.27 µs for a bare `String::with_capacity(128)` plus a `push_str` of the same
47 bytes — so roughly 0.6 µs per request is serializer machinery over what
writing the bytes costs. That, and the router's per-hit allocations, are what
the next round should take.
