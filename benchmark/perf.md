# Performance Dead Ends

Optimizations that were measured and did **not** pay off. Read before starting
performance work; add an entry whenever an A/B comes back flat.

Wall-clock A/B is the verdict. Profile shares only rank candidates — two
profiles have different sample counts, so a shifted percentage proves nothing.

```sh
wado run -O2 --profile guest,prof.json,1 benchmark/json_catalog/json_catalog.wado
for i in 1 2 3; do mise run json-catalog; done   # before and after
```

## i64 divisions in integer formatting (2026-08-16)

Decimal formatting cost three i64 divisions per digit: `count_digits_i64` ran
`t / 10` per digit just to count, `write_decimal_digits` ran both `% 10` and
`/ 10`. Cut to one (binary search over powers of ten; remainder recovered as
`temp - q * 10`).

json-catalog ser, best of 3: **1.532 ms → 1.530 ms**. Within noise — the spread
inside each group (before: 1.532–1.601) exceeded the difference between them.

Cranelift strength-reduces division by a constant into multiply-high + shift,
so no division was ever issued. The ~28% in these frames is the per-byte
`array.set` into the GC buffer and the surrounding buffer management
(`String::grow`, `push_str`), not arithmetic.

Reverted; the digit-boundary tests were kept. Generalizes to any "replace a
constant divide/modulo" idea in guest code.

## Bucketing `FieldSchema::lookup` by wire-name length (2026-08-16)

The synthesized `lookup` (`serde_synth.rs`) is a flat chain of
`__len == N && key[0] == b0 && …`. Bucketing by length, then dispatching on a
discriminating byte, mirrors `json_catalog_v2.wado`'s hand-written parser.

Not pursued. `lookup` is **0.71%** of the json-catalog profile (`next_field`
another 0.50%); it is a real function there, not inlined away. Also:

- Zero gain when name lengths are distinct — the `&&` chain already
  short-circuits on length. Only same-length-heavy structs benefit (`Event`:
  16 comparisons → 13).
- Byte-at-a-time is irreducible: WasmGC has no multi-byte load or compare over
  `Array<u8>` (`builtin.wado` has only `array_get_value_u8`/`array_copy`/`array_fill`).
- v2's discriminating-byte shortcut is unsound here — an unknown key matching
  on length and that byte would silently capture a real field's value.

Where the time actually is: `whitespace_end` 12.5% (`citm_catalog.json` is 71%
whitespace), and on ser `push_str` 6.6%, `write_plain_key` 5.1%,
`String::grow` 5.1%.

## Sharing list elements instead of deep-copying them (2026-08-20)

Lifting `value_copy_demote`'s variant-deep-copy gate is a no-op: the candidate
set grows from 26 helpers to 44 and demotes the same 3, for byte-identical Wasm
(`WADO_TRACE=demote`). It retargets only a `let x = $value_copy$T(arg)` binding,
and gale's `List<Element>` copies — 9.6% of the profile — are struct fields
(`SllConfig { ..*c, pos }`).

Extending the pass to expression position is the obvious next step and the wrong
one. Forcing every element clone shallow is the upper bound of any sharing
scheme:

|                     | `copying` | `null` (no GC) |
| ------------------- | --------- | -------------- |
| deep copy (default) | 171 KB/s  | 208 KB/s       |
| shared elements     | 60 KB/s   | 260 KB/s       |

A deep-copied element dies in the nursery and costs a copying collector nothing;
a shared one is live from everything that borrowed it. GC goes from 18% of the
run to 77%.

Generalizes: price a "share instead of copy" idea against the live set it
creates, not the allocations it removes.

The motivating copy is gone as of 2026-08-25: `SllConfig.elements` is a
`&List<Element>`, which shares a list that is already a root rather than its
elements, and gale-gen gained 32% (`package-gale/perf.md`).

## Making an expression-position labeled block a break target (2026-08-20)

`Analyzer::walk_expr` pushes no exit entry for a value-producing labeled block,
so every `break` it holds resolves to "every local live". Pushing one is more
precise and does unlock moves — `build_sll_node`'s loop binding among them.

A precise live set also admits place moves the coarse one refused, and a place
move marks its root moved, retiring every immutable share of that root:
`Rebuild::rebuild` trades three shares of a member tuple for one element move.
gale-gen, best of four alternating pairs, **182.6 KB/s without it vs 168.0**.

Pricing the two elisions against each other needs the share analysis keyed on
liveness, a standing item in
[WEP: Ownership Analysis](../docs/wep-2026-05-21-resource-ownership.md).

## Raising the `-O2` inline budget (2026-08-27)

The 13-instruction budget leaves 313 functions out of line in gale-gen, and
raising it to 20 pulls them in: gale-gen 98.9 → 95.9 ms/iter, cbor-twitter
serialize +7.8%, syntax-highlight +3.7%, sqlite-parse +3.4%, sieve +2.2%.

It is still a loss. json-twitter **serialize drops 17%** (638 → 530 MB/s),
mandelbrot goes slightly backwards, and every program pays for it in bytes:
`-Os` output grows 41% on gale-gen (1.29 → 1.81 MB), 43% on json-twitter, 16%
on syntax-highlight.

So the budget is not a dial with a better setting — a serializer's hot loop is
exactly what growing it damages. What the sweep says instead is that the
threshold is doing two jobs, admitting a callee and pricing a loop body, and a
gale-gen-shaped win needs the second priced apart from the first.
