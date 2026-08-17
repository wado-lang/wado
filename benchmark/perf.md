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
  `Array<u8>` (`builtin.wado` has only `array_get_u8`/`array_copy`/`array_fill`).
- v2's discriminating-byte shortcut is unsound here — an unknown key matching
  on length and that byte would silently capture a real field's value.

Where the time actually is: `whitespace_end` 12.5% (`citm_catalog.json` is 71%
whitespace), and on ser `push_str` 6.6%, `write_plain_key` 5.1%,
`String::grow` 5.1%.
