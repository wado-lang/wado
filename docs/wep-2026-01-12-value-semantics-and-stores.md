# WEP: Value Semantics and Reference Stores

## Context

Wado targets Wasm GC, where structs and arrays are reference types (heap-allocated, garbage-collected). However, the language design needs to decide on the semantics exposed to programmers:

1. **What semantics should structs have?** Value (copy on assign) or reference (alias on assign)?
2. **Where are local variables allocated?** Stack or heap?
3. **How to handle `f(&local_var)` when `f` might store the reference?**

### Survey of Other Languages

| Language            | Default Semantics                                           | Escape Handling                    |
| ------------------- | ----------------------------------------------------------- | ---------------------------------- |
| **Rust**            | Move by default, `Copy` trait opt-in                        | Lifetimes track reference validity |
| **Go**              | Value (shallow copy), reference types share underlying data | Escape analysis promotes to heap   |
| **Swift**           | Value for structs, Copy-on-Write for collections            | Automatic                          |
| **Java (Valhalla)** | New value types: identity-less, copied on assign            | N/A (value types can't escape)     |
| **Zig**             | Compiler chooses pass-by-value or reference                 | Explicit pointers for mutation     |

### The Wasm GC Constraint

In Wasm GC, structs and arrays are **reference types**:

- Heap-allocated
- Managed by the garbage collector
- Variables hold references, not values directly

This means implementing true value semantics requires explicit copying.

### The Escape Problem

When a function receives `&local_var`, the caller needs to know if the function will store that reference:

```wado
fn caller() {
    let local = Data{};
    store(&local);  // Will `store` keep a reference to `local`?
}
```

If `store` keeps the reference, `local` must outlive the function call. In a GC'd language, this means `local` must be on the heap.

### Approaches in Effect System Languages

**Koka**: Uses Perceus reference counting, can prove references don't escape scope via effect analysis.

**Eff**: Acknowledges escape analysis is undecidable; runtime errors if references escape their handler.

**Scala 3 Capture Checking**: Tracks captures in the type system with "capture sets":

```scala
A -> B        // Pure: does NOT capture parameters
A => B        // Impure: can capture anything
A ->{c,d} B   // Captures only c and d
```

## Decision

### 1. Structs Have Value Semantics

Structs are copied on assignment, parameter passing, and return by default:

```wado
let a = Point { x: 1, y: 2 };
let b = a;  // b is a copy of a
b.x = 10;   // does not affect a
```

**Explicit move** transfers ownership without copying:

```wado
let b = move a;  // a is invalidated
```

**Rationale**:

- Value semantics are easier to reason about (no aliasing surprises)
- Aligns with Wado's "explicitness" philosophy
- Move semantics already in the spec provide escape hatch for performance

### 2. Automatic Heap Promotion

When a reference escapes, the referenced value is automatically heap-promoted. The compiler detects escape through these conditions:

| Escape Condition                      | Example                                          |
| ------------------------------------- | ------------------------------------------------ |
| Passed to function with `stores[...]` | `store(&local)` where `store` has `stores[data]` |
| Returned from function                | `return &local;`                                 |
| Stored in global variable             | `GLOBAL = Some(&local);`                         |
| Stored in struct field                | `Container { data: &local }`                     |
| Captured by escaping closure          | `return` closure that uses `local`               |

```wado
fn example() {
    let local = Data{};
    let handle = store(&local);  // local promoted to heap (store has stores[])
}
```

**Heap promotion is automatic** based on escape analysis:

- Compiler detects when a reference might outlive its scope
- Promotion is transparent to the programmer
- Similar to Go's escape analysis

**Rationale**:

- Automatic promotion removes burden from programmer
- GC handles the heap-allocated values
- No manual stack/heap decision needed

**Implementation note**: Wasm GC structs are semantically heap-allocated. However, the Wado compiler MAY represent non-escaping structs as Wasm locals (decomposed fields) instead of `struct.new`. This is a compiler optimization, not language semantics.

### 3. The `stores[...]` Keyword for Reference Storage

Functions and functors that store references must declare this with `stores[...]`:

```wado
// Function that stores a reference parameter
fn store(data: &Data) -> Handle with stores[data] {
    // can store `data`
}

// Function that does NOT store (no stores declaration)
fn process(data: &Data) -> Result {
    // cannot store `data`, only use it
}
```

**Syntax**: `with stores[param1, param2, ...]`

- Uses `[...]` (not `{...}`) to avoid ambiguity with function body
- Familiar to C++ developers (lambda capture syntax)
- `stores` is a **keyword**, not an effect interface
- Only `stores` can use `[...]` syntax; regular effects cannot

**Naming Rationale**: The keyword is `stores` (not `captures`) because:

- "Capture" is used in closure semantics (`let f = || x + 1` captures `x`)
- `stores` describes what the function _does_ with the reference—it stores it
- This avoids conflating two different concepts: closures capturing variables vs functions storing references

### 4. Stores Rules

| Declaration                    | Stores Behavior                             |
| ------------------------------ | ------------------------------------------- |
| Named function with `&T` param | Must declare `stores[param]` if storing     |
| Closure using outer variable   | Closure captures inferred from usage        |
| Functor type (`Fn(...)`)       | Must declare `stores[0]` etc. if it stores  |
| Functor value itself           | No stores needed (functors are value types) |

**Note on functors**: In Wasm, functors are `funcref` values. Storing a functor itself (not its parameters) does not require `stores[...]` because functors have value semantics—they are copied when assigned or passed.

**Named functions**:

```wado
fn store(data: &Data) -> Handle with stores[data] { ... }
fn process(data: &Data) -> Result { ... }  // no stores = cannot store
```

**Closures**:

```wado
// Closure captures inferred from usage
let f = || { return local_var; };
// Inferred type: Fn() -> i32 (captures local_var)

// Explicit stores annotation for parameters
let g = |data| stores[data] { ... };
```

**Functor types**:

```wado
// Must declare stores in type (positional: 0 = first parameter)
fn take_storing(f: Fn(&Data) with stores[0]) { ... }
fn take_pure(f: Fn(&Data) -> Result) { ... }  // cannot store
```

### 5. Heap Promotion Based on Stores

When a reference is passed to something declaring `stores[...]`, the referenced value is automatically heap-promoted:

```wado
fn store(data: &Data) -> Handle with stores[data] { ... }

fn caller() {
    let local = Data{};
    let handle = store(&local);  // local automatically heap-promoted
}
```

**Rationale**:

- Compiler can determine heap promotion statically
- No runtime checks needed
- Transparent to programmer

### 6. Closures Capture by Value

To keep one rule across the entire language, closures capture each free variable by **deep copy at closure creation time** — the same value semantics that applies to assignment, parameter passing, and return. The closure body operates on its own copies; later mutations to the outer binding are not observed by the closure, and the closure body cannot mutate outer variables directly.

```wado
let outer = 10;
let f = || outer;          // outer is copied into the closure
let outer = 20;            // rebinding the outer name does not affect f
assert f() == 10;
```

For stateful or shared-mutable closures, capture a reference. References (`&T`, `&mut T`) are the only types in Wado that alias their referent, so a copied reference still points to the same underlying value.

```wado
fn make_counter() -> Fn() -> i32 {
    let mut count = 0;
    let cref: &mut i32 = &mut count;
    return || {
        *cref += 1;        // mutation goes through the captured reference
        return *cref;
    };
}
```

A closure that needs to mutate or share state captures a reference, exactly like any other piece of code that wants to alias.

**Rationale**:

- One rule for the whole language; closures are not a special case
- Aliasing requires the same syntactic marker (`&T` / `&mut T`) everywhere
- GC manages the lifetime of any captured reference's referent
- Escape tracking for closures reuses the existing `stores[...]` machinery — if a returned closure captures `&local`, the local is heap-promoted by the same rules that govern any escaping reference

Note: Closures that capture variables use "capture" terminology; the `stores[...]` keyword is for functions that store reference _parameters_ passed to them.

### 7. Heap Promotion of Referents Captured by Closures (Non-Normative)

Because the language-level capture is always by value, the closure's environment struct itself contains plain copies and references. The compiler's job is the same one it does for escaping references in any other context: if a closure captures `&local` and the closure outlives `local`'s lexical scope (e.g. the closure is returned), `local` is heap-promoted via the rules in §2 / §5. No closure-specific lifetime tracking is required beyond the existing escape analysis.

### 8. Edge Cases

#### Returning a Reference to Local

Allowed. The compiler promotes the local to heap automatically via escape analysis:

```wado
fn make_data() -> &Data {
    let local = Data{};
    return &local;  // OK: local promoted to heap
}
```

The return type `&Data` from a function that creates the data means "heap-allocated, GC-managed reference." This is different from storing a parameter—no `stores[...]` declaration is needed because there's no parameter being stored. The compiler detects that `local` escapes via return and promotes it to heap.

#### Storing in Globals

Allowed. The referenced value is promoted to heap:

```wado
let mut GLOBAL: Option<&Data> = None;

fn store_global(data: &Data) with stores[data] {
    GLOBAL = Some(data);  // OK: data's source promoted to heap
}
```

#### Storing in Struct Fields

Requires `stores[...]` declaration:

```wado
struct Container {
    data: &Data,
}

fn make_container(data: &Data) -> Container with stores[data] {
    return Container { data };  // Must declare stores
}
```

#### Storing Through Method Calls

The method must declare `stores[...]`:

```wado
impl Array<&Data> {
    fn push(&mut self, item: &Data) with stores[item] {
        // stores item
    }
}

fn example(list: &mut Array<&Data>, data: &Data) with stores[data] {
    list.push(data);  // Caller must also declare stores
}
```

#### Generic Functions

The compiler detects stores through type propagation:

```wado
fn apply<T, R>(f: Fn(T) -> R, x: T) -> R {
    return f(x);  // apply doesn't store, just passes through
}

// If f's type is Fn(&Data) -> R with stores[0],
// compiler traces that x may be stored
```

If a generic function stores its parameter without declaring `stores[...]`, the compiler detects this and reports an error.

#### References to Primitives

References to primitives (`&i32`, `&bool`, etc.) follow the same rules as references to structs:

```wado
fn store_int(x: &i32) with stores[x] {
    SAVED_INT = Some(x);  // OK: stores declared
}

fn use_int(x: &i32) -> i32 {
    return *x + 1;  // OK: no storage, no stores needed
}
```

#### Multiple Closures Sharing Mutable State

Closures capture by value, so two closures that name the same outer variable each receive their own copy. To share mutable state across closures, capture a reference and let aliasing do the work:

```wado
fn multi_share() {
    let mut x = 0;
    let xref: &mut i32 = &mut x;

    let inc = || { *xref += 1; };
    let get = || { return *xref; };

    inc();
    inc();
    println(get());  // Prints: 2 — both closures see the same i32 via the shared reference
}
```

Each closure's environment holds its own copy of the reference value `xref`, but `&mut i32` aliases the underlying `i32`, so every read and write lands on the same location. If the closures escape `multi_share`, `x` is heap-promoted by the existing escape rules.

### 9. Component Model Boundaries

The `stores[...]` mechanism only applies **within a Wado component**. External Wasm modules are protected by Component Model boundaries:

| Boundary                     | Reference Behavior            | `stores[...]` Needed? |
| ---------------------------- | ----------------------------- | --------------------- |
| Within Wado component        | GC references passed directly | Yes                   |
| Wado builtins (wasm-bundled) | Controlled by Wado project    | Annotated correctly   |
| External Wasm module (CM)    | Data copied at boundary       | No                    |

**Why CM boundaries are safe**:

At Component Model boundaries, data is copied/serialized:

- `struct` → `record` (copied)
- `Array<T>` → `list<T>` (copied)
- `String` → `string` (copied)
- Resources use explicit `borrow<T>` / `own<T>`

```wado
use {external_fn} from "./foo.wasm" with { type: "wasm" };

fn caller() {
    let local = Data{};
    external_fn(local);  // CM boundary: local is COPIED, not referenced
}
```

The external component receives a **copy**, not a GC reference. Even if it "stores" the data, it stores its own copy—the original `local` is unaffected.

**Consequence**: `stores[...]` only needs to track escapes within Wado code. Cross-component calls are automatically safe.

## Consequences

### Positive

1. **Predictable value semantics**: No aliasing surprises with structs
2. **Automatic heap promotion**: Programmer doesn't manage stack vs heap
3. **Explicit store tracking**: `stores[...]` makes storage intent clear
4. **Type-safe escaping**: Can't accidentally escape references without declaration
5. **Go-like ergonomics**: Escape analysis is familiar pattern
6. **C++-like syntax**: `stores[...]` familiar to C++ developers (lambda capture syntax)
7. **Uniform closure capture model**: Captures use the same value semantics as everything else; share mutable state via references (`&T` / `&mut T`), no closure-specific syntax
8. **CM boundaries protect external calls**: No annotation needed for cross-component calls
9. **Clear terminology**: "stores" for function parameters, "captures" for closures

### Negative

1. **Copy overhead**: Value semantics may cause unexpected copies for large structs
   - **Mitigation**: Use `move` for large values, profiler will identify hotspots
2. **Learning curve**: `stores[...]` is a new concept
   - **Mitigation**: Clear error messages when stores declaration is missing
3. **Verbose functor types**: `Fn(&Data) with stores[0]` is long
   - **Mitigation**: Type inference reduces explicit annotations
4. **Different from Rust**: No lifetimes, different model
   - **Mitigation**: Simpler model is easier to learn

### Examples

**Basic value semantics**:

```wado
struct Point { x: i32, y: i32 }

let a = Point { x: 1, y: 2 };
let b = a;      // copy
let c = move a; // move, `a` invalidated
```

**Function with stores**:

```wado
// Storing a functor: no stores needed (functors are value types)
fn register_callback(cb: Fn(&Event)) -> Id {
    callbacks.push(cb);  // OK: cb is a funcref, copied by value
    return new_id();
}

// Functor that stores its parameter
fn register_storing_callback(cb: Fn(&Event) with stores[0]) -> Id {
    // cb may store references passed to it
    callbacks.push(cb);
    return new_id();
}

fn process_once(cb: Fn(&Event)) {
    cb(&event);  // uses but doesn't store
}
```

**Closure capture inference**:

```wado
fn create_adder(x: i32) -> Fn(i32) -> i32 {
    return |y| { return x + y; };  // closure captures x (inferred)
}
```

**Mixed with effects**:

```wado
fn store_and_log(data: &Data) -> Handle with Stdout, stores[data] {
    println("Storing data...");
    return create_handle(data);
}
```

## Terminology: Reference vs Pointer

Wado uses **"reference"** for `&T`, not "pointer":

| Type           | Term      | Built-in       | Characteristics                             |
| -------------- | --------- | -------------- | ------------------------------------------- |
| `&T`           | Reference | Yes (language) | GC-managed, non-null, no arithmetic         |
| `LinearPtr<T>` | Pointer   | No (library)   | Linear memory, nullable, arithmetic allowed |

**Rationale for "reference"**:

- Wasm GC uses "reference types" (`ref`, `ref null`)
- `&T` is safe, GC-managed, and non-null (use `Option<&T>` for nullable)
- No pointer arithmetic on references
- Aligns with Rust's terminology for `&T`

**`LinearPtr<T>` for FFI**:

For interop with bundled Wasm functions that use linear memory (e.g., `f64_to_buffer`), a library type `LinearPtr<T>` wraps `i32` offsets:

```wado
use {LinearPtr, linear_alloc, linear_free} from "core:memory";

fn format_float(value: f64) -> String {
    let ptr: LinearPtr<u8> = linear_alloc(64);
    f64_to_buffer(value, ptr);
    let result = read_string_from_linear(ptr);
    linear_free(ptr);
    return result;
}
```

This keeps the core language clean while providing escape hatch for low-level FFI.

## Theoretical Relationship: Stores and Effects

**Is storing an effect?**

Traditional effect systems (I/O, State, Exception) treat effects as "what the function DOES." Storing is about "what the function RETAINS."

However, in capability-based systems, storing is closely related to effects:

- Storing enables future effects (stored reference can be mutated later)
- Tracking stores is tracking _potential_ effects

Wado treats `stores` as a **separate mechanism** from effects:

- Effects (`with Stdout, FileSystem`) = authority to interact with external world
- Stores (`with stores[data]`) = authority to retain references

Both use the `with` keyword for consistency, but they are orthogonal concerns.

## References

- [Rust Ownership](https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html)
- [Go Escape Analysis](https://go.dev/doc/faq#stack_or_heap)
- [Swift Value Semantics](https://developer.apple.com/swift/blog/?id=10)
- [Scala 3 Capture Checking](https://docs.scala-lang.org/scala3/reference/experimental/cc.html)
- [Koka Perceus](https://koka-lang.github.io/koka/doc/book.html#sec-perceus)
- [C++ Lambda Captures](https://en.cppreference.com/w/cpp/language/lambda)
