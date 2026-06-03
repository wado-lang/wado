# WEP: String API — checked / unchecked / internal Discipline

## Status

Draft

## Context

Wado has no `unsafe` keyword. At the same time, the standard library and a few performance-critical user paths need direct byte-level access to `String` storage that bypasses UTF-8 invariants. Two questions follow:

1. How do we expose dangerous low-level operations without an `unsafe` escape hatch?
2. How do we keep the `String` API aligned with Rust without inheriting names whose semantics no longer match in a value-semantics world?

The current `String` API (`wado-compiler/lib/core/prelude/string.wado`) mixes three categories of methods without a consistent naming rule:

- Bounds-checked, panicking methods (`truncate`, `insert`, `insert_str`).
- Bounds-unchecked methods that look ordinary (`get_byte`, `set_byte`, `substr_bytes`).
- Methods prefixed with `internal_` (`internal_raw_bytes`, `internal_reserve_uninit`, `internal_append_from_memory`, `internal_from_utf8_raw`).

There are also Rust-named methods whose semantics differ from Rust because of Wado's value semantics:

- `as_bytes() -> List<u8>` — Rust returns `&[u8]` (zero-copy borrow); Wado returns an owned, deep-copied `List<u8>`.
- `String::concat(a, b)` — Rust has no static `String::concat`; the closest is `[a, b].concat()` on a slice.

WEP-2026-03-29 (Redesign String and List APIs) catalogs the Rust-alignment of method names but does not address (a) the checked/unchecked discipline, (b) the value-semantics mismatches above, or (c) the role of `internal_*` primitives. This WEP fills those gaps and overrides the relevant rows of WEP-2026-03-29.

Since Wado has no external users, this WEP removes deprecated names outright instead of going through a deprecation period.

## Decision

### Three-Layer Naming Discipline

Every byte-level or invariant-sensitive `String` operation falls into exactly one layer:

| Layer     | Naming          | Behavior                                                                | Audience               |
| --------- | --------------- | ----------------------------------------------------------------------- | ---------------------- |
| checked   | `foo`           | Validates inputs with `assert`, then delegates to the unchecked variant | General users          |
| unchecked | `foo_unchecked` | No validation. Caller owns every precondition (bounds, UTF-8, etc.)     | Performance-critical   |
| internal  | `internal_foo`  | Stdlib-only low-level primitive. Leaks `builtin::*` types               | Stdlib implementations |

Rules:

- A checked method is exactly `assert ...; self.foo_unchecked(...)`. No duplicated logic.
- If a useful `assert` cannot be defined cheaply (e.g. UTF-8 validity from a single byte write), only the unchecked form is provided.
- `internal_*` methods are public today but should be hidden once a stdlib-only visibility annotation exists. Document each one with the precondition the caller must uphold.

### Rust Naming Alignment

- When a Wado method has the same semantics as a Rust method, use the Rust name.
- When the semantics differ, use a Wado-specific name. Reusing a Rust name with different semantics is forbidden because it silently misleads readers familiar with Rust.

The following names violate this rule today and are removed:

| Method                 | Rust meaning                       | Wado meaning today                      | Replacement                                |
| ---------------------- | ---------------------------------- | --------------------------------------- | ------------------------------------------ |
| `as_bytes()`           | `&[u8]` (zero-copy borrow)         | Owned `List<u8>` (deep copy)            | Removed. Use `bytes().collect()` if needed |
| `String::concat(a, b)` | No such method (closest: slice op) | Static two-arg concatenation            | Removed. Use `a + b`                       |
| `get_byte(i)`          | `as_bytes()[i]` is bounds-checked  | Bounds-unchecked direct access          | `get_byte_unchecked(i)`                    |
| `set_byte(i, v)`       | `as_bytes_mut()` is `unsafe`       | Bounds-unchecked, UTF-8-unchecked write | `set_byte_unchecked(i, v)`                 |
| `truncate_bytes(n)`    | n/a                                | Deprecated alias for `truncate`         | Removed                                    |

### Byte-Level Access Layer

Byte-level access is reserved for performance-critical code. Only unchecked forms are provided; users who want safe byte access use the `bytes()` iterator.

```wado
impl String {
    // Read/write the byte at `i`. Caller must ensure `0 <= i < len()`.
    // For writes, caller must also preserve the UTF-8 invariant.
    pub fn get_byte_unchecked(&self, i: i32) -> u8;
    pub fn set_byte_unchecked(&mut self, i: i32, v: u8);
}
```

Rationale: a "checked" `set_byte` cannot meaningfully validate the UTF-8 invariant from a single-byte write (the caller's intent depends on surrounding bytes), so a half-checked variant would create a false sense of safety. This mirrors why Rust gates `as_bytes_mut` behind `unsafe`.

### Bulk Append Primitives

The recurring stdlib/benchmark pattern

```wado
let start = buf.internal_reserve_uninit(n);
for let mut i = 0; i < n; i += 1 {
    buf.set_byte(start + i, src[i]);
}
```

is replaced by dedicated bulk methods that compile to a single `array_copy`:

```wado
impl String {
    // Append all bytes from `bytes`. Caller must ensure the result is valid UTF-8.
    pub fn push_bytes_unchecked(&mut self, bytes: List<u8>);

    // Append the byte range `[start, end)` of `s`. Caller must ensure the
    // range is in bounds, on UTF-8 boundaries, and that the result is valid UTF-8.
    pub fn push_str_range_unchecked(&mut self, s: String, start: i32, end: i32);
}
```

After this, the `internal_reserve_uninit` + `set_byte_unchecked` loop is needed only for **right-to-left** writes, which `array_copy` cannot express. The remaining call sites are integer and float formatters (`write_decimal_digits`, `fpfmt`). `internal_reserve_uninit` is therefore retained, with documentation pinning down its niche.

### Checked / Unchecked Pairs

Methods whose validation is genuinely useful and separable get a checked/unchecked pair. The checked form is the public default; the unchecked form is for hot loops where the caller has already proven the precondition.

| Checked                           | Unchecked                            | Validation in checked form                       |
| --------------------------------- | ------------------------------------ | ------------------------------------------------ |
| `truncate(byte_len)`              | `truncate_unchecked(byte_len)`       | `byte_len >= 0`, on UTF-8 boundary               |
| `insert(i, ch)`                   | `insert_unchecked(i, ch)`            | `0 <= i <= len()`, on UTF-8 boundary             |
| `insert_str(i, s)`                | `insert_str_unchecked(i, s)`         | Same as `insert`                                 |
| `remove(i) -> char`               | `remove_unchecked(i) -> char`        | `0 <= i < len()`, on UTF-8 boundary              |
| `substr_bytes(start, end)`        | `substr_bytes_unchecked(start, end)` | `0 <= start <= end <= len()`, both on boundaries |
| `char_at_byte(i) -> Option<char>` | `char_at_byte_unchecked(i) -> char`  | (checked form keeps `Option` for empty/past-end) |

`pop()` already returns `Option<char>` and the empty-check is essentially free, so no `pop_unchecked` is added.

### New Rust-Aligned Helper

```wado
impl String {
    // True if `i` is 0, len(), or the start of a UTF-8 sequence in self.
    pub fn is_char_boundary(&self, i: i32) -> bool;
}
```

This gives users a cheap way to validate an index before calling an `_unchecked` method, mirroring `str::is_char_boundary` in Rust.

### `internal_*` Inventory

| Method                                         | Purpose                                                      | Disposition                                       |
| ---------------------------------------------- | ------------------------------------------------------------ | ------------------------------------------------- |
| `internal_raw_bytes() -> builtin::array<u8>`   | Hand the underlying GC array to stdlib for zero-copy reads   | Keep                                              |
| `internal_from_utf8_raw(bytes, len) -> String` | Construct a `String` from a known-valid `builtin::array<u8>` | Keep                                              |
| `internal_append_from_memory(ptr, len)`        | Lift a CM string from linear memory                          | Keep                                              |
| `internal_reserve_uninit(n) -> i32`            | Reserve `n` bytes and advance `used` in one step             | Keep, but document as "right-to-left writes only" |

A future visibility annotation (e.g. `#[stdlib_only]`) should restrict these to stdlib code; until then, naming and doc-comments carry the discipline.

## Consequences

### Positive

- A reader can predict the safety story of a `String` method from its name alone.
- The standard library can use `_unchecked` paths in hot loops without touching `internal_*` types.
- Removing Rust-named methods with mismatched semantics prevents silent misuse by Rust-trained developers.
- Dedicated bulk-append methods replace error-prone manual loops in benchmarks and JSON parsing.

### Negative

- Wado now has Rust-named methods that Rust does not have (`get_byte_unchecked`, `push_bytes_unchecked`, `push_str_range_unchecked`). They are necessary because Wado lacks `unsafe { … }` and zero-copy slicing, and the `_unchecked` suffix advertises the trade-off explicitly.
- Stdlib code remains responsible for UTF-8 validity when calling the unchecked layer; bugs surface as runtime corruption rather than compile errors.

### Overrides of WEP-2026-03-29

The following rows in WEP-2026-03-29's mapping table are superseded by this WEP:

- `as_bytes()` — removed (was: NEW returning `List<u8>`).
- `String::concat(a, b)` — removed (was: kept as Wado-specific).
- `String::get_byte(i)` / `set_byte(i, v)` — removed; replaced by `get_byte_unchecked` / `set_byte_unchecked` (was: kept as low-level byte access).
- `truncate_bytes` — removed outright (was: rename with deprecation period).
- `insert`, `insert_str`, `remove`, `truncate` — gain `_unchecked` siblings.

WEP-2026-03-29 itself remains a useful catalog of Rust ↔ Wado method names; only the rows above change.

## Implementation TODOs

P0 — removals and renames

- [ ] Remove `String::as_bytes`. Update call sites to `bytes().collect()` if any exist.
- [ ] Remove `String::concat(a, b)` static method. Update call sites to use `+`.
- [ ] Remove `String::truncate_bytes` deprecated alias.
- [ ] Rename `get_byte` → `get_byte_unchecked`; rename `set_byte` → `set_byte_unchecked`. Update all call sites.

P1 — checked / unchecked pairs

- [ ] Split `truncate` into `truncate` (assert) + `truncate_unchecked`.
- [ ] Split `insert` and `insert_str` similarly.
- [ ] Split `remove` similarly.
- [ ] Add `assert` to `substr_bytes` and provide `substr_bytes_unchecked`.
- [ ] Add `char_at_byte_unchecked(i) -> char` (keep `char_at_byte` returning `Option<char>`).

P1 — bulk append primitives

- [ ] Add `String::push_bytes_unchecked(bytes: List<u8>)`.
- [ ] Add `String::push_str_range_unchecked(s: String, start: i32, end: i32)`.
- [ ] Replace the `internal_reserve_uninit` + `set_byte_unchecked` loop in:
  - `wado-compiler/lib/core/json.wado` (3 sites)
  - `benchmark/json_catalog/*.wado`
  - `benchmark/json_twitter/*.wado`
  - `benchmark/json_canada/*.wado`
  - `benchmark/sqlite_parse/*.wado`
  - `benchmark/syntax_highlight/*.wado`

P2 — Rust-aligned helper

- [ ] Add `String::is_char_boundary(i) -> bool`.

P3 — internal layer

- [ ] Document each `internal_*` method's caller-side preconditions.
- [ ] Annotate `internal_reserve_uninit` as "right-to-left writes only" and audit remaining call sites.
- [ ] Once a stdlib-only visibility annotation exists, restrict `internal_*` to stdlib modules.

P4 — documentation

- [ ] Update `docs/stdlib-core-prelude.md` (auto-generated).
- [ ] Update `docs/cheatsheet.md` if byte-level examples appear there.
- [ ] Cross-link this WEP from `docs/wep-2026-03-29-redesign-string-array-api.md`.
