# Wado Standard Library MVP

This WEP defines the MVP (Minimum Viable Product) scope for Wado's standard library.

## Context

Wado needs a standard library that provides essential functionality for real-world applications.

### WASI Proposal Status (as of 2026-01)

| Phase                      | Proposals                                                              |
| -------------------------- | ---------------------------------------------------------------------- |
| Phase 3 (Implementation)   | cli, clocks, random, filesystem, sockets, http                         |
| Phase 2 (Spec Text)        | clocks:timezone, wasi-nn, wasi-gfx                                     |
| Phase 1 (Feature Proposal) | crypto, keyvalue, logging, messaging, sql, url, threads, pattern-match |

### Wasmtime v40 Implementation Status

| Interface                                | Crate                  | Status                              |
| ---------------------------------------- | ---------------------- | ----------------------------------- |
| cli, clocks, filesystem, random, sockets | wasmtime-wasi          | ✅ Stable                           |
| http                                     | wasmtime-wasi-http     | ⚠️ Experimental                     |
| keyvalue                                 | wasmtime-wasi-keyvalue | ✅ Available (in-memory)            |
| nn                                       | wasmtime-wasi-nn       | ⚠️ Experimental (OpenVINO, ONNX)    |
| crypto                                   | wasmtime-wasi-crypto   | ⚠️ Stale (no updates since 2023-09) |
| pattern-match (regex)                    | -                      | ❌ Not implemented                  |

### Features NOT in WASI

- Math functions (sin, cos, sqrt, pow, etc.)
- String manipulation (split, trim, replace, etc.)
- Iterator combinators (map, filter, reduce, etc.)
- Collections (HashMap, TreeMap, Set)
- DateTime formatting/parsing (only timezone in Phase 2)

## Decision

The MVP standard library will focus on four modules:

### 1. `core:math`

- **Wasm Native**: abs, floor, ceil, trunc, nearest, sqrt, min, max, copysign (direct Wasm instructions)
- **Deterministic libm**: Trigonometric, exponential, logarithmic functions (see [WEP-2026-01-10-deterministic-libm](./wep-2026-01-10-deterministic-libm.md))
- **Utility**: round, clamp, lerp, deg_to_rad, rad_to_deg
- **Constants**: PI, E, TAU

### 2. `String` (prelude extension)

String manipulation methods: search, case conversion (ASCII), trim, slice, split/join, replace, character access.

### 3. `core:iterators`

- Array methods: map, filter, fold, reduce, any, all, find, take, skip, reverse, enumerate, zip, sort_by
- Range functions: range, range_inclusive, range_step
- Post-MVP: Lazy iterators (requires trait bounds)

### 4. `core:collections`

| Type    | Description                     | Key Trait |
| ------- | ------------------------------- | --------- |
| HashMap | O(1) average access, unordered  | Hash      |
| TreeMap | O(log n) access, ordered by key | Ord       |
| HashSet | Backed by HashMap<T, ()>        | Hash      |
| TreeSet | Backed by TreeMap<T, ()>        | Ord       |

MVP: Built-in Hash/Ord for primitives only.

## Implementation Phases

| Phase | Scope                     | Dependencies         |
| ----- | ------------------------- | -------------------- |
| 1     | `core:math` (Wasm native) | None                 |
| 2     | `String` basic methods    | None                 |
| 3     | `core:math` (libm)        | Bundled libm         |
| 4     | `Array` iterator methods  | Closure improvements |
| 5     | `HashMap`, `HashSet`      | Hash for primitives  |
| 5b    | `TreeMap`, `TreeSet`      | Ord for primitives   |
| 6     | Lazy iterators (Post-MVP) | Trait bounds         |

## Consequences

### Positive

- Provides essential functionality for real-world programs
- Wasm-native math functions have zero overhead
- Phased approach allows incremental delivery

### Negative

- libm integration adds bundle size (~50-100KB estimated)
- Full Unicode support deferred
- Collections only work with primitive keys initially

## References

- [WEP-2026-01-10-deterministic-libm](./wep-2026-01-10-deterministic-libm.md)
- [WASI Interfaces](https://wasi.dev/interfaces)
- [Wasmtime v40 Release](https://github.com/bytecodealliance/wasmtime/releases)
