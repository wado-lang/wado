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

## Typing a string-literal pattern's `Eq::eq` argument as `&String` (2026-08-19)

`x matches { "lit" }` lowers to `String^Eq::eq(&x, &"lit")`, and pattern
lowering typed that argument node as `String` rather than `&String` ("type_id
here is approximate"). The value-copy fold therefore defended the literal, so
every call built the backing array twice — `array.new_data` for the literal,
`array_new` + `array_copy` for the clone — and `const_object_globalization`
never saw the `&literal` shape it hoists. On gale-gen that is **1507 of the
module's 1522 array clones**; giving the node the callee's own `&String` deletes
all of them.

gale-gen, best of 3 back to back, `-O2`:

|        | globalization on (default) | `WADO_SKIP_PASS=nir/const_object_globalization` |
| ------ | -------------------------- | ----------------------------------------------- |
| before | 154.09 KB/s                | 155.32 KB/s                                     |
| after  | 143.72 KB/s                | 150.58 KB/s                                     |

Slower either way, and the WIR is otherwise identical — same 1657 functions,
same 122933 lines, only the literal expressions replaced. Fixing the copy moves
1175 more constants into module globals (1250 -> 2425), and under the copying
collector every one is permanently live and re-copied at each cycle; GC is ~20%
of this benchmark (`--collector null` runs at 188 KB/s). Even with the pass
skipped the clone removal does not pay: `String^Eq::eq` is 8.1% of the profile,
but 3.6 points are `TreeMap<String, i32>::find_index` comparing two runtime
strings and under 0.5 points are literal sites.

Reverted. Generalizes: on a copying collector a hoist buys a cheaper use and
sells a permanent root, so a constant that is small and rarely read loses.
`const_object_globalization` is already near break-even on gale-gen without the
extra hoists.

## Sharing list elements instead of deep-copying them (2026-08-20)

`value_copy_demote` refuses any helper reaching a variant deep copy, which on
gale-gen covers `List<Element>` — 9.6% of the profile.

Lifting that gate is a strict no-op: the candidate set grows from 26 helpers to
44 and demotes the same 3, for byte-identical Wasm (`WADO_TRACE=demote`). It
never blocked these copies — `demote_candidate` retargets only a
`let x = $value_copy$T(arg)` binding, and gale's are struct fields
(`SllConfig { ..*c, pos }`).

Extending the pass to expression position is the obvious next step and the wrong
one. Forcing every element clone shallow — the upper bound of any sharing scheme
— runs gale-gen 3x slower:

|                     | `copying` | `null` (no GC) |
| ------------------- | --------- | -------------- |
| deep copy (default) | 171 KB/s  | 208 KB/s       |
| shared elements     | 60 KB/s   | 260 KB/s       |

Sharing is the faster program and the slower one under a collector: a deep-copied
element dies in the nursery and costs a copying collector nothing, a shared one
is live from every config that borrowed it. GC goes from 18% of the run to 77%.

Generalizes: price a "share instead of copy" idea against the live set it
creates, not the allocations it removes.

## Making an expression-position labeled block a break target (2026-08-20)

`Analyzer::walk_expr` pushes no exit entry for a value-producing labeled block,
so every `break` it holds resolves to "every local live". Pushing one is more
precise and does unlock moves — `build_sll_node`'s loop binding among them.

It costs more than it returns. The precise live set also admits place moves the
coarse one refused, and a place move marks its root moved, which retires every
immutable share of that root: `Rebuild::rebuild` trades three shares of a member
tuple for one element move. gale-gen, best of four alternating pairs:
**182.6 KB/s without the change vs 168.0 with it**, and the golden corpus grows
from 34 changed fixtures to 132.

Not pursued. Making it pay needs the share analysis keyed on liveness (the
standing item in [WEP: Ownership Analysis](../docs/wep-2026-05-21-resource-ownership.md)),
so the two elisions can be priced against each other instead of racing.
