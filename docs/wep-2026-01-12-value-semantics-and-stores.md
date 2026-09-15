# WEP: Value Semantics and Reference Retention

## Context

Wado targets Wasm GC, where structs and arrays are reference types (heap-allocated, garbage-collected). However, the language design needs to decide on the semantics exposed to programmers:

1. What semantics should structs have — value (copy on assign) or reference (alias on assign)?
2. Where are local variables allocated, stack or heap?
3. How should `f(&local_var)` be handled when `f` might keep the reference?

Question 2 turned out to have no second answer to pick, which is §2, and that
takes the danger out of question 3: it is a question about what the optimizer
may skip, not about whether a program is well-formed.

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

When a function receives `&local_var`, the question is whether the function will
keep that reference:

```wado
fn caller() {
    let local = Data{};
    store(&local);  // Will `store` keep a reference to `local`?
}
```

In a language with a stack, the answer decides where `local` lives, and getting
it wrong is a dangling reference. Wado has no stack to be wrong about (§2), so
the answer decides only how much the compiler may do to `local` — which is why
the same question reaches the optimizer and never the programmer.

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

### 2. There Is No Stack, So Nothing Is Promoted

A programmer never decides between stack and heap, and never annotates a value
so that it may outlive a scope. That is the decision; the mechanism that carries
it is not promotion.

Under [GC in Components](./wep-2026-03-28-gc-in-components.md) every struct,
array and string a Wado program builds is a Wasm GC allocation from the moment
it is built. A local holds a reference to it, not the value itself, so a
reference that outlives its local's scope already points at a live object and
the collector keeps it live. There is no second home to move it to, and no
escape condition that could invalidate one.

The compiler's freedom therefore runs the other way. A value the escape walk
finds nowhere may be taken _out_ of the heap — held as decomposed Wasm locals
(`optimize::sroa` and its neighbours) — or moved rather than copied at its last
use. An escape does not promote anything; it withdraws one of those, which is
what §5 enumerates. Being a compiler optimization, neither is language
semantics: a program means the same thing with both turned off.

This is why none of §3's facts is a safety condition. Go's escape analysis
decides where a value lives and must be right or the program is wrong; Wado's
decides only what the optimizer may skip, and a wrong answer costs a copy.

### 3. Escape Is Inferred, Never Declared

A reference parameter leaves a call in two ways, and they are different claims:

- Borrow-out — the result aliases the parameter's storage. `StrSlice::sub`,
  `List::as_slice`, `array_get_ref`. The caller already owns the referent, so
  nothing outlives anything.
- Retain — the reference reaches a global, or is written through a `&mut` the
  caller still holds. `array_set`, `array_fill`.

Neither is a safety condition, for §2's reason: there is nothing for a
declaration to prevent. Both are optimizer inputs, and §5 lists what each one
buys.

A body states both, so the compiler reads them from it rather than from a
declaration. `lower::plan::value_copy::stores` is that reading: an
interprocedural least fixpoint over the call graph, publishing the union to
callers. Wado has no separate compilation — a published package ships its
sources ([Provider Metadata](./wep-2026-07-26-provider-metadata.md)) — so the
fixpoint always has every body it needs.

A fact is one of three, by where the reference lands:

| Channel       | Where it lands                     | Example                    |
| ------------- | ---------------------------------- | -------------------------- |
| `escapes`     | Somewhere the caller cannot see    | a global                   |
| `into_result` | The return value — borrow-out      | `List::as_slice`           |
| `into_param`  | A named parameter the caller holds | `array_copy`, `List::push` |

Retain is two channels rather than one because a destination the caller can name
bounds the retention: a reference put into a parameter the caller owns lives as
long as that parameter, while one that reaches a global is bounded by nothing.

`into_param` is what `into = dst` states, and it is a channel of the walk rather
than a reading of an attribute: a body that puts a reference into one of its own
parameters lands there too. Without that, the fact would stop at the one
declaration carrying it — `array_copy` would be precise while `List::push`,
`List::extend` and `String::push_str`, the same shape with a body, stayed at
`escapes`, and the precision would be lost one call up.

A function with a body therefore declares nothing. There is no escape row on a
function declaration, in a function type, or on a closure, and no obligation for
a programmer to discharge.

### 4. A Declaration Only Where There Is No Body

A body-less declaration is the exception: there is nothing to read, so it states
its facts itself. It states them as attributes, next to the `#[returns(...)]`
that already carries borrow-out:

```wado
#[returns(part_of = arr)]
pub fn array_get_ref<T>(arr: &Array<T>, idx: i32) -> &T;

#[retain(value, into = arr)]
pub fn array_set<T>(arr: &mut Array<T>, idx: i32, value: T);
```

`#[returns(owned)]` and `#[returns(part_of = p)]` state borrow-out.
`#[retain(...)]` states retain, and names one retained thing per attribute,
repeated where there is more than one — so each carries its own destination
without the attribute grammar growing a way to group them:

```wado
#[retain(value, into = arr)]               // `value` itself, landing in `arr`
#[retain(elements_of = src, into = dst)]   // `src`'s elements, landing in `dst`
#[retain(data)]                            // `data` itself, destination unknown
```

A bare name is the parameter as a whole and `elements_of = p` is that
parameter's elements, which is the difference `array_copy` needs: what reaches
`dst` is what `src` holds, not `src`. `into = q` names where it lands; without
it the destination is unknown, which is the conservative reading. Every form
names a parameter rather than a position, reusing the `key = parameter` shape
`part_of` already has, and reports an argument that names none.

Silence is not uniformly the safe reading. `#[returns]` is the exception: the
plan phase takes a missing one for "allocates", which elides copies and is wrong
for a declaration that does hand out an argument's storage. `core:builtin` is
compiler-owned, so `link` asserts that a builtin reading through a reference and
returning storage declared one — a missing declaration there is the compiler's
bug, not a program's. Extending the attributes past `core:builtin` has to answer
that assert, which roadmap item 2 carries.

Where each is accepted:

| Declaration                                        | `#[retain]` / `#[returns]`   |
| -------------------------------------------------- | ---------------------------- |
| `core:builtin`, body-less                          | Yes                          |
| CM component import, WASI, `.wasm` / `.wat` import | Yes                          |
| `trait` / `interface` method requirement           | Error — the impl's body does |
| Anything with a body                               | Error — the body states it   |

A trait method requirement has no body of its own, but every call to it is
statically dispatched to an impl that has one, and monomorphization resolves
that before the fixpoint runs — so an attribute there would never be read, and
one contradicting the impl would never be caught. The eleven requirements
that carried a `stores` clause (`AsStrSlice`, `AsSlice`, `AsByteSlice`, and
eight across `Serializer` and `Deserializer` in `core:serde`) all state
borrow-out their impls already state, so they lose it rather than convert it.

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
and it is the one an indirect call reads. That row used to mean "retains
nothing", and it meant it only because the frontend kept it true: a closure
could not retain a reference parameter, and a named function that did was
rejected where a non-retaining functor type was expected. Removing the
obligation removed that guarantee with it. The row is now empty on every
function type and carries no claim at all, so an indirect call assumes every
reference position escapes.

What the row needs to say something again is a source, not a new analysis, and
the fixpoint of §3 is it. A function value of a given functor type is minted in exactly two places — a
reference to a named function, and a closure literal — so joining the facts of
every such expression of one type bounds every call through a value of that
type. That join belongs beside the per-function facts, in the same pass, since
both node kinds are already on its walk.

Sourcing the row this way settles what the row is: a derived fact about a type,
not part of its identity. Two function types differing only in retention are one
type, nothing is checked at coercion, and retention leaves the mangled type
name. The precision it recovers is per type rather than per call site, which is
a gap below.

A closure declares no effects, and no more than a named function does about what
it retains. The parser still reads a `with` row where one would go, and reports
that the compiler does not carry it yet.
That keyword is always the closure's row, whether it follows the parameter list
or the return type. It never starts a handler expression, so a handler reaches
the body through a block or a pair of parentheses:

```wado
let f = || (with Log => &mut sink do { Log::emit(`hi`); });
let g = || { with Log => &mut sink do { Log::emit(`hi`); } };
```

Storing a functor value itself needs no declaration either way: functors are
`funcref` values with value semantics, copied when assigned or passed.

### 5. What the Facts Buy

Nothing is promoted to reach a retained reference (§2). What the facts buy is
what the compiler may then stop doing to the argument, which each consumer reads
for itself:

- `lower::plan::mut_ref_writeback` writes no `&mut` argument back at a call that
  retains it: the borrow outlives the call, so the call is no place to write it.
- `lower::plan::value_copy` reads the union. A local passed where the callee
  retains it or hands it out is borrow-escaped and cannot be moved out of.
- `wir_optimize::const_forward` forwards no constant into a retained parameter.
- `niri` refuses to fold a call whose result could embed a retained reference,
  because the engine has no reference values and the result would be a snapshot
  the next write to that storage leaves stale. It also refuses any retaining
  call outright, which is the blanket reading roadmap item 10 narrows to the
  first.

Keeping the channels apart is what makes each precise. An iterator holds a
reference to what it walks, so folding `into_result` into `escapes` makes every
`&List` parameter retained the moment a body iterates it, while a `collect()`
that drops the iterator retains nothing. Folding `into_param` in costs the same
way: a reference put into a parameter the caller owns is bounded by that
parameter's extent, and reading it as "somewhere the caller cannot see" throws
the bound away.

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
- Capturing by reference costs no lifetime machinery of its own (§7), so the ergonomics are had without the analysis that usually pays for them.

Note: closures use "capture" terminology for the outer bindings they name.
Retention is about the reference _parameters_ a call is handed. These are
separate mechanisms.

### 7. An Escaping Closure Needs No Lifetime Rule

When a closure escapes its declaring scope — returned, stored in a struct field
— its captured bindings outlive the scope they were declared in. Nothing has to
arrange that: each referent is a GC allocation the capture holds a reference to
(§2), so the collector keeps it. The only thing the escape changes is what the
optimizer may do to the binding, which is §5's list.

### 8. Edge Cases

#### Returning a Reference to Local

Allowed, and there is nothing to arrange:

```wado
fn make_data() -> &Data {
    let local = Data{};
    return &local;  // OK: the referent outlives the frame
}
```

`local` holds a reference to a GC allocation, and returning it returns that
reference. What the escape costs is the chance to scalarize `local` or to move
out of it, not a relocation.

#### Storing in Globals

Allowed. The referent is already where the global needs it, and the walk records
`data` as retained:

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

Each closure's environment holds a reference to `x`. Because references alias, every read and write lands on the same location. If the closures escape `multi_share`, that location outlives the call on its own (§7).

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

### Why CM boundaries are safe

At Component Model boundaries, data is copied or serialized:

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

The external component receives a copy, not a GC reference. Whatever it keeps,
it keeps its own copy, and the original `local` is unaffected. So escape
tracking only has to reach within Wado code: a cross-component call is safe on
its own.

## Consequences

### Positive

1. Predictable value semantics: no aliasing surprises with structs.
2. No stack-versus-heap decision to make or to get wrong, and no annotation that
   lets a value outlive a scope: every value is on the GC heap already.
3. Nothing to declare and nothing to get wrong: escape is a property of the
   body, and the body is what the compiler reads.
4. The two escapes stay apart, so neither consumer reads a fact meant for the
   other (§5).
5. An escape analysis that can only cost a copy: a wrong answer is slow code,
   never wrong code, so it may be tuned without a correctness argument (§2).
6. Auto-capture by reference for closures: captures share Wado's general
   reference-aliasing semantics, with `&T` / `&mut T` inferred per binding from
   body usage (see [Closure Implementation](./wep-2026-01-16-closure-implementation.md));
   no separate aliasing model is needed for closures.
7. CM boundaries protect external calls: the copy answers, so no annotation is
   needed for a cross-component call.

### Negative

1. Copy overhead: value semantics may cause unexpected copies for large structs.
   - Mitigation: use `move` for large values; the profiler identifies hotspots.
2. A bodied function's signature no longer says what it retains, so a reader of
   a `pub` API learns it from the body or not at all.
   - Mitigation: partial. Roadmap item 13 surfaces the attributes a body-less
     declaration carries, which is every declaration that states anything;
     nothing renders the inferred fact for a bodied one.
3. An indirect call is answered by arity alone: with retention out of the type,
   nothing at the call names the body that will run.
   - Mitigation: partial, and it differs by reader — the copy analysis assumes
     every argument is retained, the write-back that none is. Seventeen
     signatures in the corpus take a functor with a reference parameter at all,
     four of them comparators in the prelude. Roadmap item 4 gives both an
     inferred row; reading it per call site rather than per type is a gap below.
4. Different from Rust: no lifetimes, different model.
   - Mitigation: the simpler model is easier to learn.

### Examples

### Basic value semantics

```wado
struct Point { x: i32, y: i32 }

let a = Point { x: 1, y: 2 };
let b = a;      // copy
let c = move a; // move, `a` invalidated
```

### Retention read from the body

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

### Retention declared where there is no body

```wado
#[retain(value)]
pub fn array_fill<T>(arr: &mut Array<T>, offset: i32, value: T, len: i32);
```

### Closure capture inference

```wado
fn create_adder(x: i32) -> fn(i32) -> i32 {
    return |y| { return x + y; };  // closure captures x (inferred)
}
```

### Mixed with effects

```wado
fn store_and_log(data: &Data) -> Handle with Stdout {
    println("Storing data...");
    return create_handle(data);
}
```

## Roadmap

1. [x] Add `#[retain(...)]`, accepted on a body-less declaration and reported on
       one with a body or on a trait requirement (§4). Its `into` and
       `elements_of` parse and reach `BuiltinDeclaration` here but have no reader
       until items 11 and 12, so `array_set` and `array_fill` keep exactly the
       fact the clause they carried stated. Done when that clause is gone from
       both and nothing downstream has changed.
2. [ ] Snapshot the declarations of every body-less function at link, not only
       the ones `ms.is_core_builtin()` admits, so a CM import or a `.wasm` /
       `.wat` asset import can carry §4's attributes. The gate is that check,
       not the body test beside it, which already holds. Widening it puts
       user-supplied declarations under the assert that a builtin reading
       through a reference and returning storage declared `#[returns]` — written
       when only compiler-owned code reached it, so a missing declaration was a
       compiler bug. Done when that assert is a diagnostic against the
       declaration for everything outside `core:builtin`, and a panic for
       nothing.
3. [ ] Measure what a conservative reading of an indirect call would cost, at the
       seventeen exposed signatures Negative 3 names. Done when the number is known: it
       says how much item 4 is worth, and how much the gap below it leaves.
4. [ ] Give the functor type's row an inferred source, so an indirect call has
       one again (§4). The fixpoint gains a second map, from functor `TypeId` to
       facts, joined from every expression that mints a function value of that
       type — a `FuncRef` takes the named function's facts, a `Closure` its
       body's. A function value of type `T` is minted nowhere else, so the join
       bounds whatever reaches an `IndirectCall`, with no points-to analysis and
       no phase to move: `stores.rs` already walks both node kinds. Items 5 to 7
       landed first, so the row is empty and its three readers had to split:
       `stores.rs` and `last_use.rs` read an indirect call as retaining every
       argument, which costs a copy, and `mut_ref_writeback` as retaining none,
       because the conservative reading there is a hard error on a program no
       keyword can now make acceptable. Done when all three read the join, a
       `sort_by` comparator that retains neither argument stops pinning them,
       and the write-back asymmetry in the gaps below is closed with them.
5. [x] Delete `check_stores_semantic` and what only it reaches — the oracle, the
       return-provenance fixpoint, the escape walk, the type-reachability memo.
       Done when `effect_check.rs` reports effects and purity only, and the
       fixtures asserting a stores diagnostic are gone with it. This closes
       issues #2049 and #2050.
6. [x] Strip the declarations from the corpus: 137 under `wado-compiler/lib`,
       3024 under `package-gale` — 2957 of them Kiln output, so Gale's generator
       stops emitting them first — and 2 under `package-marl`. Done when no
       function declaration in the corpus carries a `stores` clause and
       `mise run test-wado` passes. The eleven trait requirements §4 names lose
       theirs here rather than converting it.
7. [x] Remove the `stores` clause from the grammar, in both declaration and
       function type position, after nothing writes one. Done when the parser
       rejects both.
8. [ ] Take retention out of the type, which §4 says it is no part of: delete
       `mangle_stores_member` from the function type name, the subset check in
       `typecheck::check_at`, and `ResolvedType::Function::stores`, which item 4
       replaces. Nothing writes the row since item 7, so the member is already
       absent from every mangled name and the subset check already vacuous; what
       is left is the field and its two remaining readers, `mangle_stores_member`
       and `typecheck::check_at`. Done when two function types differing only in
       retention are one type, and golden type names carry no stores member.
9. [ ] Seed `lower::plan::value_copy::stores` from the attribute alone. Its two
       halves parted with item 5: `declared_positions` reads `func.retains` and
       `direct` falls back to the linked `BuiltinDeclaration`, because
       monomorphization drops a generic body-less declaration before the
       fixpoint sees it — but the `hands_out_result` heuristic is still there.
       Done when it is gone, closing the "declared `stores` the walk does not
       confirm" gap in [Ownership Analysis](./wep-2026-05-21-resource-ownership.md).
10. [ ] Delete the `func.retains.is_empty()` gate in `niri::is_ctfe_eligible`.
        It refuses every retaining call, where the reason §5 gives reaches only
        the calls whose result could embed the retained reference — and
        `niri::frame` already tests exactly that, narrowly, before it runs one.
        The blanket gate is the older, coarser copy of the same idea. Done when
        the narrow test is the only place retention decides a fold, and a
        retaining call returning a scalar folds.
11. [ ] Add `into_param` as the walk's third channel (§3), fed by `into = q` on a
        declaration and by a body that puts a reference into one of its own
        parameters. Done when `StoresFacts` carries retained parameter to
        destination parameters, the fixpoint propagates it, and `List::push` —
        which has a body — reaches it without an attribute.
12. [ ] Say which place a retained parameter lands in, and whether what lands is
        the parameter or its elements (§4). `array_copy(dst, _, src, _, _)` is
        the case that needs both: `src`'s elements reach `dst` afterwards, so a
        destinationless retention marks the whole reference borrow-escaped at all
        twenty call sites. Fifteen are `Array<u8>`, where a scalar element
        escapes nothing; the other five are the backing-array swap in
        `List::grow` and its neighbours, which hand elements out of an array they
        then discard. Done when `array_copy` carries `elements_of` and `into`,
        the fifteen scalar sites stop paying for it, and a caller reasons about
        `dst`'s extent instead of assuming the worst.
13. [ ] Surface every attribute a declaration carries in `wado query hover` and
        `wado doc`, with no per-attribute allowlist — `#[retain]` and
        `#[returns]` reach a reader because attributes do, not because these two
        were singled out. Done when a body-less declaration's attributes appear
        in both, and adding an attribute needs no change to either.
14. [ ] Record the effect on `benchmark/` and `wasm-size/`. Done when both
        READMEs carry the new numbers.

## Known gaps

- [ ] Read an indirect call per call site rather than per functor type. Roadmap
      item 4 joins every function value of one type into one answer, so a
      comparator that retains an argument coarsens every other call through the
      same type. Closing it takes knowing which function values reach which call
      — a points-to analysis, which nothing here is;
      `lower::plan::value_copy::funcset` is a borrow-keyed container, not that.

- [ ] Read an indirect call the same way everywhere. `stores.rs` and
      `last_use.rs` assume it retains every argument; `mut_ref_writeback`
      assumes it retains none, so a write-back through a functor is emitted on a
      body that may keep the reference. The frontend used to refuse that
      program, and the refusal went with item 5; the reading that would replace
      it rejects programs no keyword can now make acceptable, which is worse —
      and unlike the other two it costs a refusal, not a copy. Roadmap item 4
      closes it by giving all three the same join.

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
