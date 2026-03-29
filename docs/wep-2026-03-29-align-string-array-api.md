# WEP: Align String and Array API Design

## Status

Draft

## Context

`String` and `Array<T>` are the two most heavily used collection types in Wado. Both are growable, value-semantic, GC-backed containers — `String` over `u8` (UTF-8 bytes), `Array<T>` over arbitrary `T`. Despite their structural similarity, their public APIs have diverged: `Array` has methods that `String` lacks and vice versa, naming is inconsistent, and common Rust idioms are missing from both.

This proposal aligns the two APIs so that:

1. Equivalent operations have the same name and signature shape.
2. Commonly needed Rust `Vec<T>` / `String` / `&str` / `&[T]` methods are available (minus anything requiring lifetimes or ownership).
3. The API feels cohesive — learning one type teaches you the other.

### Current State Summary

| Capability       | Array                               | String                                    | Gap                        |
| ---------------- | ----------------------------------- | ----------------------------------------- | -------------------------- |
| Length           | `len()`                             | `len()` (bytes)                           | OK                         |
| Empty check      | `is_empty()`                        | `is_empty()`                              | OK                         |
| Safe access      | `get(i)`                            | `get_byte(i)`                             | Naming mismatch            |
| Last element     | `last()`                            | —                                         | String missing             |
| Append single    | `append(v)`                         | `append_char(c)`                          | Naming mismatch            |
| Append bulk      | `append(v)` in loop                 | `append(s)`                               | `Array` missing `extend()` |
| Pop              | `pop()`                             | —                                         | String missing             |
| Insert at index  | —                                   | —                                         | Both missing               |
| Remove at index  | —                                   | —                                         | Both missing               |
| Clear            | —                                   | —                                         | Both missing               |
| Contains element | —                                   | —                                         | Both missing               |
| Find / search    | —                                   | —                                         | Both missing               |
| Starts/ends with | —                                   | —                                         | Both missing               |
| Reverse          | —                                   | —                                         | Both missing               |
| Split            | —                                   | —                                         | String missing             |
| Join             | —                                   | —                                         | Array missing              |
| Repeat           | —                                   | —                                         | Both missing               |
| Slice (range)    | `slice(start,end)` + range indexing | —                                         | String missing             |
| Truncate         | `truncate(len)`                     | `truncate_bytes(n)` / `truncate_chars(n)` | Naming mismatch            |
| Sort             | `sort()` / `sort_by()`              | —                                         | N/A for String             |
| Capacity         | `with_capacity()`                   | `with_capacity()`                         | OK                         |

## Decision

### Design Principles

1. **Mirror when possible**: If an operation makes sense on both types, use the same method name.
2. **Rust-inspired naming**: Follow Rust's `Vec` / `String` method names unless Wado has a good reason to diverge.
3. **No lifetime/ownership concerns**: Skip methods that only exist because of Rust's ownership model (e.g., `as_str()`, `as_slice()`, `into_boxed_str()`).
4. **UTF-8 correctness for String**: String methods that operate on "elements" work on chars by default. Byte-level operations use explicit `_byte` / `_bytes` suffix when ambiguous.
5. **Panic on out-of-bounds**: Consistent with existing `arr[i]` behavior. Safe alternatives return `Option`.

### New and Renamed Methods

#### Shared API (both String and Array)

These methods exist on both types with the same name and analogous behavior:

```wado
// --- Query ---
fn len(&self) -> i32;              // existing (byte length for String)
fn is_empty(&self) -> bool;        // existing
fn contains(&self, x: T) -> bool;  // NEW: element/substring search
fn starts_with(&self, x: T) -> bool; // NEW
fn ends_with(&self, x: T) -> bool;   // NEW

// --- Access ---
fn first(&self) -> Option<T>;     // NEW
fn last(&self) -> Option<T>;      // existing on Array, NEW on String (last char)
fn get(&self, index: i32) -> Option<T>; // existing on Array, NEW on String (char by index)

// --- Mutation ---
fn push(&mut self, x: T);         // RENAME: Array::append → push, String::append_char → push
fn pop(&mut self) -> Option<T>;   // existing on Array, NEW on String (pop last char)
fn clear(&mut self);               // NEW
fn insert(&mut self, index: i32, x: T);  // NEW
fn remove(&mut self, index: i32) -> T;   // NEW
fn truncate(&mut self, len: i32);  // existing on Array, see String notes below
fn retain(&mut self, pred: fn(&T) -> bool); // NEW

// --- Bulk ---
fn extend(&mut self, iter: impl IntoIterator<Item = T>); // NEW
fn repeat(&self, n: i32) -> Self;  // NEW

// --- Reordering ---
fn reverse(&mut self);             // NEW

// --- Search ---
fn find(&self, pred: fn(&T) -> bool) -> Option<i32>; // NEW: index of first match
fn rfind(&self, pred: fn(&T) -> bool) -> Option<i32>; // NEW: index of last match
```

#### Array-Specific

```wado
impl<T> Array<T> {
    // --- existing (no change) ---
    fn with_capacity(capacity: i32) -> Array<T>;
    fn filled(n: i32, element: T) -> Array<T>;
    fn slice(&self, start: i32, end: i32) -> ArraySlice<T>;
    fn iter(&self) -> ArrayIter<T>;
    fn sort(&mut self);          // T: Ord
    fn sorted(&self) -> Array<T>; // T: Ord
    fn sort_by(&mut self, cmp: fn(&T, &T) -> Ordering);
    fn sorted_by(&self, cmp: fn(&T, &T) -> Ordering) -> Array<T>;

    // --- NEW ---
    fn swap(&mut self, a: i32, b: i32);         // swap two elements
    fn dedup(&mut self);                          // T: Eq, remove consecutive duplicates
    fn windows(&self, size: i32) -> WindowsIter<T>;   // sliding window iterator
    fn chunks(&self, size: i32) -> ChunksIter<T>;     // non-overlapping chunk iterator
    fn join(&self, separator: String) -> String;       // T: Display
    fn binary_search(&self, target: &T) -> Result<i32, i32>; // T: Ord

    // --- RENAME ---
    fn append(&mut self, value: T);  // DEPRECATED alias for push()
}
```

#### String-Specific

```wado
impl String {
    // --- existing (no change) ---
    fn with_capacity(capacity: i32) -> String;
    fn bytes(&self) -> StrUtf8ByteIter;
    fn chars(&self) -> StrCharIter;
    fn trim(&self) -> String;
    fn trim_start(&self) -> String;
    fn trim_end(&self) -> String;
    fn trim_ascii(&self) -> String;
    fn trim_ascii_start(&self) -> String;
    fn trim_ascii_end(&self) -> String;
    fn to_ascii_lowercase(&self) -> String;
    fn to_ascii_uppercase(&self) -> String;
    fn concat(a: String, b: String) -> String;
    fn from_iter<I: IntoIterator<Item = char>>(iter: I) -> String;
    fn from_utf8<I: IntoIterator<Item = u8>>(bytes: I) -> Result<String, String>;
    fn from_utf8_lossy<I: IntoIterator<Item = u8>>(bytes: I) -> String;
    fn from_utf8_unchecked<I: IntoIterator<Item = u8>>(bytes: I) -> String;

    // --- byte-level (existing, keep for low-level use) ---
    fn get_byte(&self, index: i32) -> u8;
    fn set_byte(&mut self, index: i32, value: u8);
    fn truncate_bytes(&mut self, byte_len: i32);
    fn append_byte_filled(&mut self, byte: u8, n: i32);

    // --- NEW: char-level operations ---
    fn char_len(&self) -> i32;                 // number of chars (distinct from len() = bytes)
    fn char_at(&self, index: i32) -> char;     // panicking char access by char index
    fn truncate_chars(&mut self, count: i32);  // existing

    // --- NEW: substring operations ---
    fn substring(&self, start: i32, end: i32) -> String;  // by char index
    fn split(&self, separator: String) -> Array<String>;
    fn splitn(&self, n: i32, separator: String) -> Array<String>;
    fn lines(&self) -> Array<String>;
    fn replace(&self, from: String, to: String) -> String;
    fn replacen(&self, from: String, to: String, count: i32) -> String;

    // --- NEW: search ---
    fn find_str(&self, pattern: String) -> Option<i32>;   // byte index of first occurrence
    fn rfind_str(&self, pattern: String) -> Option<i32>;  // byte index of last occurrence

    // --- RENAME ---
    fn append(&mut self, other: String);       // DEPRECATED alias, use extend() or +=
    fn append_char(&mut self, c: char);        // DEPRECATED alias, use push()
}
```

### Detailed Design Notes

#### `push` vs `append`

Rust uses `push` for single-element addition on both `Vec` and `String`. Wado currently uses `append` for both, but with inconsistent semantics:

- `Array::append(value)` — pushes one element
- `String::append(other)` — concatenates an entire string (like Rust's `push_str`)
- `String::append_char(c)` — pushes one char

This is confusing. We align with Rust:

| Operation            | Rust                          | Wado (new)                                |
| -------------------- | ----------------------------- | ----------------------------------------- |
| Push one element     | `vec.push(x)` / `s.push('c')` | `arr.push(x)` / `s.push('c')`             |
| Extend from iterable | `vec.extend(iter)`            | `arr.extend(iter)`                        |
| Append string        | `s.push_str("...")`           | `s += "..."` or `s.extend("...".chars())` |

`String::append(other)` remains as a deprecated alias during the transition. `Array::append(value)` also remains as a deprecated alias for `push()`.

#### `contains` — Different Semantics per Type

```wado
// Array<T: Eq> — does the array contain this element?
let arr: Array<i32> = [1, 2, 3];
arr.contains(&2);         // true

// String — does the string contain this substring?
let s = "hello world";
s.contains("world");      // true
s.contains("xyz");        // false
```

This mirrors Rust exactly: `Vec::contains(&T)` vs `str::contains(Pattern)`.

#### `find` / `rfind` — Different Semantics per Type

```wado
// Array<T> — predicate search, returns index
let idx = arr.find(|x: &i32| *x > 10);  // Option<i32>

// String — find() is char-predicate, find_str() is substring
let idx = s.find(|c: &char| *c == 'o');   // Option<i32> (char index)
let idx = s.find_str("world");             // Option<i32> (byte index)
```

#### `get` on String

```wado
// Array: get by index → Option<T>
arr.get(0);   // Option<i32>

// String: get by char index → Option<char>
s.get(0);     // Option<char>
```

This is the char-level counterpart. For byte-level access, `get_byte(i)` remains.

#### `insert` / `remove` on String

These operate on char indices:

```wado
let mut s = "hello";
s.insert(5, '!');    // "hello!"
s.remove(0);         // "ello!" returns 'h'
```

These are O(n) operations, consistent with `Array::insert` / `Array::remove`.

#### `truncate` on String

`String::truncate(n)` truncates to `n` chars (not bytes). This is the char-level default, matching the shared API. `truncate_bytes(n)` remains for byte-level truncation.

#### `split` Returns `Array<String>`

Unlike Rust's lazy `Split` iterator, `split()` eagerly returns `Array<String>`. This is simpler and sufficient for Wado's use cases. A lazy `split_iter()` can be added later if needed.

#### `join` on `Array<T: Display>`

```wado
let words: Array<String> = ["hello", "world"];
words.join(", ");  // "hello, world"

let nums: Array<i32> = [1, 2, 3];
nums.join("-");    // "1-2-3"
```

Requires `T: Display`. This is Rust's `[T].join()` / `itertools::join()`.

#### `reverse` — In-Place

```wado
// Array
let mut arr: Array<i32> = [1, 2, 3];
arr.reverse();  // [3, 2, 1]

// String — reverses by chars (not bytes)
let mut s = "hello";
s.reverse();  // "olleh"
```

`reversed() -> Self` (non-mutating) can be added later.

### Migration Path

1. **Phase 1 — Add new methods**: `push`, `extend`, `clear`, `contains`, `starts_with`, `ends_with`, `first`, `last` (String), `get` (String), `find`, `rfind`, `insert`, `remove`, `reverse`, `retain`, `repeat`, `swap`, `dedup`, `join`, `binary_search`, `windows`, `chunks`, `substring`, `split`, `splitn`, `lines`, `replace`, `replacen`, `find_str`, `rfind_str`, `char_len`, `char_at`, String's `pop`.
2. **Phase 2 — Deprecate old names**: Mark `Array::append` and `String::append` / `String::append_char` as deprecated with compiler warnings pointing to `push` / `extend`.
3. **Phase 3 — Remove deprecated aliases**: In a future major version.

### Methods NOT Included (and Why)

| Method                    | Reason                                                           |
| ------------------------- | ---------------------------------------------------------------- |
| `as_str()` / `as_slice()` | No borrowing distinction in Wado                                 |
| `drain()`                 | Complex iterator-over-mutation; use `retain` + iteration instead |
| `split_off()`             | Niche; use `substring` / `slice` + `truncate`                    |
| `sort_by_key()`           | Can be expressed as `sort_by(\|a, b\| key(a).cmp(&key(b)))`      |
| `rotate_left/right()`     | Niche                                                            |
| `spare_capacity_mut()`    | No unsafe / MaybeUninit in Wado                                  |
| `shrink_to_fit()`         | GC handles memory; explicit shrinking is rarely useful           |
| `capacity()`              | Internal detail; not useful without `shrink_to_fit`              |

## Consequences

### Positive

1. **Consistency**: Learning `Array` teaches you `String` and vice versa.
2. **Rust familiarity**: Developers coming from Rust find expected methods.
3. **Completeness**: Common operations like `contains`, `split`, `join`, `reverse` no longer require manual loops.
4. **Clear naming**: `push` = one element, `extend` = many, no ambiguity.

### Negative

1. **Breaking change**: `append` → `push` rename requires migration.
   - Mitigated by deprecation period with compiler warnings.
2. **API surface growth**: More methods to document and maintain.
   - Mitigated by only adding genuinely useful methods.
3. **O(n) methods on String**: `insert`, `remove`, `reverse`, `get` by char index are O(n) due to UTF-8.
   - Mitigated by clear documentation of complexity.

### Implementation Order

Suggested priority for implementation:

1. **High priority** (most commonly needed):
   - `push` (rename), `clear`, `contains`, `starts_with`, `ends_with`
   - `String::split`, `String::replace`, `String::find_str`
   - `Array::join`
2. **Medium priority**:
   - `first`, `last` (String), `get` (String), `char_len`, `char_at`
   - `insert`, `remove`, `reverse`, `extend`
   - `String::substring`, `String::lines`
3. **Lower priority**:
   - `retain`, `repeat`, `swap`, `dedup`
   - `binary_search`, `windows`, `chunks`
   - `String::splitn`, `String::replacen`, `rfind`, `rfind_str`

## Implementation TODOs

- [ ] Phase 1: Add new methods (high priority)
- [ ] Phase 1: Add new methods (medium priority)
- [ ] Phase 1: Add new methods (lower priority)
- [ ] Phase 2: Add deprecation warnings for `append` / `append_char`
- [ ] Phase 3: Remove deprecated aliases
- [ ] Update `docs/cheatsheet.md`
- [ ] Update `docs/cheatsheet-stdlib-core.md` (auto-generated)
- [ ] Update `docs/spec.md`
