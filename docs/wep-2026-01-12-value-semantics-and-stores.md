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

| Escape Condition                     | Example                                    |
| ------------------------------------ | ------------------------------------------ |
| Passed to a function that retains it | `store(&local)` where `store` keeps `data` |
| Returned from function               | `return &local;`                           |
| Stored in global variable            | `GLOBAL = Some(&local);`                   |
| Stored in struct field               | `Container { data: &local }`               |
| Captured by escaping closure         | `return` closure that uses `local`         |

```wado
fn example() {
    let local = Data{};
    let handle = store(&local);  // local promoted to heap
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

### 3. Two Escapes, Both Inferred

A reference parameter leaves a call in two ways, and they are different claims:

- Borrow-out — the result aliases the parameter's storage. `StrSlice::sub`,
  `List::as_slice`, `array_get_ref`. The caller already owns the referent, so
  nothing outlives anything.
- Retain — the reference reaches a global, or is written through a `&mut` the
  caller still holds. `array_set`, `array_fill`.

Neither is a safety condition. §1 and §2 put every referent under the GC, so a
reference that outlives its scope keeps its referent alive and there is nothing
for a declaration to prevent. Both are optimizer inputs, and §5 lists what each
one buys.

A body states both, so the compiler reads them from it rather than from a
declaration. `lower::plan::value_copy::stores` is that reading: an
interprocedural least fixpoint over the call graph, keeping retain in
`StoresFacts::escapes` and borrow-out in `StoresFacts::into_result`, publishing
the union to callers. Wado has no separate compilation — a published package
ships its sources ([Provider Metadata](./wep-2026-07-26-provider-metadata.md)) — so the fixpoint always has
every body it needs.

A function with a body therefore declares nothing. There is no `stores` row on
a function declaration, in a function type, or on a closure, and no obligation
for a programmer to discharge.

### 4. A Declaration Only Where There Is No Body

A body-less declaration is the exception: there is nothing to read, so it
states its two facts itself. It states them as attributes, next to the
`#[returns(...)]` that already carries borrow-out:

```wado
#[returns(part_of = arr)]
pub fn array_get_ref<T>(arr: &Array<T>, idx: i32) -> &T;

#[stores(value)]
pub fn array_set<T>(arr: &mut Array<T>, idx: i32, value: T);
```

`#[returns(owned)]` and `#[returns(part_of = p)]` state borrow-out;
`#[stores(p, ...)]` states retain. Both name parameters rather than positions,
and both report an argument that names none. Silence is the conservative
reading of whichever consumer asks — for `#[returns]` that is "allocates",
which elides copies.

Where each is accepted:

| Declaration                                        | `#[stores]` / `#[returns]` |
| -------------------------------------------------- | -------------------------- |
| `core:builtin`, body-less                          | Yes                        |
| CM component import, WASI, `.wasm` / `.wat` import | Yes                        |
| Anything with a body                               | Error — the body states it |

A program has no body-less function of its own to put these on — the one it can
write is a [declared absence](./wep-2026-09-13-declared-absence.md), which
reserves a name and is never called — so in practice the attributes belong to
`core:builtin` and to imports. That is the sense in which §3 removes escape
declaration from user code entirely, rather than making it optional.

An attribute rather than a `with` row, because retention is not part of the
function's type. Two declarations that differ only in what they retain are one
type, and a call resolves against the declaration it names, never against a row
carried by the type.

The row on a function type is a separate question from the clause on a function,
and it is the one an indirect call reads. An empty row means "retains nothing"
today, and that reading is true only because the frontend keeps it true: a
closure may not retain a reference parameter, and a named function that does is
rejected where a non-retaining functor type is expected. Remove the obligation
without replacing the row's source and the empty row becomes a lie, so an
indirect call must instead assume every reference position escapes.

What the row needs is a source, not a new analysis, and the fixpoint of §3 is
it. A function value of a given functor type is minted in exactly two places — a
reference to a named function, and a closure literal — so joining the facts of
every such expression of one type bounds every call through a value of that
type. That join belongs beside the per-function facts, in the same pass, since
both node kinds are already on its walk.

Sourcing the row this way settles what the row is: a derived fact about a type,
not part of its identity. Two function types differing only in retention are one
type, nothing is checked at coercion, and retention leaves the mangled type
name. The precision it recovers is per type rather than per call site, which is
a gap below.

A closure declares neither effects nor stores. The parser still reads a `with`
row where one would go, and reports that the compiler does not carry it yet.
That keyword is always the closure's row, whether it follows the parameter list
or the return type. It never starts a handler expression, so a handler reaches
the body through a block or a pair of parentheses:

```wado
let f = || (with Log => &mut sink do { Log::emit(`hi`); });
let g = || { with Log => &mut sink do { Log::emit(`hi`); } };
```

Storing a functor value itself needs no declaration either way: functors are
`funcref` values with value semantics, copied when assigned or passed.

### 5. What the Two Facts Buy

Components running on GC hold a reference as a reference, so nothing is promoted
to reach it. What the facts buy is what the compiler may then stop doing to the
argument, which each consumer reads for itself:

- `lower::plan::mut_ref_writeback` writes no `&mut` argument back at a call that
  retains it: the borrow outlives the call, so the call is no place to write it.
- `lower::plan::value_copy` reads the union. A local passed where the callee
  retains it or hands it out is borrow-escaped and cannot be moved out of.
- `wir_optimize::const_forward` forwards no constant into a retained parameter.
- `niri` folds a call at compile time on what the body does, not on retention:
  a compile-time evaluation keeps nothing past itself.

Keeping the two apart is what makes the second precise. An iterator holds a
reference to what it walks, so reading both as one makes every `&List` parameter
retained the moment a body iterates it, while a `collect()` that drops the
iterator retains nothing.

### 6. Closures Capture by Reference

Closures auto-capture each free variable by reference. The compiler infers the reference kind (`&T` for read-only, `&mut T` for mutating) from body usage; the closure type is `fn` if all captures are read-only, `fn mut` if any are mutating. See [Closure Implementation](./wep-2026-01-16-closure-implementation.md) for the full design.

```wado
let mut count = 0;
let s = "hello";

let mut inc = || count += 1;     // captures &mut count; type fn mut() -> ()
let get = || count;               // captures &count; type fn() -> i32
let greet = || println(s);        // captures &s; type fn() with Stdout
```

Multiple closures referring to the same outer binding automatically share state through the underlying location — no explicit `&mut foo` dance needed:

```wado
fn make_counter() -> fn mut() -> i32 {
    let mut count = 0;
    return || {
        count += 1;        // captures &mut count
        return count;
    };
}
```

The closure value itself follows Wado value semantics: deep-copied on assignment, parameter passing, and return. Because env fields hold reference values (under auto-by-reference), copying a closure copies references that alias — all copies observe the same captured bindings.

**Rationale**:

- Matches Rust ergonomics for capture inference, with the borrow-checker complexity dropped.
- Aliasing through `&mut` captures is consistent with Wado's general rule that references are the only aliasing types.
- Escape tracking for closures reuses the existing escape-analysis machinery — if a returned closure captures `&local`, the local is heap-promoted by the same rules that govern any escaping reference.

Note: Closures use "capture" terminology; the `stores[...]` keyword is for functions that store reference _parameters_ passed to them. These are separate mechanisms.

### 7. Heap Promotion of Referents Captured by Closures (Non-Normative)

When a closure escapes its declaring scope (returned, stored in a struct field, etc.), the captured bindings must outlive the closure. The compiler heap-promotes them via the same machinery as any escaping reference (§2 / §5). No closure-specific lifetime tracking is required beyond the existing escape analysis.

### 8. Edge Cases

#### Returning a Reference to Local

Allowed. The compiler promotes the local to heap automatically via escape analysis:

```wado
fn make_data() -> &Data {
    let local = Data{};
    return &local;  // OK: local promoted to heap
}
```

The return type `&Data` from a function that creates the data means "heap-allocated, GC-managed reference." The compiler detects that `local` escapes via return and promotes it to heap.

#### Storing in Globals

Allowed. The referenced value is promoted to heap, and the walk records `data`
as retained:

```wado
let mut GLOBAL: Option<&Data> = None;

fn store_global(data: &Data) {
    GLOBAL = Some(data);
}
```

#### Storing in Struct Fields

Allowed. Whether the reference leaves with the result or lands somewhere the
caller cannot see follows from where `Container` goes:

```wado
struct Container {
    data: &Data,
}

fn make_container(data: &Data) -> Container {
    return Container { data };
}
```

#### Storing Through Method Calls

A caller that hands its own reference parameter to a retaining callee retains it
in turn, and the fixpoint carries that along the call graph:

```wado
impl List<&Data> {
    fn push(&mut self, item: &Data) { ... }
}

fn example(list: &mut List<&Data>, data: &Data) {
    list.push(data);  // `data` is retained here too
}
```

#### Generic Functions

A pass-through keeps nothing of its own:

```wado
fn apply<T, R>(f: fn(T) -> R, x: T) -> R {
    return f(x);
}
```

What `f` does with `x` is not visible in `f`'s type (§4), so a call through it
assumes every reference position escapes.

#### References to Primitives

References to primitives (`&i32`, `&bool`, etc.) follow the same rules as references to structs:

```wado
fn store_int(x: &i32) {
    SAVED_INT = Some(x);  // retained
}

fn use_int(x: &i32) -> i32 {
    return *x + 1;  // nothing kept
}
```

#### Multiple Closures Sharing Mutable State

Closures auto-capture by reference (§6), so two closures naming the same outer variable share the underlying location automatically:

```wado
fn multi_share() {
    let mut x = 0;

    let mut inc = || x += 1;          // captures &mut x; type fn mut() -> ()
    let get = || x;                    // captures &x; type fn() -> i32

    inc();
    inc();
    println(get());  // Prints: 2 — both closures observe the same x
}
```

Each closure's environment holds a reference to `x`. Because references alias, every read and write lands on the same location. If the closures escape `multi_share`, `x` is heap-promoted by the existing escape rules.

### 9. Component Model Boundaries

Escape tracking is a within-component concern. A call that crosses a Component
Model boundary copies, so it carries nothing of the caller's storage across:

| Boundary                     | Reference Behavior            | Where the fact comes from |
| ---------------------------- | ----------------------------- | ------------------------- |
| Within Wado component        | GC references passed directly | The body (§3)             |
| Wado builtins (wasm-bundled) | Controlled by Wado project    | The attribute (§4)        |
| External Wasm module (CM)    | Data copied at boundary       | The copy — nothing to say |

A CM import is still a body-less declaration, so §4's attributes are accepted on
one. Nothing under `lib/wasi/` needs them today, because the copy already
answers; the attribute is there for an import whose lowering does not copy.

**Why CM boundaries are safe**:

At Component Model boundaries, data is copied/serialized:

- `struct` → `record` (copied)
- `List<T>` → `list<T>` (copied)
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

1. Predictable value semantics: no aliasing surprises with structs.
2. Automatic heap promotion: the programmer does not manage stack versus heap.
3. Nothing to declare and nothing to get wrong: escape is a property of the
   body, and the body is what the compiler reads.
4. The two escapes stay apart, so neither consumer reads a fact meant for the
   other (§5).
5. Go-like ergonomics: escape analysis is a familiar pattern.
6. Auto-capture by reference for closures: captures share Wado's general
   reference-aliasing semantics, with `&T` / `&mut T` inferred per binding from
   body usage (see [Closure Implementation](./wep-2026-01-16-closure-implementation.md));
   no separate aliasing model is needed for closures.
7. CM boundaries protect external calls: the copy answers, so no annotation is
   needed for a cross-component call.

### Negative

1. Copy overhead: value semantics may cause unexpected copies for large structs.
   - Mitigation: use `move` for large values; the profiler identifies hotspots.
2. A signature no longer says what a function retains, so a reader of a `pub`
   API learns it from the body or not at all.
   - Mitigation: none in the language. `wado doc` could render the inferred
     fact, which is a gap below.
3. An indirect call is answered per functor type, not per call site (§4), so one
   retaining function value coarsens every call through the same type.
   - Mitigation: seventeen signatures in the corpus take a functor with a
     reference parameter at all, four of them comparators in the prelude.
     Sharpening it to the call site is a gap below.
4. Different from Rust: no lifetimes, different model.
   - Mitigation: the simpler model is easier to learn.

### Examples

**Basic value semantics**:

```wado
struct Point { x: i32, y: i32 }

let a = Point { x: 1, y: 2 };
let b = a;      // copy
let c = move a; // move, `a` invalidated
```

**Retention read from the body**:

```wado
fn register(data: &Data) -> Handle {
    REGISTRY.push(data);   // retained: reaches a global
    return new_handle();
}

fn process(data: &Data) -> Result {
    return compute(*data); // nothing kept
}

fn view(s: &String) -> StrSlice {
    return s.as_str_slice();  // handed out with the result, not retained
}
```

**Retention declared where there is no body**:

```wado
#[stores(value)]
pub fn array_fill<T>(arr: &mut Array<T>, offset: i32, value: T, len: i32);
```

**Closure capture inference**:

```wado
fn create_adder(x: i32) -> fn(i32) -> i32 {
    return |y| { return x + y; };  // closure captures x (inferred)
}
```

**Mixed with effects**:

```wado
fn store_and_log(data: &Data) -> Handle with Stdout {
    println("Storing data...");
    return create_handle(data);
}
```

## Roadmap

1. [ ] Add `#[stores(p, ...)]`, accepted on a body-less declaration and reported
       on one with a body. Done when `BuiltinDeclaration::stores` is fed from the
       attribute and `array_set` / `array_fill` carry the same fact they carry
       today.
2. [ ] Snapshot the declarations of every body-less function at link, not only
       `core:builtin`'s. Done when `record_builtin_declaration` keys on the
       absence of a body, so a CM import or a `.wasm` / `.wat` asset import can
       carry §4's attributes.
3. [ ] Measure what a conservative reading of an indirect call would cost, at the
       seventeen exposed signatures §4 names. Done when the number is known: it
       says how much item 4 is worth, and how much the gap below it leaves.
4. [ ] Give the functor type's row an inferred source, so an indirect call keeps
       today's precision once item 5 stops the frontend keeping it (§4). The
       fixpoint gains a second map, from functor `TypeId` to facts, joined from
       every expression that mints a function value of that type — a `FuncRef`
       takes the named function's facts, a `Closure` its body's. A function value
       of type `T` is minted nowhere else, so the join bounds whatever reaches an
       `IndirectCall`, with no points-to analysis and no phase to move:
       `stores.rs` already walks both node kinds. Done when `indirect` reads the
       join instead of a declared row, and a `sort_by` comparator that retains
       neither argument stops pinning them. Before item 5, not after: it is what
       makes an empty row true once a closure may retain.
5. [ ] Delete `check_stores_semantic` and what only it reaches — the oracle, the
       return-provenance fixpoint, the escape walk, the type-reachability memo.
       Done when `effect_check.rs` reports effects and default purity only, and
       the fixtures asserting a stores diagnostic are gone with it. This closes
       issues #2049 and #2050.
6. [ ] Strip the declarations from the corpus: 137 under `wado-compiler/lib`,
       3023 under `package-gale` — 2957 of them Kiln output, so Gale's generator
       stops emitting them first — and 2 under `package-marl`. Done when no
       function declaration in the corpus carries a `stores` clause and
       `mise run test-wado` passes.
7. [ ] Remove the `stores` clause from the grammar, in both declaration and
       function type position, after nothing writes one. Done when the parser
       rejects both.
8. [ ] Take retention out of the type, which §4 says it is no part of: delete
       `mangle_stores_member` from the function type name, the subset check in
       `typecheck::check_at`, and `ResolvedType::Function::stores`, which item 4
       replaces. Done when two function types differing only in retention are one
       type, and golden type names carry no stores member.
9. [ ] Seed `lower::plan::value_copy::stores` from the attribute alone. Done when
       `declared_positions` reads `BuiltinDeclaration` and the `hands_out_result`
       heuristic is gone, closing the "declared `stores` the walk does not
       confirm" gap in [Ownership Analysis](./wep-2026-05-21-resource-ownership.md).
10. [ ] Delete the `func.stores.is_empty()` gate in `niri::is_ctfe_eligible`, per
        §5. Done when compile-time evaluation is decided by the body alone.
11. [ ] Show the inferred facts, which no signature states any more. Done when
        `wado query hover` and `wado doc` say what a function retains and hands
        out — which needs the facts in the language service, where only the
        frontend runs today.
12. [ ] Record the effect on `benchmark/` and `wasm-size/`. Done when both
        READMEs carry the new numbers.

## Known gaps

- [ ] Read an indirect call per call site rather than per functor type. Roadmap
      item 4 joins every function value of one type into one answer, so a
      comparator that retains an argument coarsens every other call through the
      same type. Closing it takes knowing which function values reach which call
      — a points-to analysis, which nothing here is;
      `lower::plan::value_copy::funcset` is a borrow-keyed container, not that.

- [ ] Say which place a retained parameter lands in. Retention records that `p`
      outlives the call, not where it goes, and `array_copy(dst, _, src, _, _)`
      is the case that needs the difference: for a reference `T` its elements
      reach `dst` afterwards, so `src` escapes into a place the caller may still
      hold. Read as plain retention it marks the whole reference borrow-escaped
      at all twenty call sites. Fifteen are `Array<u8>`, where a scalar element
      escapes nothing; the other five are the backing-array swap in `List::grow`
      and its neighbours, which hand elements from an array they then discard, so
      it would cost the hottest paths in the stdlib for a leak none of them has.
      §4's attribute has room for the destination — `#[stores(src, into = dst)]`
      — and nothing reads one.

- [ ] Populate `NirFunction::stores_aliased_locals` from a retaining call, or say
      it is not that. Its doc reads "when inlining a function that stores `x`
      with argument `&local`, `local` is added here", and no writer does that:
      `sroa` adds the aliases it mints, `inline`, `cold_outline` and `dae` carry
      and renumber what is already there. Either the field is fed only by SROA
      and the doc names an intent, or a caller-side write is missing.

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

## Retention Is Not an Effect

Traditional effect systems (I/O, State, Exception) treat an effect as what a
function does. Retention is what it keeps, and in a capability-based reading the
two look close: a retained reference can be mutated later, so tracking retention
looks like tracking a potential effect.

Wado keeps them apart, and §3 and §4 are where the difference shows. An effect
is authority a caller grants and a handler can intercept, so it belongs to the
signature and the caller must see it. Retention grants nothing and intercepts
nothing; it only tells the compiler what it may stop doing to an argument. So an
effect is declared in the `with` row and is part of the function's type, while
retention is read from the body — or, where there is none, stated as an
attribute that the type does not carry.

## References

- [Rust Ownership](https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html)
- [Go Escape Analysis](https://go.dev/doc/faq#stack_or_heap)
- [Swift Value Semantics](https://developer.apple.com/swift/blog/?id=10)
- [Scala 3 Capture Checking](https://docs.scala-lang.org/scala3/reference/experimental/cc.html)
- [Koka Perceus](https://koka-lang.github.io/koka/doc/book.html#sec-perceus)
- [C++ Lambda Captures](https://en.cppreference.com/w/cpp/language/lambda)
