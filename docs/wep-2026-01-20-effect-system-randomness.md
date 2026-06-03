# Effect System and Randomness in Collections

## Context

Modern hash map implementations require random seeding to protect against Hash DoS (Denial of Service) attacks, where attackers craft inputs to cause hash collisions and degrade performance from O(1) to O(n). This creates a design challenge for effect systems:

1. **Security requirement**: Hash maps need random seeds for DoS protection
2. **Effect system implication**: Random seed generation is a side effect
3. **Usability tension**: Requiring effects on all map operations reduces ergonomics

### How Other Languages Handle This

#### Haskell

Haskell's `unordered-containers` library currently has **no Hash DoS protection** and explicitly documents this limitation. The recommended workaround is to use `Data.Map` (tree-based, O(log n)) for untrusted input.

Challenges:

- All HashMaps share the same default salt
- Generating random seeds requires the IO monad
- Tension between pure interface and security needs
- Ongoing debate between global random seed vs per-map seeding

#### Koka

Koka faces the same challenge: if hash functions need the `ndet` (non-deterministic) effect, **every HashMap operation** must be marked with `ndet`, which propagates throughout the codebase.

From user experience reports: "There are situations where a 'hash' function should be non-deterministic... I have to mark every operation on the hashmap `ndet`, for every type."

#### Rust

Rust successfully uses SipHash 1-3 with `RandomState` to generate random seeds, but Rust doesn't have an effect system, making this straightforward.

#### Python

Python adopted SipHash in PEP 456, generating a cryptographically secure random seed at startup (controllable via `PYTHONHASHSEED`).

### WASI P3's Solution

WASI P3 provides a dedicated effect specifically for this use case:

```wado
/// The insecure-seed interface for seeding hash-map DoS resistance.
pub interface InsecureSeed {
    /// This function is intended to only be called once, by a source language
    /// to initialize Denial Of Service (DoS) protection in its hash-map
    /// implementation.
    fn get_insecure_seed() -> Tuple<u64, u64>;
}
```

Key properties:

- Intended to be called **once** during initialization
- **Not cryptographically secure** (CSPRNG not required)
- May even be deterministic (for testing/replay scenarios)
- Expected future evolution: will likely become a **value import** (constant)

This is distinct from:

- `Random`: Cryptographically secure random bytes (for security)
- `Insecure`: Fast pseudo-random bytes (for simulations, games)

## Decision

### Both Maps Preserve Insertion Order

Wado provides two map types. Both iterate in **insertion order**; they differ only in the lookup backing and the trait bound on keys. The type name reflects the lookup data structure, not the iteration semantics:

- `TreeMap<K, V>` — keys indexed by a balanced tree, O(log n) lookups, `K: Ord`.
- `HashMap<K, V>` — keys indexed by a hash table, O(1) expected lookups, `K: Hash + Eq`.

```wado
// core:collections

/// Insertion-order map. Keys indexed by a balanced tree. K: Ord.
/// No effects required. Secure against Hash DoS attacks by construction.
pub struct TreeMap<K, V> {
    // tree of key -> index, plus a dense Vec<(K, V)> for iteration
}

impl TreeMap {
    pub fn new<K, V>() -> TreeMap<K, V> { ... }
    pub fn insert(&mut self, key: K, value: V) { ... }
    pub fn get(&self, key: &K) -> Option<&V> { ... }
    // Lookups: O(log n)
}

/// Insertion-order map. Keys indexed by a hash table. K: Hash + Eq.
/// Seeds a SipHash key for Hash DoS resistance; the seed is unobservable
/// through the interface (see "The #[benign(...)] Attribute" below).
pub struct HashMap<K, V> {
    // hash table of key -> index, plus a dense Vec<(K, V)> for iteration
}

impl HashMap {
    #[benign(InsecureSeed)]
    pub fn new<K, V>() -> HashMap<K, V> {
        let seed = get_insecure_seed();  // consulted once per HashMap instance
        return HashMap { seed, ... };
    }
    pub fn insert(&mut self, key: K, value: V) { ... }
    pub fn get(&self, key: &K) -> Option<&V> { ... }
    // Lookups: O(1) expected, O(n) worst case
}
```

Both types store key-value pairs in a dense array and iterate that array, so **iteration order is determined solely by the sequence of insert/remove calls**, exactly like Rust's `indexmap::IndexMap` and Python's `dict` (insertion-order since 3.7). `TreeMap` is named for its tree index but is semantically an `IndexMap`; `HashMap` is likewise an insertion-order `IndexMap` with a hash index.

### Why Insertion Order Matters for the Effect System

`HashMap` still needs a random seed for Hash DoS resistance (a per-instance SipHash key). The seed changes which keys collide internally, but because **iteration order is insertion-order — independent of the hash seed** — the randomness is _not observable_ through the public interface (iteration order, lookup results). This is the classical notion of observational purity (Gifford & Lucassen 1986; Naumann 2007): an internal effect that cannot be observed through the interface need not appear in the externally visible effect signature.

Contrast with an _unordered_ hash map (e.g. Rust's `std::HashMap`), whose iteration order is arbitrary and seed-dependent. There the randomness _is_ observable, and an effect-free interface would silently leak nondeterminism into iteration order — a documented cause of non-reproducible builds. Wado deliberately does not provide an unordered map, so this class of leak cannot arise.

The result is a clean design rule:

| Map (iteration order)                | Seed observable? | Effect on operations                      |
| ------------------------------------ | ---------------- | ----------------------------------------- |
| `TreeMap` (insertion order, no seed) | n/a              | none                                      |
| `HashMap` (insertion order, hashed)  | no               | `#[benign(InsecureSeed)]`, not propagated |
| _unordered hash map (not provided)_  | yes              | would require an explicit `with` effect   |

Whether the seed effect can be elided depends precisely on whether iteration order is seed-independent — the observational-purity boundary, applied as a design rule.

### The `#[benign(...)]` Attribute

`HashMap::new` consults `InsecureSeed`, yet its callers do not declare `with InsecureSeed`. This is expressed with the `#[benign(...)]` attribute:

```wado
impl HashMap {
    #[benign(InsecureSeed)]
    pub fn new<K, V>() -> HashMap<K, V> {
        let seed = get_insecure_seed();
        return HashMap { seed, ... };
    }
}
```

`#[benign(E)]` declares that the function performs effect `E`, but that the effect is _observationally pure_ — unobservable through the function's interface — and therefore is **not propagated** into the caller's effect signature. The caller needs no `with InsecureSeed`.

#### `#[benign]` requires the world import

`#[benign]` removes only the per-function `with` annotation. The capability itself is still real, so the world must still import the interface:

```wado
world MyApp {
    import InsecureSeed;  // still required when using HashMap
}

fn process() {            // no `with InsecureSeed` needed
    let map: HashMap<String, i32> = HashMap::new();
    map.insert("key", 42);
}
```

This keeps WASI's capability-based security model intact: the dependency on `InsecureSeed` stays visible at the component boundary. Only the effect-row noise that would otherwise propagate through every caller in the call graph is elided. A deterministic environment can still supply a constant `InsecureSeed` implementation (or omit the import and use `TreeMap`).

#### `#[benign]` is not `ambient`

`#[benign]` and the `ambient` facility (`log_stdout` / `log_stderr`, see `wep-2026-01-12-ambient-logging.md`) both suppress effect propagation, but they are distinct concepts justified by different principles:

|                    | `ambient` (logging)                                                                         | `#[benign(E)]`                     |
| ------------------ | ------------------------------------------------------------------------------------------- | ---------------------------------- |
| World import       | not required                                                                                | required                           |
| `with` propagation | none                                                                                        | none                               |
| The effect itself  | observable (output is visible) but best-effort / unreliable, so callers cannot depend on it | unobservable through the interface |
| Justification      | ambient authority + best-effort                                                             | observational purity               |

`ambient` grants authority with no capability at all (ambient authority). `#[benign]` keeps the capability and only hides the propagation. They are nearly orthogonal — one is "observable but unreliable and capability-free," the other is "unobservable but capability-bound" — so they remain separate attributes rather than one shared name.

#### `#[benign]` is a trust boundary

The compiler cannot verify observational purity; `#[benign(E)]` is a programmer assertion, analogous to Haskell's `unsafePerformIO`. Its soundness rests on an invariant of the annotated type — here, `HashMap`'s insertion-order guarantee. The attribute may only be applied where that invariant makes the effect genuinely unobservable:

- Applying `#[benign(InsecureSeed)]` to an _unordered_ map would be unsound: the seed would leak through iteration order.
- The justification must cite the interface property that hides the effect (insertion-order iteration, seed-independent lookup results).

Reviewers must check this invariant at each use site, just as `unsafePerformIO` uses are audited in Haskell.

### TreeMap as the Default Recommendation

When in doubt, developers should use `TreeMap`:

1. No effect requirements: no side effects at all.
2. Secure by design: immune to Hash DoS attacks without seeding.
3. Predictable performance: O(log n) is practically fast (20-30 comparisons for millions of elements).
4. Fully deterministic: the same inputs always produce the same structure (useful for testing and reproducible builds).

Use `HashMap` when O(1) expected lookups matter on large datasets, or when keys implement `Hash + Eq` but not `Ord`. Both maps iterate in insertion order, so switching between them does not change observable iteration order — only performance and the trait bound.

### Not Included in Prelude

Both `TreeMap` and `HashMap` are not included in `core:prelude`:

```wado
// Explicit import required
use {TreeMap} from "core:collections";
use {HashMap} from "core:collections";
```

Rationale:

- Keeps prelude minimal (YAGNI principle).
- Makes dependencies explicit.
- The `InsecureSeed` world import (for `HashMap`) stays explicit at the world boundary.
- Follows Rust's precedent (`HashMap` requires explicit import).
- Encourages staged learning: start with `List<T>`, progress to maps.

### Literal Coercion

Following `wep-2026-01-18-iterator-based-literal-coercion.md`, object literals can coerce to both map types:

```wado
let tree: TreeMap<String, i32> = {"a": 1, "b": 2};
let hash: HashMap<String, i32> = {"a": 1, "b": 2};  // no effect on the enclosing function
```

Because `HashMap::new` is `#[benign(InsecureSeed)]`, the coercion does not impose any `with` effect on the enclosing function. The only requirement is that the world imports `InsecureSeed`.

## Consequences

### Positive

1. Security without ergonomic cost: Hash DoS protection is built in, yet `HashMap` operations carry no propagating effect.
2. Safe default: `TreeMap` provides security with neither effects nor a world import.
3. Uniform semantics: both maps iterate in insertion order, so observable behavior is seed-independent and stable across runs.
4. Capability still visible: the `InsecureSeed` world import keeps the dependency auditable at the component boundary.
5. WASI P3 integration: leverages the purpose-built `InsecureSeed` interface.
6. Reusable mechanism: `#[benign(E)]` is a general attribute for observationally pure effects, not a one-off carve-out.

### Negative

1. Additional complexity: two map types instead of one.
2. Trust boundary: `#[benign]` is an unverified assertion; its soundness depends on the insertion-order invariant and must be audited.
3. Learning curve: developers must understand the `TreeMap` / `HashMap` trade-off and the meaning of `#[benign]`.

### Compared to Alternatives

#### Alternative: O(log n) only (like Haskell recommendation)

- Rejected: leaves performance on the table for legitimate use cases.
- Wado provides both options, letting developers choose, with identical observable semantics.

#### Alternative: an unordered `HashMap` whose `new` propagates `with InsecureSeed`

- Rejected: an unordered map makes the seed observable through iteration order, leaking nondeterminism and breaking reproducible builds.
- It also forces every caller in the chain to declare `with InsecureSeed`, the propagation problem Koka users report. Insertion order plus `#[benign]` removes both issues at once.

#### Alternative: make `InsecureSeed` `ambient`

- Rejected: `ambient` would also drop the world import, hiding a real capability. `#[benign]` keeps the capability visible at the boundary and only elides propagation. See "The `#[benign(...)]` Attribute" above.

#### Alternative: HashMap with a fixed seed (no effect, no import)

- Rejected: vulnerable to Hash DoS attacks. Security cannot be optional for a default implementation.

### Implementation Status

- [x] `InsecureSeed` declared in `lib/wasi/random/insecure_seed.wado`.
- [x] `TreeMap` implemented in `core:collections` (insertion-order, tree-indexed).
- [ ] `#[benign(E)]` attribute: parsing, effect-check integration (suppress propagation while still requiring the world import), and diagnostics.
- [ ] `HashMap`: insertion-order, hash-indexed, `SipHash 1-3`, and a `Hash` / `Eq` trait pair, with `#[benign(InsecureSeed)]` on `new`.

## Relationship with wasi-keyvalue

WASI provides a separate key-value storage API called `wasi-keyvalue`, which is currently in Phase 2 of standardization. This is a **complementary API** that serves a fundamentally different purpose than `TreeMap` and `HashMap`.

### wasi-keyvalue Overview

`wasi-keyvalue` provides an abstraction layer over external persistent storage systems:

```wit
resource bucket {
    get: func(key: string) -> result<option<list<u8>>, error>
    set: func(key: string, value: list<u8>) -> result<_, error>
    delete: func(key: string) -> result<_, error>
    exists: func(key: string) -> result<bool, error>
    list-keys: func(cursor: option<u64>) -> result<key-response, error>
}

open: func(identifier: string) -> result<bucket, error>
```

Backend implementations can include:

- Redis
- DynamoDB
- MongoDB
- CosmosDB
- In-memory (for testing)

### Key Differences

| Aspect          | TreeMap/HashMap                                                | wasi-keyvalue                      |
| --------------- | -------------------------------------------------------------- | ---------------------------------- |
| **Purpose**     | In-memory data structures                                      | Persistent external storage        |
| **Scope**       | Process-local                                                  | Cross-service, shared              |
| **Latency**     | Nanoseconds to microseconds                                    | Milliseconds (network I/O)         |
| **Persistence** | Volatile (lost on exit)                                        | Durable (survives restarts)        |
| **Capacity**    | Memory-limited                                                 | Storage-limited (typically larger) |
| **Effects**     | none propagated (HashMap needs an `InsecureSeed` world import) | I/O effects (always)               |
| **Consistency** | Immediate                                                      | Read-your-writes guaranteed        |

### When to Use Each

**Use TreeMap or HashMap for:**

- Application state during execution
- Caching frequently accessed data
- Local data structures (counters, indexes, lookups)
- Performance-critical in-memory operations
- Temporary data that doesn't need to persist

**Use wasi-keyvalue for:**

- Persistent data across application restarts
- Sharing data between microservices
- User sessions, preferences, or profiles
- Data that must survive crashes or deployments
- Database-backed storage

### Complementary Usage

Both APIs can coexist in the same application:

```wado
use {HashMap} from "core:collections";
use {open} from "wasi:keyvalue";

fn process_user_request(user_id: String) /* with IO; InsecureSeed import only */ {
    // Local cache for this request (fast, ephemeral)
    let cache: HashMap<String, Data> = HashMap::new();

    // Persistent user data (durable, shared across instances)
    let store = open("user-sessions")?;
    let session_data = store.get(user_id)?;

    // Use cache for temporary computations
    cache.insert("temp", compute(session_data));

    // Save result back to persistent storage
    store.set(user_id, serialize(result))?;
}
```

### Implementation Status

As of 2025, wasmtime's `wasmtime_wasi_keyvalue` crate provides:

- In-memory backend (for development/testing)
- Experimental P3 support (unstable, not production-ready)
- External backend support (planned, not yet implemented)

The presence of `wasi-keyvalue` does not diminish the need for `TreeMap` and `HashMap`. They operate at different layers of the application stack and serve complementary purposes.

## Implementation Notes

### TreeMap Implementation Options

1. **Red-Black Tree**: Self-balancing binary search tree
2. **B-Tree**: Better cache locality, Rust's choice for `BTreeMap`

Both provide O(log n) guarantees. B-Tree may have better practical performance due to memory layout.

### HashMap Implementation

- Store key -> index entries in a hash table and key-value pairs in a dense array, so iteration is insertion-order (the property that makes the seed unobservable). This mirrors `indexmap::IndexMap`.
- Use SipHash-1-3 for the hash function (fast and DoS-resistant).
- Call `get_insecure_seed()` once during `HashMap::new()` and store the seed in the structure.
- Iteration order must never depend on the seed; reviewers rely on this invariant to justify `#[benign(InsecureSeed)]`.

### `#[benign]` in the effect checker

- Effect-check treats a `#[benign(E)]` function as performing `E` for the purpose of requiring the world import, but removes `E` from the function's outgoing effect row so it does not propagate to callers.
- Reject `#[benign(E)]` if `E` is not a declared effect, or if the function does not actually use `E` (a benign annotation on an unused effect is dead and should warn).

### Testing

Deterministic test environments can:

- Use `TreeMap` exclusively (no random seed needed)
- Or provide a deterministic `InsecureSeed` implementation that returns constant values

## References

### Observational purity and effect masking

- Gifford & Lucassen, "Integrating Functional and Imperative Programming" (LFP 1986) and Lucassen & Gifford, "Polymorphic Effect Systems" (POPL 1988) — effect masking of unobservable side effects (the `private` construct).
- Talpin & Jouvelot, "The Type and Effect Discipline" (LICS 1992 / Information and Computation 1994) — masking effects on regions not observable from the result type.
- Naumann, "Observational Purity and Encapsulation" (Theoretical Computer Science, 2007) — a method that mutates internal state is sound to treat as pure if it simulates a weakly pure method.
- Launchbury & Peyton Jones, "Lazy Functional State Threads" (PLDI 1994) — `runST` encapsulating internal state behind a pure type; precedent for hiding an unobservable effect.

### Hash DoS and seed randomization

- Rust HashMap documentation (randomized SipHash 1-3 seed; arbitrary iteration order): https://doc.rust-lang.org/std/collections/struct.HashMap.html
- `indexmap::IndexMap` (insertion-order, order independent of the hash function): https://docs.rs/indexmap/latest/indexmap/map/struct.IndexMap.html
- Python `dict` insertion-order guarantee (3.7+): https://docs.python.org/3/whatsnew/3.7.html
- Python PEP 456 (SipHash, hash randomization): https://peps.python.org/pep-0456/
- SipHash: https://en.wikipedia.org/wiki/SipHash
- Reproducible builds — unstable map/dict iteration order as a non-reproducibility source: https://reproducible-builds.org/docs/stable-outputs/
- Koka hash map challenges (the `ndet` propagation problem): https://zephyrtronium.github.io/articles/koka-experience.html
- Haskell unordered-containers issue (no Hash DoS protection): https://github.com/haskell-unordered-containers/unordered-containers/issues/265

### WASI and related

- WASI P3 `wasi:random/insecure-seed` specification
- WASI Key-Value specification: https://github.com/WebAssembly/wasi-keyvalue
- wasmtime_wasi_keyvalue documentation: https://docs.wasmtime.dev/api/wasmtime_wasi_keyvalue/
- wep-2026-01-12-ambient-logging.md (the `ambient` facility, contrasted with `#[benign]`)
- wep-2026-01-18-iterator-based-literal-coercion.md
