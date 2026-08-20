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
