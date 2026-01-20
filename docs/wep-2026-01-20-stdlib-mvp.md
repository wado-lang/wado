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
- Collections (HashMap, TreeMap, Set)
- DateTime formatting/parsing (only timezone in Phase 2)

### Conclusion

Wado must implement these core features natively. WASI provides I/O, networking, and system interfaces, but computational utilities must be built into the language's standard library.

## Decision

The MVP standard library will focus on four modules:

1. `core:math` - Mathematical functions
2. `String` (prelude) - String manipulation methods
3. `core:iterators` - Iterator trait and combinators
4. `core:collections` - HashMap, TreeMap, HashSet, TreeSet

### 1. `core:math`

Mathematical functions divided by implementation strategy:

| Category       | Functions                                                        | Implementation          |
| -------------- | ---------------------------------------------------------------- | ----------------------- |
| Wasm Native    | abs, floor, ceil, trunc, nearest, sqrt, min, max, copysign       | Direct Wasm instruction |
| Integer        | abs_i32, min_i32, max_i32, clamp_i32 (+ i64 variants)            | Wado implementation     |
| Trigonometric  | sin, cos, tan, asin, acos, atan, atan2, sinh, cosh, tanh         | Deterministic libm      |
| Exp/Log        | exp, exp2, ln, log2, log10, pow                                  | Deterministic libm      |
| Other          | hypot, cbrt, fmod                                                | Deterministic libm      |
| Utility        | round, clamp, lerp, deg_to_rad, rad_to_deg                       | Wado implementation     |
| Constants      | PI, E, TAU                                                       | Compile-time constants  |

See [WEP-2026-01-10-deterministic-libm](./wep-2026-01-10-deterministic-libm.md) for libm integration details.

### 2. `String` (prelude extension)

Extend the existing String struct with manipulation methods:

| Category     | Methods                                                   | Notes                  |
| ------------ | --------------------------------------------------------- | ---------------------- |
| Search       | contains, starts_with, ends_with, index_of, last_index_of |                        |
| Case         | to_upper, to_lower, eq_ignore_case                        | ASCII only for MVP     |
| Trim         | trim, trim_start, trim_end                                |                        |
| Slice        | substring, slice, concat, repeat                          |                        |
| Split/Join   | split, split_once, join (static)                          |                        |
| Replace      | replace, replace_first                                    |                        |
| Char Access  | char_at, chars                                            | UTF-8 aware            |
| Construction | from_char                                                 |                        |

UTF-8 considerations:
- `len()` returns byte length (existing behavior)
- `substring()` operates on byte indices
- Full Unicode case conversion deferred (requires ICU or similar)

### 3. `core:iterators`

Iterator support with a focus on practical usage.

| Category        | Functions/Methods                                     | Notes                     |
| --------------- | ----------------------------------------------------- | ------------------------- |
| Trait           | Iterator<T> with next()                               |                           |
| Transform       | map, filter, filter_map                               | Array methods             |
| Reduce          | fold, reduce                                          | Array methods             |
| Predicate       | any, all, find, find_index, position                  | Array methods             |
| Slice           | take, skip, slice                                     | Array methods             |
| Other           | reverse, enumerate, zip, flatten                      | Array methods             |
| In-place        | reverse_in_place, sort, sort_by                       | Array methods             |
| Range           | range, range_inclusive, range_step                    | Free functions            |
| Lazy (Post-MVP) | MapIter, FilterIter, etc.                             | Requires trait bounds     |

MVP approach: Provide methods directly on Array<T> since trait bounds are not yet implemented.

### 4. `core:collections`

| Type      | Description                          | Key Trait | Notes                    |
| --------- | ------------------------------------ | --------- | ------------------------ |
| HashMap   | O(1) average access, unordered       | Hash      | Chaining for collisions  |
| TreeMap   | O(log n) access, ordered by key      | Ord       | Balanced tree (e.g. RB)  |
| HashSet   | Backed by HashMap<T, ()>             | Hash      |                          |
| TreeSet   | Backed by TreeMap<T, ()>             | Ord       | Range queries supported  |

MVP approach: Built-in Hash and Ord for primitive types (i32, i64, f64, String, char, bool). Custom struct hashing/ordering deferred to post-MVP.

## Implementation Phases

| Phase | Scope                        | Dependencies         |
| ----- | ---------------------------- | -------------------- |
| 1     | `core:math` (Wasm native)    | None                 |
| 2     | `String` basic methods       | None                 |
| 3     | `core:math` (libm)           | Bundled libm         |
| 4     | `Array` iterator methods     | Closure improvements |
| 5     | `HashMap`, `HashSet`         | Hash for primitives  |
| 5b    | `TreeMap`, `TreeSet`         | Ord for primitives   |
| 6     | Lazy iterators (Post-MVP)    | Trait bounds         |

## Consequences

### Positive

- Provides essential functionality for real-world programs
- Wasm-native math functions have zero overhead
- String methods enable common text processing tasks
- HashMap/TreeMap unlock many algorithmic patterns
- Phased approach allows incremental delivery

### Negative

- libm integration adds bundle size (~50-100KB estimated)
- Full Unicode support deferred (to_upper/to_lower ASCII only)
- Lazy iterators require trait bounds (deferred)
- Collections only work with primitive keys initially

### Risks

- Closure support must be stable for iterator methods
- Hash implementation must be deterministic across platforms
- UTF-8 boundary handling in String methods needs careful testing

## References

- [WEP-2026-01-10-deterministic-libm](./wep-2026-01-10-deterministic-libm.md)
- [WASI Interfaces](https://wasi.dev/interfaces)
- [WASI Roadmap](https://wasi.dev/roadmap)
- [wasmtime-wasi-crypto](https://docs.rs/crate/wasmtime-wasi-crypto/latest) (stale since 2023-09)
- [wasi-pattern-match](https://github.com/WebAssembly/wasi-pattern-match) (Phase 1, not in wasmtime)
- [Wasmtime v40 Release](https://github.com/bytecodealliance/wasmtime/releases)
