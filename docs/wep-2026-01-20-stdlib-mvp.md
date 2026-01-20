# Wado Standard Library MVP

This WEP defines the MVP (Minimum Viable Product) scope for Wado's standard library.

## Context

Wado needs a standard library that provides essential functionality for real-world applications. The scope must be carefully chosen to balance usefulness with implementation effort.

### Current State

Wado's standard library currently provides:

- `core:prelude` - String (len, get, set), Array (with_capacity, filled, len, capacity, append), Option, Result, panic, unreachable
- `core:cli` - println, eprintln, print, eprint
- `core:clocks` - now (monotonic clock)
- `core:filesystem` - basic file operations
- `wasi:*` - WASI P3 bindings

### WASI Proposal Status (as of 2026-01)

Research into WASI proposals and wasmtime implementation status:

#### WASI Proposals by Phase

| Phase                      | Proposals                                                              |
| -------------------------- | ---------------------------------------------------------------------- |
| Phase 3 (Implementation)   | cli, clocks, random, filesystem, sockets, http                         |
| Phase 2 (Spec Text)        | clocks:timezone, wasi-nn, wasi-gfx                                     |
| Phase 1 (Feature Proposal) | crypto, keyvalue, logging, messaging, sql, url, threads, pattern-match |

#### Wasmtime v40 Implementation Status

| Interface                                | Crate                  | Status                              |
| ---------------------------------------- | ---------------------- | ----------------------------------- |
| cli, clocks, filesystem, random, sockets | wasmtime-wasi          | ✅ Stable                           |
| http                                     | wasmtime-wasi-http     | ⚠️ Experimental                     |
| keyvalue                                 | wasmtime-wasi-keyvalue | ✅ Available (in-memory)            |
| nn                                       | wasmtime-wasi-nn       | ⚠️ Experimental (OpenVINO, ONNX)    |
| crypto                                   | wasmtime-wasi-crypto   | ⚠️ Stale (no updates since 2023-09) |
| pattern-match (regex)                    | -                      | ❌ Not implemented                  |

#### Features NOT in WASI

The following commonly needed features have no WASI proposal:

- Math functions (sin, cos, sqrt, pow, etc.)
- String manipulation (split, trim, replace, etc.)
- Iterator combinators (map, filter, reduce, etc.)
- Collections (Dict/HashMap, Set)
- DateTime formatting/parsing (only timezone in Phase 2)

### Conclusion

Wado must implement these core features natively. WASI provides I/O, networking, and system interfaces, but computational utilities must be built into the language's standard library.

## Decision

The MVP standard library will focus on four modules:

1. `core:math` - Mathematical functions
2. `String` (prelude) - String manipulation methods
3. `core:iterators` - Iterator trait and combinators
4. `core:collections` - Dict and Set data structures

### 1. `core:math`

Mathematical functions divided by implementation strategy:

#### Wasm Native (maps directly to Wasm instructions)

These functions compile to single Wasm instructions with no runtime overhead:

```wado
// f64 versions (also provide f32 variants)
fn abs(x: f64) -> f64;      // f64.abs
fn floor(x: f64) -> f64;    // f64.floor
fn ceil(x: f64) -> f64;     // f64.ceil
fn trunc(x: f64) -> f64;    // f64.trunc
fn nearest(x: f64) -> f64;  // f64.nearest (round to nearest even)
fn sqrt(x: f64) -> f64;     // f64.sqrt
fn min(a: f64, b: f64) -> f64;  // f64.min
fn max(a: f64, b: f64) -> f64;  // f64.max
fn copysign(x: f64, y: f64) -> f64;  // f64.copysign

// Integer versions
fn abs_i32(x: i32) -> i32;
fn min_i32(a: i32, b: i32) -> i32;
fn max_i32(a: i32, b: i32) -> i32;
fn clamp_i32(x: i32, lo: i32, hi: i32) -> i32;
// ... i64 variants
```

#### Deterministic libm (requires bundled implementation)

These require a deterministic math library (see WEP-2026-01-10-deterministic-libm):

```wado
// Trigonometric
fn sin(x: f64) -> f64;
fn cos(x: f64) -> f64;
fn tan(x: f64) -> f64;
fn asin(x: f64) -> f64;
fn acos(x: f64) -> f64;
fn atan(x: f64) -> f64;
fn atan2(y: f64, x: f64) -> f64;
fn sinh(x: f64) -> f64;
fn cosh(x: f64) -> f64;
fn tanh(x: f64) -> f64;

// Exponential and logarithmic
fn exp(x: f64) -> f64;
fn exp2(x: f64) -> f64;
fn ln(x: f64) -> f64;       // natural logarithm
fn log2(x: f64) -> f64;
fn log10(x: f64) -> f64;
fn pow(base: f64, exp: f64) -> f64;

// Other
fn hypot(x: f64, y: f64) -> f64;
fn cbrt(x: f64) -> f64;     // cube root
fn fmod(x: f64, y: f64) -> f64;
```

#### Utility Functions (Wado implementation)

```wado
fn round(x: f64) -> f64;    // round half away from zero (differs from nearest)
fn clamp(x: f64, lo: f64, hi: f64) -> f64;
fn lerp(a: f64, b: f64, t: f64) -> f64;  // linear interpolation
fn deg_to_rad(deg: f64) -> f64;
fn rad_to_deg(rad: f64) -> f64;

// Constants
const PI: f64 = 3.14159265358979323846;
const E: f64 = 2.71828182845904523536;
const TAU: f64 = 6.28318530717958647692;  // 2π
```

### 2. `String` (prelude extension)

Extend the existing String struct with manipulation methods:

```wado
impl String {
    // === Existing ===
    fn len(&self) -> i32;
    fn get(&self, index: i32) -> i32;
    fn set(&mut self, index: i32, value: i32);

    // === Search ===
    fn contains(&self, needle: &String) -> bool;
    fn starts_with(&self, prefix: &String) -> bool;
    fn ends_with(&self, suffix: &String) -> bool;
    fn index_of(&self, needle: &String) -> Option<i32>;
    fn last_index_of(&self, needle: &String) -> Option<i32>;

    // === Case Conversion ===
    fn to_upper(&self) -> String;   // ASCII only for MVP
    fn to_lower(&self) -> String;   // ASCII only for MVP

    // === Trimming ===
    fn trim(&self) -> String;
    fn trim_start(&self) -> String;
    fn trim_end(&self) -> String;

    // === Slicing and Concatenation ===
    fn substring(&self, start: i32, end: i32) -> String;
    fn slice(&self, start: i32) -> String;  // from start to end
    fn concat(&self, other: &String) -> String;
    fn repeat(&self, n: i32) -> String;

    // === Splitting and Joining ===
    fn split(&self, delimiter: &String) -> Array<String>;
    fn split_once(&self, delimiter: &String) -> Option<[String, String]>;

    // === Replacement ===
    fn replace(&self, from: &String, to: &String) -> String;
    fn replace_first(&self, from: &String, to: &String) -> String;

    // === Character Access ===
    fn char_at(&self, index: i32) -> Option<char>;  // UTF-8 aware
    fn chars(&self) -> Array<char>;  // decode to char array

    // === Comparison ===
    fn eq_ignore_case(&self, other: &String) -> bool;  // ASCII only

    // === Static Methods ===
    fn from_char(c: char) -> String;
    fn join(parts: &Array<String>, separator: &String) -> String;
}
```

#### UTF-8 Considerations

- `len()` returns byte length (existing behavior)
- `char_at()` and `chars()` decode UTF-8 properly
- `to_upper()`/`to_lower()` handle ASCII only in MVP (full Unicode requires ICU or similar)
- `substring()` operates on byte indices; users must ensure valid UTF-8 boundaries

### 3. `core:iterators`

Iterator support with a focus on practical usage without requiring full trait bounds.

#### Iterator Trait

```wado
trait Iterator<T> {
    fn next(&mut self) -> Option<T>;
}
```

#### Array Methods (MVP approach without trait bounds)

Since trait bounds are not yet implemented, provide methods directly on Array:

```wado
impl Array<T> {
    // === Transformations (return new Array) ===
    fn map<U>(&self, f: Fn(T) -> U) -> Array<U>;
    fn filter(&self, pred: Fn(&T) -> bool) -> Array<T>;
    fn filter_map<U>(&self, f: Fn(T) -> Option<U>) -> Array<U>;

    // === Reductions ===
    fn fold<A>(&self, init: A, f: Fn(A, T) -> A) -> A;
    fn reduce(&self, f: Fn(T, T) -> T) -> Option<T>;

    // === Predicates ===
    fn any(&self, pred: Fn(&T) -> bool) -> bool;
    fn all(&self, pred: Fn(&T) -> bool) -> bool;
    fn find(&self, pred: Fn(&T) -> bool) -> Option<T>;
    fn find_index(&self, pred: Fn(&T) -> bool) -> Option<i32>;
    fn position(&self, value: &T) -> Option<i32>;  // requires Eq

    // === Slicing ===
    fn take(&self, n: i32) -> Array<T>;
    fn skip(&self, n: i32) -> Array<T>;
    fn slice(&self, start: i32, end: i32) -> Array<T>;

    // === Other ===
    fn reverse(&self) -> Array<T>;
    fn enumerate(&self) -> Array<[i32, T]>;
    fn zip<U>(&self, other: &Array<U>) -> Array<[T, U]>;
    fn flatten(&self) -> Array<T>;  // for Array<Array<T>>

    // === In-place mutations ===
    fn reverse_in_place(&mut self);
    fn sort(&mut self);  // requires Ord trait or comparison fn
    fn sort_by(&mut self, cmp: Fn(&T, &T) -> i32);
}
```

#### Range Functions

```wado
// Create iterable ranges
fn range(start: i32, end: i32) -> Array<i32>;           // [start, end)
fn range_inclusive(start: i32, end: i32) -> Array<i32>; // [start, end]
fn range_step(start: i32, end: i32, step: i32) -> Array<i32>;
```

#### Future: Lazy Iterators

When trait bounds are implemented, add lazy iterator adapters:

```wado
// Lazy versions that don't allocate intermediate arrays
struct MapIter<I, F> { iter: I, f: F }
struct FilterIter<I, F> { iter: I, pred: F }
// etc.
```

### 4. `core:collections`

Two map implementations are provided: `HashMap` for O(1) average access and `TreeMap` for ordered keys.

#### HashMap

```wado
struct HashMap<K, V> {
    // Internal: array of buckets with chaining
    buckets: builtin::array<Option<Entry<K, V>>>,
    len: i32,
}

impl HashMap<K, V> {
    // === Construction ===
    fn new() -> HashMap<K, V>;
    fn with_capacity(n: i32) -> HashMap<K, V>;

    // === Size ===
    fn len(&self) -> i32;
    fn is_empty(&self) -> bool;
    fn capacity(&self) -> i32;

    // === Access ===
    fn get(&self, key: &K) -> Option<&V>;
    fn get_mut(&mut self, key: &K) -> Option<&mut V>;
    fn contains_key(&self, key: &K) -> bool;

    // === Modification ===
    fn insert(&mut self, key: K, value: V) -> Option<V>;  // returns old value
    fn remove(&mut self, key: &K) -> Option<V>;
    fn clear(&mut self);

    // === Iteration (unordered) ===
    fn keys(&self) -> Array<K>;
    fn values(&self) -> Array<V>;
    fn entries(&self) -> Array<[K, V]>;

    // === Bulk Operations ===
    fn extend(&mut self, other: &HashMap<K, V>);
}
```

#### TreeMap

Ordered map using a balanced tree (e.g., red-black tree). Keys are iterated in sorted order.

```wado
struct TreeMap<K, V> {
    // Internal: balanced tree structure
    root: Option<&Node<K, V>>,
    len: i32,
}

impl TreeMap<K, V> {
    // === Construction ===
    fn new() -> TreeMap<K, V>;

    // === Size ===
    fn len(&self) -> i32;
    fn is_empty(&self) -> bool;

    // === Access ===
    fn get(&self, key: &K) -> Option<&V>;
    fn get_mut(&mut self, key: &K) -> Option<&mut V>;
    fn contains_key(&self, key: &K) -> bool;

    // === Modification ===
    fn insert(&mut self, key: K, value: V) -> Option<V>;
    fn remove(&mut self, key: &K) -> Option<V>;
    fn clear(&mut self);

    // === Ordered iteration ===
    fn keys(&self) -> Array<K>;      // sorted order
    fn values(&self) -> Array<V>;    // sorted by key
    fn entries(&self) -> Array<[K, V]>;

    // === Range queries ===
    fn first(&self) -> Option<[K, V]>;   // minimum key
    fn last(&self) -> Option<[K, V]>;    // maximum key
    fn range(&self, start: &K, end: &K) -> Array<[K, V]>;
}
```

#### HashSet and TreeSet

```wado
struct HashSet<T> {
    // Internal: backed by HashMap<T, ()>
    map: HashMap<T, ()>,
}

struct TreeSet<T> {
    // Internal: backed by TreeMap<T, ()>
    map: TreeMap<T, ()>,
}

// Both HashSet and TreeSet share this interface
impl HashSet<T> {  // (same for TreeSet<T>)
    // === Construction ===
    fn new() -> HashSet<T>;
    fn from_array(arr: &Array<T>) -> HashSet<T>;

    // === Size ===
    fn len(&self) -> i32;
    fn is_empty(&self) -> bool;

    // === Membership ===
    fn contains(&self, value: &T) -> bool;

    // === Modification ===
    fn insert(&mut self, value: T) -> bool;  // returns true if new
    fn remove(&mut self, value: &T) -> bool; // returns true if existed
    fn clear(&mut self);

    // === Set Operations ===
    fn union(&self, other: &HashSet<T>) -> HashSet<T>;
    fn intersection(&self, other: &HashSet<T>) -> HashSet<T>;
    fn difference(&self, other: &HashSet<T>) -> HashSet<T>;
    fn symmetric_difference(&self, other: &HashSet<T>) -> HashSet<T>;
    fn is_subset(&self, other: &HashSet<T>) -> bool;
    fn is_superset(&self, other: &HashSet<T>) -> bool;
    fn is_disjoint(&self, other: &HashSet<T>) -> bool;

    // === Conversion ===
    fn to_array(&self) -> Array<T>;
}

// TreeSet additional methods (ordered)
impl TreeSet<T> {
    fn first(&self) -> Option<T>;
    fn last(&self) -> Option<T>;
    fn range(&self, start: &T, end: &T) -> Array<T>;
}
```

#### Hash and Ord Traits

HashMap/HashSet require Hash trait; TreeMap/TreeSet require Ord trait. Options:

1. **Built-in hash/ord for primitives**: Compiler generates Hash/Ord for i32, i64, String, etc.
2. **User-implementable traits**: Hash and Ord traits for custom types
3. **Identity hash for GC references**: Use Wasm GC reference identity

MVP approach: Built-in Hash and Ord for primitive types (i32, i64, f64, String, char, bool). Custom struct hashing/ordering deferred to post-MVP.

```wado
// Compiler-provided for primitives
trait Hash {
    fn hash(&self) -> u64;
}

trait Ord {
    fn cmp(&self, other: &Self) -> i32;  // -1, 0, 1
}

// Built-in implementations for primitives
impl Hash for i32 { ... }
impl Hash for i64 { ... }
impl Hash for String { ... }
impl Ord for i32 { ... }
impl Ord for i64 { ... }
impl Ord for String { ... }  // lexicographic
// etc.
```

## Implementation Phases

| Phase | Scope                        | Dependencies          | Effort |
| ----- | ---------------------------- | --------------------- | ------ |
| 1     | `core:math` (Wasm native)    | None                  | Low    |
| 2     | `String` basic methods       | None                  | Medium |
| 3     | `core:math` (libm)           | Bundled libm          | Medium |
| 4     | `Array` iterator methods     | Closure improvements  | Medium |
| 5     | `HashMap`, `HashSet`         | Hash for primitives   | High   |
| 5b    | `TreeMap`, `TreeSet`         | Ord for primitives    | High   |
| 6     | Lazy iterators               | Trait bounds          | High   |

### Phase 1: core:math (Wasm Native)

Implement functions that map directly to Wasm instructions:

- `abs`, `floor`, `ceil`, `trunc`, `nearest`, `sqrt`, `min`, `max`, `copysign` for f32/f64
- Integer versions for i32/i64
- Utility functions: `clamp`, `lerp`, `deg_to_rad`, `rad_to_deg`
- Constants: `PI`, `E`, `TAU`

### Phase 2: String Basic Methods

- Search: `contains`, `starts_with`, `ends_with`, `index_of`
- Trim: `trim`, `trim_start`, `trim_end`
- Slice: `substring`, `concat`
- Split/Join: `split`, `join`
- Case: `to_upper`, `to_lower` (ASCII only)

### Phase 3: core:math (libm)

Integrate deterministic libm (per WEP-2026-01-10-deterministic-libm):

- Trigonometric: `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2`
- Hyperbolic: `sinh`, `cosh`, `tanh`
- Exponential: `exp`, `exp2`, `pow`
- Logarithmic: `ln`, `log2`, `log10`
- Other: `hypot`, `cbrt`

### Phase 4: Array Iterator Methods

- `map`, `filter`, `filter_map`
- `fold`, `reduce`
- `any`, `all`, `find`
- `take`, `skip`, `reverse`
- `enumerate`, `zip`
- `sort_by`

### Phase 5: HashMap and HashSet

- Implement Hash trait for primitives
- HashMap with chaining collision resolution
- HashSet backed by HashMap
- Set operations (union, intersection, etc.)

### Phase 5b: TreeMap and TreeSet

- Implement Ord trait for primitives
- TreeMap with red-black tree or similar balanced tree
- TreeSet backed by TreeMap
- Range queries (first, last, range)

### Phase 6: Lazy Iterators (Post-MVP)

- Iterator trait with default methods
- Lazy adapter structs (MapIter, FilterIter, etc.)
- `collect()` to materialize

## Consequences

### Positive

- Provides essential functionality for real-world programs
- Wasm-native math functions have zero overhead
- String methods enable common text processing tasks
- Dict/Set unlock many algorithmic patterns
- Phased approach allows incremental delivery

### Negative

- libm integration adds bundle size (~50-100KB estimated)
- Full Unicode support deferred (to_upper/to_lower ASCII only)
- Lazy iterators require trait bounds (deferred)
- Dict/Set only work with primitive keys initially

### Risks

- Closure support must be stable for iterator methods
- Hash implementation must be deterministic across platforms
- UTF-8 boundary handling in String methods needs careful testing

## Alternatives Considered

### Use WASI for Everything

Rejected because:

- Math, String, Iterator, Collections have no WASI proposals
- WASI focuses on I/O and system interfaces, not computational utilities

### Minimal stdlib (User-land Libraries)

Rejected because:

- Core operations like String.split() are too fundamental
- No package manager yet for distributing user libraries
- Consistency across Wado programs is valuable

### Full Unicode String Support in MVP

Deferred because:

- ICU is large (~25MB)
- ASCII-only covers most programming use cases
- Can add Unicode support later without breaking changes

## References

- [WEP-2026-01-10-deterministic-libm](./wep-2026-01-10-deterministic-libm.md)
- [WASI Interfaces](https://wasi.dev/interfaces)
- [WASI Roadmap](https://wasi.dev/roadmap)
- [wasmtime-wasi-crypto](https://docs.rs/crate/wasmtime-wasi-crypto/latest) (stale since 2023-09)
- [wasi-pattern-match](https://github.com/WebAssembly/wasi-pattern-match) (Phase 1, not in wasmtime)
- [Wasmtime v40 Release](https://github.com/bytecodealliance/wasmtime/releases)
