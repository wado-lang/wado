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

### Provide Two Map Types

Following the precedent of Rust, C++, and Java, provide both tree-based and hash-based map implementations with different trade-offs:

```wado
// core:collections

/// Ordered map with O(log n) operations.
/// Keys must implement Ord trait.
/// No effects required. Secure against Hash DoS attacks.
pub struct TreeMap<K, V> {
    // Red-Black Tree or B-Tree implementation
}

impl TreeMap {
    pub fn new<K, V>() -> TreeMap<K, V> { ... }
    pub fn insert(&mut self, key: K, value: V) { ... }
    pub fn get(&self, key: &K) -> Option<&V> { ... }
    // All operations: O(log n)
}

/// Hash-based map with O(1) expected time operations.
/// Keys must implement Hash and Eq traits.
/// Requires InsecureSeed effect for DoS protection.
pub struct HashMap<K, V> {
    // SipHash-based hash table
}

impl HashMap {
    pub fn new<K, V>() -> HashMap<K, V> with InsecureSeed {
        let seed = get_insecure_seed();  // Called once per HashMap instance
        return HashMap { seed, ... };
    }
    pub fn insert(&mut self, key: K, value: V) { ... }
    pub fn get(&self, key: &K) -> Option<&V> { ... }
    // Expected: O(1), Worst case: O(n)
}
```

Effect requirements live on the function signature, like every other effect in Wado. There is no per-expression `with` syntax: a caller satisfies `HashMap::new`'s `InsecureSeed` requirement by declaring `with InsecureSeed` on its own signature (or by being inside a `with InsecureSeed => h do { ... }` block).

### InsecureSeed is NOT an Ambient Effect

`InsecureSeed` requires **explicit world import** rather than being implicitly available:

```wado
world MyApp {
    import InsecureSeed;  // Required only if using HashMap
}

fn process() with InsecureSeed {
    let map: HashMap<String, i32> = HashMap::new();
    map.insert("key", 42);
}
```

Rationale:

- Follows WASI's capability-based security model
- Makes effect dependencies visible in the type system
- Allows deterministic environments (testing, replay) to omit this import
- Contrasts with an "ambient effect" approach that would hide the dependency

### TreeMap as the Default Recommendation

When in doubt, developers should use `TreeMap`:

1. **No effect requirements**: Pure, no side effects
2. **Secure by design**: Immune to Hash DoS attacks
3. **Predictable performance**: O(log n) is practically fast (20-30 comparisons for millions of elements)
4. **Deterministic**: Same input always produces same structure (useful for testing)

Use `HashMap` only when:

- Performance profiling shows `TreeMap` is a bottleneck
- Working with very large datasets where O(1) vs O(log n) matters
- Willing to add `InsecureSeed` effect to the dependency chain

### Not Included in Prelude

Both `TreeMap` and `HashMap` are **not** included in `core:prelude`:

```wado
// Explicit import required
use {TreeMap} from "core:collections";
use {HashMap} from "core:collections";
```

Rationale:

- Keeps prelude minimal (YAGNI principle)
- Makes dependencies explicit
- Effect requirements (for `HashMap`) become visible at import site
- Follows Rust's precedent (HashMap requires explicit import)
- Encourages staged learning: start with `Array<T>`, progress to maps

### Literal Coercion

Following `wep-2026-01-18-iterator-based-literal-coercion.md`, object literals can coerce to both map types:

```wado
let tree: TreeMap<String, i32> = {"a": 1, "b": 2};
let hash: HashMap<String, i32> = {"a": 1, "b": 2};  // requires `with InsecureSeed` on the enclosing function
```

The coercion mechanism handles calling constructors with appropriate effects; the constructor's `with InsecureSeed` is checked against the enclosing function's effect set as usual.

## Consequences

### Positive

1. **Effect system aligns with security**: Hash DoS protection is visible in the type system
2. **Safe default**: `TreeMap` provides security without effect requirements
3. **Clear trade-offs**: Performance (HashMap) vs simplicity (TreeMap) is explicit
4. **WASI P3 integration**: Leverages purpose-built `InsecureSeed` interface
5. **Flexibility**: Deterministic environments can use `TreeMap` exclusively

### Negative

1. **Additional complexity**: Two map types instead of one
2. **Effect propagation**: Using `HashMap` requires `InsecureSeed` in calling functions
3. **Learning curve**: Developers must understand the trade-offs

### Compared to Alternatives

#### Alternative: O(log n) only (like Haskell recommendation)

- **Rejected**: Leaves performance on the table for legitimate use cases
- Wado provides both options, letting developers choose

#### Alternative: Make InsecureSeed ambient/implicit

- **Rejected**: Violates effect system principles
- Hides security-relevant dependencies
- Breaks deterministic execution guarantees

#### Alternative: HashMap with fixed seed (no effect)

- **Rejected**: Vulnerable to Hash DoS attacks
- Security cannot be optional for default implementations

### Implementation Status

`InsecureSeed` is declared in `lib/wasi/random/insecure_seed.wado` and is reachable from any function declaring `with InsecureSeed`. `TreeMap` is implemented in `core:collections`; `HashMap` is not yet implemented — it will land alongside `SipHash 1-3` and a `Hash`/`Eq` trait pair.

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

| Aspect          | TreeMap/HashMap             | wasi-keyvalue                      |
| --------------- | --------------------------- | ---------------------------------- |
| **Purpose**     | In-memory data structures   | Persistent external storage        |
| **Scope**       | Process-local               | Cross-service, shared              |
| **Latency**     | Nanoseconds to microseconds | Milliseconds (network I/O)         |
| **Persistence** | Volatile (lost on exit)     | Durable (survives restarts)        |
| **Capacity**    | Memory-limited              | Storage-limited (typically larger) |
| **Effects**     | InsecureSeed (HashMap only) | I/O effects (always)               |
| **Consistency** | Immediate                   | Read-your-writes guaranteed        |

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

fn process_user_request(user_id: String) /* with InsecureSeed, IO */ {
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

- Use SipHash-1-3 for the hash function (fast and DoS-resistant)
- Call `get_insecure_seed()` once during `HashMap::new()`
- Store seed in the HashMap structure
- Future optimization: consider global seed with lazy initialization (if effect syntax permits)

### Testing

Deterministic test environments can:

- Use `TreeMap` exclusively (no random seed needed)
- Or provide a deterministic `InsecureSeed` implementation that returns constant values

## References

- WASI P3 `wasi:random/insecure-seed` specification
- WASI Key-Value specification: https://github.com/WebAssembly/wasi-keyvalue
- wasmtime_wasi_keyvalue documentation: https://docs.wasmtime.dev/api/wasmtime_wasi_keyvalue/
- Rust HashMap documentation: https://doc.rust-lang.org/std/collections/struct.HashMap.html
- Haskell unordered-containers issue: https://github.com/haskell-unordered-containers/unordered-containers/issues/265
- Koka hash map challenges: https://zephyrtronium.github.io/articles/koka-experience.html
- Python PEP 456 (Secure Hash Algorithm): https://peps.python.org/pep-0456/
- SipHash: https://en.wikipedia.org/wiki/SipHash
- wep-2026-01-18-iterator-based-literal-coercion.md
