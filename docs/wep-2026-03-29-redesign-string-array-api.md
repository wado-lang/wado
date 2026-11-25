# WEP: String and List API Conventions

## Context

Wado's `String` and `List<T>` read as Rust's `String` / `&str` and `Vec<T>` /
`&[T]` do, so a Rust developer finds the method they expect under the name they
expect. What Rust ties to ownership and lifetimes has no counterpart.

[`core:prelude`](./stdlib-core-prelude.md) lists what is offered. This WEP
holds the rules the names follow, and the Rust methods that were left out.

## Decision

### Naming

A method with the semantics of a Rust method takes the Rust name. A method
whose semantics differ takes a name of its own: a Rust name over different
semantics misleads the reader who knows Rust.

- `as_bytes` returns a `ByteSlice`, a view, as Rust's returns `&[u8]`. A method
  that copies the bytes out does not take the name.
- `truncate_chars(n)` counts characters, which no Rust method does, so the
  suffix says so. `truncate(n)` counts bytes, as Rust's does.
- `sorted` and `sorted_by` return a new list. `sort` and `sort_by` sort in
  place, as Rust's do.
- `List::filled(n, x)` is `vec![x; n]` as a static method.
- There is no `String::concat(a, b)`: Rust has none, and `a + b` says it.

A name that is replaced is removed outright, with no deprecation period.

### Byte Indices

A `String` method that takes or returns an index means a byte index, as in
Rust and as `len()` does. `find` returns one, and `insert`, `remove`,
`truncate` and `substr_bytes` take one. An index must sit on a character
boundary.

Three helpers turn a byte offset into an index those methods accept:
`is_char_boundary`, `floor_char_boundary` and `ceil_char_boundary`.
`s.substr_bytes(0, s.floor_char_boundary(200))` never traps, where
`s.substr_bytes(0, 200)` does on the first input that puts a multibyte
character across byte 200. `StrSlice` and every `AsStrSlice` carry all three.

An index past `len()` clamps to `len()`: a budget longer than the text is the
common case, and the bytes past `len()` are spare capacity, so the range check
is needed anyway. A negative index traps, as `truncate` does, rather than
clamp a caller's bug to 0.

### Preconditions

A method states what it requires as a `contract`, and a call marked `unchecked`
skips the check. No public method name carries `_unchecked`.
[Contract Checks](./wep-2026-10-06-contract-checks.md) holds the mechanism.

A write that leaves a `String` invalid UTF-8 between two steps has no public
form. A checked version of a single-byte write cannot validate UTF-8, since
what the caller means depends on the bytes around it, and a half-checked
method would promise more than it holds. Those operations are the `internal_*`
methods, visible to the standard library alone.

### Pattern Arguments

Rust's `contains`, `find` and `split` take a `Pattern`, which a `&str`, a
`char` and a closure all implement. Wado has no such trait:

- A text pattern is any `AsStrSlice`: `contains(pat)`, `find(pat)`,
  `split(sep)`, `starts_with(pat)`.
- A character has methods of its own: `contains_char(ch)` and
  `find_char(pred)`.

### Rust Methods Left Out

`List<T>`:

| Rust                                | Instead                                 |
| ----------------------------------- | --------------------------------------- |
| `sort_by_key(f)`                    | `sort_by`                               |
| `sort_unstable()`                   | `sort()` is stable; no unstable sibling |
| `binary_search(&x)`                 | `TreeMap` / `TreeSet`                   |
| `dedup()`, `dedup_by(f)`            | `TreeSet`                               |
| `retain(pred)`                      | `.iter().filter().collect()`            |
| `split_at(i)`, `split_off(i)`       | a `Slice` of each part, and `truncate`  |
| `drain(..)`                         | an iterator                             |
| `rotate_left(n)`, `rotate_right(n)` | niche                                   |
| `resize(n, val)`                    | `truncate` and `extend`                 |
| `as_mut_slice()`                    | no borrowing distinction                |
| `into_boxed_slice()`, `leak()`      | no ownership model                      |

`String`:

| Rust                               | Instead                                                   |
| ---------------------------------- | --------------------------------------------------------- |
| `rsplit(pat)`, `rsplitn(n, pat)`   | niche; `rsplit_once` covers the common case               |
| `to_lowercase()`, `to_uppercase()` | `core:icu`'s `Locale`: case mapping depends on the locale |
| `as_str()`                         | no borrowing distinction                                  |
| `into_bytes()`                     | no ownership transfer                                     |
| `drain(..)`                        | an iterator                                               |
| `retain(pred)`                     | `.chars().filter().collect()`                             |
| `split_off(i)`                     | niche                                                     |
| `encode_utf16()`                   | not needed for Wasm and WASI                              |

## Consequences

- A Rust developer's knowledge transfers, and where it does not, the name says
  so.
- A character-boundary trap can surprise a reader unfamiliar with UTF-8. The
  boundary helpers are the answer to it.
