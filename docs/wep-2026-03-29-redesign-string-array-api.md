# WEP: Redesign String and List APIs

## Status

Draft (partially superseded by [WEP-2026-05-16: String API checked/unchecked discipline](./wep-2026-05-16-string-checked-unchecked-discipline.md), which overrides the rows for `as_bytes`, `String::concat`, `get_byte`/`set_byte`, and `truncate_bytes`, and introduces `_unchecked` siblings for `truncate`/`insert`/`insert_str`/`remove`.)

## Context

Wado's `String` and `List<T>` are designed to feel familiar to Rust developers, but many commonly used methods from Rust's `String`/`&str` and `Vec<T>`/`&[T]` are missing or named differently. This proposal catalogs the gaps and adds the missing pieces.

Wado intentionally omits Rust concepts tied to ownership and lifetimes (`as_str()`, `as_mut_slice()`, `into_boxed_str()`, `Cow`, etc.). Everything else should be available.

## Decision

### List vs Rust's `Vec<T>` / `&[T]`

#### Mapping Table

"—" means intentionally excluded (see rationale below the table).

| Rust `Vec<T>` / `&[T]`   | Wado `List<T>`                           | Status                                  |
| ------------------------ | ---------------------------------------- | --------------------------------------- |
| `Vec::new()`             | `[]` (literal) or `List::<T>::default()` | OK                                      |
| `Vec::with_capacity(n)`  | `List::<T>::with_capacity(n)`            | OK                                      |
| `vec![x; n]`             | `List::filled(n, x)`                     | OK                                      |
| `len()`                  | `len()`                                  | OK                                      |
| `is_empty()`             | `is_empty()`                             | OK                                      |
| `v[i]` (index)           | `arr[i]`                                 | OK                                      |
| `v[i] = x` (assign)      | `arr[i] = x`                             | OK                                      |
| `get(i)`                 | `get(i)`                                 | OK                                      |
| `first()`                | `first()`                                | NEW                                     |
| `last()`                 | `last()`                                 | OK                                      |
| `push(x)`                | `push(x)`                                | RENAME from `append`                    |
| `pop()`                  | `pop()`                                  | OK                                      |
| `insert(i, x)`           | `insert(i, x)`                           | NEW                                     |
| `remove(i)`              | `remove(i)`                              | NEW                                     |
| `swap(a, b)`             | `swap(a, b)`                             | NEW                                     |
| `truncate(len)`          | `truncate(len)`                          | OK                                      |
| `clear()`                | `clear()`                                | NEW                                     |
| `extend(iter)`           | `extend(iter)`                           | NEW                                     |
| `extend_from_slice(s)`   | —                                        | use `extend`                            |
| `contains(&x)`           | `contains(&x)`                           | NEW                                     |
| `iter()`                 | `iter()`                                 | OK                                      |
| `sort()`                 | `sort()`                                 | OK                                      |
| `sort_by(cmp)`           | `sort_by(cmp)`                           | OK                                      |
| `sort_by_key(f)`         | —                                        | use `sort_by`                           |
| `sort_unstable()`        | —                                        | `sort()` is stable; unstable not needed |
| `binary_search(&x)`      | —                                        | use `TreeMap` / `TreeSet` instead       |
| `reverse()`              | `reverse()`                              | NEW                                     |
| `dedup()`                | —                                        | use `TreeSet`                           |
| `dedup_by(f)`            | —                                        | use `TreeSet`                           |
| `retain(pred)`           | —                                        | use `.iter().filter().collect()`        |
| `windows(n)`             | `windows(n)`                             | NEW                                     |
| `chunks(n)`              | `chunks(n)`                              | NEW                                     |
| `repeat(n)`              | `repeat(n)`                              | NEW                                     |
| `concat()` / `join(sep)` | `join(sep)`                              | NEW                                     |
| `split_at(i)`            | —                                        | use `slice` + `truncate`                |
| `split_off(i)`           | —                                        | use `slice` + `truncate`                |
| `drain(..)`              | —                                        | complex; use iterator                   |
| `rotate_left(n)`         | —                                        | niche                                   |
| `rotate_right(n)`        | —                                        | niche                                   |
| `resize(n, val)`         | —                                        | use `truncate` + `extend`               |
| `capacity()`             | `capacity()`                             | NEW                                     |
| `reserve(n)`             | `reserve(n)`                             | NEW                                     |
| `shrink_to_fit()`        | `shrink_to_fit()`                        | NEW                                     |
| `as_slice()`             | —                                        | no borrowing distinction                |
| `as_mut_slice()`         | —                                        | no borrowing distinction                |
| `into_boxed_slice()`     | —                                        | no ownership transfer                   |
| `leak()`                 | —                                        | no ownership model                      |

Also available via existing Wado features:

| Rust                           | Wado equivalent                                    |
| ------------------------------ | -------------------------------------------------- |
| `v[start..end]`                | `arr[start..end]` (range indexing, WEP-2026-03-03) |
| `v.iter().filter(f).collect()` | same (Iterator trait)                              |
| `v.iter().map(f).collect()`    | same                                               |
| `v.iter().fold(init, f)`       | same                                               |
| `v.iter().find(pred)`          | same                                               |
| `v.iter().position(pred)`      | same                                               |
| `v.iter().any(pred)`           | same                                               |
| `v.iter().all(pred)`           | same                                               |
| `v.iter().enumerate()`         | same                                               |
| `v.iter().take(n)`             | same                                               |
| `v.iter().skip(n)`             | same                                               |
| `v.iter().chain(other)`        | same                                               |
| `v.iter().zip(other)`          | same                                               |
| `v.iter().count()`             | same                                               |
| `v.iter().sum()`               | `arr.iter().sum()` (on ListIter)                   |
| `v.iter().min()`               | `arr.iter().min()` (on ListIter)                   |
| `v.iter().max()`               | `arr.iter().max()` (on ListIter)                   |

#### New List Methods — Signatures

```wado
impl<T> List<T> {
    pub fn first(&self) -> Option<T>;
    pub fn push(&mut self, value: T);                       // rename from append
    pub fn insert(&mut self, index: i32, value: T);
    pub fn remove(&mut self, index: i32) -> T;
    pub fn swap(&mut self, a: i32, b: i32);
    pub fn clear(&mut self);
    pub fn capacity(&self) -> i32;
    pub fn reserve(&mut self, additional: i32);
    pub fn shrink_to_fit(&mut self);
    pub fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I);
    pub fn reverse(&mut self);
    pub fn repeat(&self, n: i32) -> List<T>;
    pub fn windows(&self, size: i32) -> WindowsIter<T>;
    pub fn chunks(&self, size: i32) -> ChunksIter<T>;
}

impl<T: Eq> List<T> {
    pub fn contains(&self, value: &T) -> bool;
}

impl<T: Display> List<T> {
    pub fn join(&self, separator: String) -> String;
}
```

### String vs Rust's `String` / `&str`

#### Mapping Table

| Rust `String` / `&str`               | Wado `String`                               | Status                                                                                                            |
| ------------------------------------ | ------------------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| `String::new()`                      | `""` (literal) or `String::default()`       | OK                                                                                                                |
| `String::with_capacity(n)`           | `String::with_capacity(n)`                  | OK                                                                                                                |
| `len()`                              | `len()` (byte length)                       | OK                                                                                                                |
| `is_empty()`                         | `is_empty()`                                | OK                                                                                                                |
| `push(ch)`                           | `push(ch)`                                  | RENAME from `append_char`                                                                                         |
| `push_str(s)`                        | `push_str(s)`                               | RENAME from `append`                                                                                              |
| `pop()`                              | `pop()`                                     | NEW                                                                                                               |
| `insert(i, ch)`                      | `insert(i, ch)` + `insert_unchecked`        | SUPERSEDED by WEP-2026-05-16 (checked/unchecked pair, byte index)                                                 |
| `insert_str(i, s)`                   | `insert_str(i, s)` + `insert_str_unchecked` | SUPERSEDED by WEP-2026-05-16                                                                                      |
| `remove(i)`                          | `remove(i)` + `remove_unchecked`            | SUPERSEDED by WEP-2026-05-16 (checked/unchecked pair, returns char)                                               |
| `truncate(n)`                        | `truncate(n)` + `truncate_unchecked(n)`     | SUPERSEDED by WEP-2026-05-16 (`truncate_bytes` removed outright; checked/unchecked pair)                          |
| `clear()`                            | `clear()`                                   | NEW                                                                                                               |
| `chars()`                            | `chars()`                                   | OK                                                                                                                |
| `bytes()`                            | `bytes()`                                   | OK                                                                                                                |
| `char_indices()`                     | `char_indices()`                            | NEW                                                                                                               |
| `contains(pat)`                      | `contains(pat)`                             | NEW                                                                                                               |
| `starts_with(pat)`                   | `starts_with(pat)`                          | NEW                                                                                                               |
| `ends_with(pat)`                     | `ends_with(pat)`                            | NEW                                                                                                               |
| `find(pat)`                          | `find(pat)`                                 | NEW                                                                                                               |
| `rfind(pat)`                         | `rfind(pat)`                                | NEW                                                                                                               |
| `split(pat)`                         | `split(sep)`                                | NEW (returns iterator)                                                                                            |
| `splitn(n, pat)`                     | `splitn(n, sep)`                            | NEW (returns iterator)                                                                                            |
| `rsplit(pat)`                        | —                                           | niche                                                                                                             |
| `rsplitn(n, pat)`                    | —                                           | niche                                                                                                             |
| `split_whitespace()`                 | `split_whitespace()`                        | NEW (returns iterator)                                                                                            |
| `lines()`                            | `lines()`                                   | NEW (returns iterator)                                                                                            |
| `trim()`                             | `trim()`                                    | OK                                                                                                                |
| `trim_start()`                       | `trim_start()`                              | OK                                                                                                                |
| `trim_end()`                         | `trim_end()`                                | OK                                                                                                                |
| `trim_ascii()`                       | `trim_ascii()`                              | OK                                                                                                                |
| `trim_ascii_start()`                 | `trim_ascii_start()`                        | OK                                                                                                                |
| `trim_ascii_end()`                   | `trim_ascii_end()`                          | OK                                                                                                                |
| `replace(from, to)`                  | `replace(from, to)`                         | NEW                                                                                                               |
| `replacen(from, to, n)`              | `replacen(from, to, n)`                     | NEW                                                                                                               |
| `to_lowercase()`                     | `to_lowercase()`                            | NEW (Unicode)                                                                                                     |
| `to_uppercase()`                     | `to_uppercase()`                            | NEW (Unicode)                                                                                                     |
| `to_ascii_lowercase()`               | `to_ascii_lowercase()`                      | OK                                                                                                                |
| `to_ascii_uppercase()`               | `to_ascii_uppercase()`                      | OK                                                                                                                |
| `repeat(n)`                          | `repeat(n)`                                 | NEW                                                                                                               |
| `starts_with(ch)` / `starts_with(s)` | overloaded or separate                      | see below                                                                                                         |
| `s + &t`                             | `s + t`                                     | OK                                                                                                                |
| `s += &t`                            | `s += t`                                    | OK                                                                                                                |
| `String::from_utf8(bytes)`           | `String::from_utf8(bytes)`                  | OK                                                                                                                |
| `String::from_utf8_lossy(bytes)`     | `String::from_utf8_lossy(bytes)`            | OK                                                                                                                |
| `String::from_utf8_unchecked(bytes)` | `String::from_utf8_unchecked(bytes)`        | OK                                                                                                                |
| `as_bytes()`                         | —                                           | SUPERSEDED by WEP-2026-05-16 (removed; semantics differ from Rust under value semantics). Use `bytes().collect()` |
| `as_str()`                           | —                                           | no borrowing distinction                                                                                          |
| `into_bytes()`                       | —                                           | no ownership transfer, use `as_bytes()`                                                                           |
| `capacity()`                         | `capacity()`                                | NEW                                                                                                               |
| `reserve(n)`                         | `reserve(n)`                                | NEW                                                                                                               |
| `shrink_to_fit()`                    | `shrink_to_fit()`                           | NEW                                                                                                               |
| `drain(..)`                          | —                                           | complex                                                                                                           |
| `retain(pred)`                       | —                                           | use `.chars().filter().collect()`                                                                                 |
| `split_off(i)`                       | —                                           | niche                                                                                                             |
| `encode_utf16()`                     | —                                           | not needed for Wasm/WASI                                                                                          |

Also available via existing Wado features:

| Rust                            | Wado equivalent                          |
| ------------------------------- | ---------------------------------------- |
| `format!("{}", x)`              | `` `{x}` `` (template string)            |
| `s.chars().filter(f).collect()` | `String::from_iter(s.chars().filter(f))` |
| `s.chars().count()`             | `s.chars().count()`                      |

#### Pattern Argument Design

Rust's `str::contains`, `find`, `split`, etc. accept a `Pattern` trait that works with `&str`, `char`, and closures. Wado simplifies this:

- **String patterns**: `contains(pat: String)`, `find(pat: String)`, `split(sep: String)` — substring matching.
- **Char patterns**: `contains_char(ch: char)`, `find_char(pred: fn(&char) -> bool)` — character-level matching.
- **`starts_with` / `ends_with`**: Accept `String`. For single char, use `s.starts_with(String::from('x'))`.

This avoids the complexity of a Pattern trait while covering the common cases.

#### String Byte-Index Convention

Following Rust, `String` methods that accept or return indices use **byte indices**, not char indices:

- `find(pat)` → `Option<i32>` (byte index)
- `insert(i, ch)` — `i` is a byte index
- `remove(i)` — `i` is a byte index
- `truncate(n)` — `n` is byte count

This is consistent with `len()` returning byte length and matches Rust's design. Methods panic if the index is not on a char boundary.

#### New String Methods — Signatures

```wado
impl String {
    pub fn push(&mut self, ch: char);                        // rename from append_char
    pub fn push_str(&mut self, s: String);                   // rename from append
    pub fn pop(&mut self) -> Option<char>;
    pub fn insert(&mut self, byte_index: i32, ch: char);
    pub fn insert_str(&mut self, byte_index: i32, s: String);
    pub fn remove(&mut self, byte_index: i32) -> char;
    pub fn truncate(&mut self, byte_len: i32);               // rename from truncate_bytes
    pub fn clear(&mut self);
    pub fn capacity(&self) -> i32;
    pub fn reserve(&mut self, additional: i32);
    pub fn shrink_to_fit(&mut self);
    pub fn as_bytes(&self) -> List<u8>;
    pub fn contains(&self, pat: String) -> bool;
    pub fn starts_with(&self, pat: String) -> bool;
    pub fn ends_with(&self, pat: String) -> bool;
    pub fn find(&self, pat: String) -> Option<i32>;          // byte index
    pub fn rfind(&self, pat: String) -> Option<i32>;         // byte index
    pub fn contains_char(&self, ch: char) -> bool;
    pub fn find_char(&self, pred: fn(&char) -> bool) -> Option<i32>;  // byte index
    pub fn split(&self, sep: String) -> StrSplitIter;           // Iterator<Item = String>
    pub fn splitn(&self, n: i32, sep: String) -> StrSplitNIter; // Iterator<Item = String>
    pub fn split_whitespace(&self) -> StrSplitWhitespaceIter;  // Iterator<Item = String>
    pub fn lines(&self) -> StrLinesIter;                       // Iterator<Item = String>
    pub fn replace(&self, from: String, to: String) -> String;
    pub fn replacen(&self, from: String, to: String, count: i32) -> String;
    pub fn to_lowercase(&self) -> String;
    pub fn to_uppercase(&self) -> String;
    pub fn repeat(&self, n: i32) -> String;
    pub fn char_indices(&self) -> StrCharIndicesIter;        // Iterator<Item = [i32, char]>
}
```

### Renames and Deprecations

| Type     | Old Name            | New Name      | Reason                        |
| -------- | ------------------- | ------------- | ----------------------------- |
| `List`   | `append(value)`     | `push(value)` | Match Rust `Vec::push`        |
| `String` | `append_char(c)`    | `push(c)`     | Match Rust `String::push`     |
| `String` | `append(s)`         | `push_str(s)` | Match Rust `String::push_str` |
| `String` | `truncate_bytes(n)` | `truncate(n)` | Match Rust `String::truncate` |

Per WEP-2026-05-16, old names are removed outright (no deprecation period) since Wado has no external users yet.

`String::truncate_chars(n)` is kept as-is — it has no Rust equivalent (Rust has no char-count truncation) and the `_chars` suffix makes the distinction clear.

### Existing Wado-Specific Methods (Kept)

These have no direct Rust counterpart but are useful in Wado:

| Method                                                       | Purpose                                                   |
| ------------------------------------------------------------ | --------------------------------------------------------- |
| `List::filled(n, x)`                                         | Like `vec![x; n]` but as a static method                  |
| `List::sorted()` / `sorted_by()`                             | Non-mutating sort (returns new List)                      |
| `List::slice(start, end)`                                    | Explicit slice creation                                   |
| `List::copy_within_append()`                                 | Low-level, used by zlib                                   |
| `String::from_iter(iter)`                                    | Construct from char iterator                              |
| `String::get_byte_unchecked(i)` / `set_byte_unchecked(i, v)` | Low-level byte access (unchecked-only per WEP-2026-05-16) |
| `String::append_byte_filled(byte, n)`                        | Low-level byte fill                                       |
| `String::trim_ascii*()`                                      | ASCII-only trim variants                                  |

## Consequences

### Positive

1. **Rust familiarity**: Developers can transfer Rust knowledge directly.
2. **Completeness**: Common operations (`contains`, `split`, `join`, `find`, `replace`) no longer require manual loops.
3. **Consistent naming**: `push` = one element, `push_str` = string, `extend` = iterable — same vocabulary as Rust.

### Negative

1. **Breaking rename**: `append` → `push` / `push_str` requires migration (mitigated by deprecation period).
2. **Byte-index convention for String**: Char-boundary panics can surprise users unfamiliar with UTF-8. Documented clearly.

### Implementation Priority

1. **High** (most requested):
   - `push` / `push_str` (rename), `clear`, `contains`, `starts_with`, `ends_with`
   - `String::split`, `String::replace`, `String::find`
   - `List::join`, `List::contains`
2. **Medium**:
   - `insert`, `remove`, `reverse`, `extend`
   - `String::lines`, `String::split_whitespace`, `String::repeat`
   - `List::first`, `List::repeat`
3. **Lower**:
   - `swap`, `windows`, `chunks`
   - `String::rfind`, `String::splitn`, `String::replacen`, `String::char_indices`
   - `String::to_lowercase`, `String::to_uppercase`

## Implementation TODOs

- [ ] Phase 1: Add new methods (high priority)
- [ ] Phase 1: Add new methods (medium priority)
- [ ] Phase 1: Add new methods (lower priority)
- [ ] Phase 2: Deprecation warnings for old names
- [ ] Phase 3: Remove deprecated aliases
- [ ] Update `docs/cheatsheet.md`
- [ ] Update `docs/stdlib-core-prelude.md` (auto-generated)
- [ ] Update `docs/spec.md`
