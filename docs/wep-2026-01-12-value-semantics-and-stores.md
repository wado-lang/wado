# WEP: Value Semantics and Reference Retention

## Context

Wado targets Wasm GC, where a struct or an array is a reference type. A language
built on that has to answer two questions the runtime does not answer for it:
what an assignment between struct-typed variables means, and what happens when a
reference is handed to a call that may keep it.

```wado
fn caller() {
    let local = Data {};
    store(&local);  // will `store` keep this reference?
}
```

In a language with a stack the second question decides where `local` lives, and
a wrong answer is a dangling reference. Wado's answer makes it a question about
what the compiler may skip instead, which is why it reaches the optimizer and
never the programmer.

## Decision

### 1. Structs Have Value Semantics

A struct is copied on assignment, on parameter passing, and on return.

```wado
let a = Point { x: 1, y: 2 };
let b = a;       // a copy
b.x = 10;        // `a` is unchanged
let c = move a;  // ownership transferred, `a` invalidated
```

Value semantics are the model with no aliasing surprises to reason about, and
`move` is the escape hatch where a copy is too expensive to take.

### 2. There Is No Stack, So Nothing Is Promoted

A programmer never chooses between stack and heap, and never annotates a value
so that it may outlive a scope. Neither the choice nor the annotation exists.

Under [GC in Components](./wep-2026-03-28-gc-in-components.md) every struct,
array and string a program builds is a GC allocation from the moment it is
built, and a local holds a reference to it rather than the value. A reference
that outlives its local's scope therefore already points at a live object, and
the collector keeps it. There is no second home to move it to.

The compiler's freedom runs the other way: a value nothing escapes with may be
taken _out_ of the heap and held as plain Wasm locals, or moved rather than
copied where it is used last. An escape withdraws one of those. Both are
optimizations, so a program means the same thing with either turned off.

This is what makes retention safe to be imprecise about. An escape analysis that
decides where a value lives must be right or the program is wrong; one that
decides only what the optimizer may skip costs a copy when it is wrong.

### 3. Retention Is Inferred, Never Declared

A reference parameter leaves a call in two ways, and they are different claims:

- Borrow-out — the result aliases the parameter's storage, as `StrSlice::sub`
  and `List::as_slice` do. The caller already owns the referent.
- Retain — the reference reaches a global, or is written through a `&mut` the
  caller still holds, as `array_set` does.

Neither is a safety condition, for §2's reason: there is nothing for a
declaration to prevent. Both are optimizer inputs, and §5 says what they buy.

A body states both, so the compiler reads them from it rather than from a
declaration: an interprocedural least fixpoint over the call graph, publishing
each function's facts to its callers. Wado has no separate compilation — a
published package ships its sources
([Provider Metadata](./wep-2026-07-26-provider-metadata.md)) — so the fixpoint
always has every body it needs. A caller that hands its own reference parameter
to a retaining callee retains it in turn, and the fixpoint carries that along
the graph without either function saying so.

A fact is one of three, by where the reference lands:

| Channel       | Where it lands                     | Example          |
| ------------- | ---------------------------------- | ---------------- |
| `escapes`     | Somewhere the caller cannot see    | a global         |
| `into_result` | The return value — borrow-out      | `List::as_slice` |
| `into_param`  | A named parameter the caller holds | `List::push`     |

Retain is two channels rather than one because a destination the caller can name
bounds the retention: a reference put into a parameter the caller owns lives as
long as that parameter, while one that reaches a global is bounded by nothing.
`into_param` is designed and not yet built, so a reference landing in a
parameter is read today as reaching somewhere unseen.

Keeping the channels apart is what makes each precise. An iterator holds a
reference to what it walks, so folding `into_result` into `escapes` would make
every `&List` parameter retained the moment a body iterates it, while a
`collect()` that drops the iterator keeps nothing.

A function with a body therefore declares nothing. There is no retention row on
a function declaration, in a function type, or on a closure, and no obligation
for a programmer to discharge.

### 4. A Declaration Only Where There Is No Body

A body-less declaration is the exception: there is nothing to read, so it states
its facts itself, as attributes.

```wado
#[returns(part_of = arr)]
pub fn array_get_ref<T>(arr: &Array<T>, idx: i32) -> &T;

#[retain(value, into = arr)]
pub fn array_set<T>(arr: &mut Array<T>, idx: i32, value: T);
```

`#[returns(owned)]` and `#[returns(part_of = p)]` state borrow-out.
`#[retain(...)]` states retain, naming one retained thing per attribute and
repeating where there is more than one, so each carries its own destination
without the attribute grammar growing a way to group them:

```wado
#[retain(value, into = arr)]               // `value` itself, landing in `arr`
#[retain(elements_of = src, into = dst)]   // `src`'s elements, landing in `dst`
#[retain(data)]                            // `data` itself, destination unknown
```

A bare name is the parameter as a whole, and `elements_of = p` is that
parameter's elements — the difference a copy between arrays needs, where what
reaches the destination is what the source holds rather than the source. `into =
q` names where it lands; without it the destination is unknown, which is the
conservative reading. Every form names a parameter rather than a position,
reusing the shape `part_of` already has, and an argument naming no parameter is
reported. `elements_of` and `into` are accepted and carried but not yet read, so
a declaration writing them gets the destinationless reading either way.

Silence is the safe reading for `#[retain]` and not for `#[returns]`: a missing
`#[returns]` is taken for "allocates", which elides copies and is wrong for a
declaration that does hand out an argument's storage. Since only
compiler-owned declarations carry these today, a missing one there is the
compiler's own bug and is asserted rather than diagnosed.

Where each is accepted:

| Declaration                                        | `#[retain]` / `#[returns]`   |
| -------------------------------------------------- | ---------------------------- |
| `core:builtin`, body-less                          | Yes                          |
| CM component import, WASI, `.wasm` / `.wat` import | Parsed, not yet read         |
| `trait` / `interface` method requirement           | Error — the impl's body does |
| Anything with a body                               | Error — the body states it   |

A trait method requirement has no body of its own, but every call to it is
statically dispatched to an impl that has one, and monomorphization resolves
that before the fixpoint runs. An attribute there would never be read, and one
contradicting the impl would never be caught.

A program has no body-less function of its own to put these on — the one it can
write is a [declared absence](./wep-2026-09-13-declared-absence.md), which
reserves a name and is never called — so in practice the attributes belong to
the compiler's own declarations and to imports. That is the sense in which §3
removes retention from user code entirely rather than making it optional.

They are attributes rather than a `with` row because retention is not part of a
function's type. Two declarations differing only in what they retain are one
type, and a call resolves against the declaration it names.

### 5. What Retention Buys

Nothing is promoted to reach a retained reference (§2), so what the facts buy is
what the compiler may stop doing to the argument:

- A local passed where the callee retains it or hands it out cannot be moved out
  of, so it is copied; one passed where the callee keeps nothing can be moved.
- A `&mut` argument is not written back at a call that retains it: the borrow
  outlives the call, so the call is no place to write it.
- A constant is not forwarded into a retained parameter.
- A call whose result could embed a retained reference is not folded at compile
  time, since compile-time evaluation has no reference values to embed and the
  result would be a snapshot the next write leaves stale.

### 6. Closures Capture by Reference

A closure auto-captures each free variable by reference, and the reference kind
is inferred from body usage: `&T` where the body only reads, `&mut T` where it
writes. The closure's type follows — `fn` when every capture is read-only, `fn
mut` when any is mutating. See
[Closure Implementation](./wep-2026-01-16-closure-implementation.md).

```wado
let mut count = 0;
let mut inc = || count += 1;   // captures &mut count; fn mut() -> ()
let get = || count;            // captures &count; fn() -> i32
inc();
inc();
assert get() == 2;             // both closures see the same location
```

Two closures naming the same outer binding share its location, because
references alias and nothing here makes a second copy. The closure value itself
follows §1 and is copied when assigned or passed; since its environment holds
references, every copy observes the same bindings.

A closure that escapes its declaring scope needs no lifetime rule of its own.
Its captured referents are GC allocations the environment holds references to
(§2), so the collector keeps them; the escape changes only what the optimizer
may do to the bindings.

A closure declares no more about what it retains than a named function does, and
no effects either — those are inferred from the body. A `with` row written where
one would go is read and reported as not yet carried.

The term for what a closure does to an outer binding is capture, and retention
is about the reference parameters a call is handed. They are separate
mechanisms.

### 7. The Functor Type Carries No Retention

Retention is a fact about a function, not about its type, so a function type
says nothing about it and an indirect call has nothing to read. The copy
analysis therefore assumes an indirect call retains every reference argument.

```wado
fn apply<T, R>(f: fn(T) -> R, x: T) -> R {
    return f(x);
}
```

`apply` keeps nothing of its own, but what `f` does with `x` is not in `f`'s
type, so the call through it is read as an escape.

Recovering precision here needs a source for the fact, not a new analysis. A
function value of a given type is minted in exactly two places — a reference to
a named function, and a closure literal — so joining the facts of every such
expression of one type bounds every call through a value of that type, with no
points-to analysis. Sourcing it that way keeps retention a derived fact about a
type rather than part of its identity: two function types differing only in
retention stay one type, and nothing is checked when one coerces to the other.

### 8. Component Model Boundaries

Retention is a within-component concern. A call crossing a Component Model
boundary copies — `struct` to `record`, `List<T>` to `list<T>`, `String` to
`string`, with resources passed as explicit `borrow<T>` / `own<T>` — so it
carries none of the caller's storage across. Whatever the other component keeps,
it keeps its own copy.

A CM import is still a body-less declaration, so §4's attributes belong on one;
nothing needs them today, because the copy already answers. They are there for
an import whose lowering does not copy.

### 9. Retention Is Not an Effect

An effect is what a function does, retention is what it keeps, and a
capability-based reading makes the two look close: a retained reference can be
mutated later, so tracking retention resembles tracking a potential effect.

They stay apart because an effect is authority a caller grants and a handler can
intercept, which is why it belongs to the signature and the caller must see it.
Retention grants nothing and intercepts nothing; it only tells the compiler what
it may stop doing to an argument. So an effect is declared in the `with` row and
is part of the function's type, while retention is read from the body — or,
where there is none, stated as an attribute the type does not carry.

### 10. Reference, Not Pointer

`&T` is a reference and Wado does not call it a pointer: it is GC-managed and
non-null, with no arithmetic on it, and `Option<&T>` is how a nullable one is
written. Wasm GC uses the same word for the same thing.

## Roadmap

1. Read the attributes of every body-less declaration, not only the compiler's
   own, so that a CM import or a `.wasm` / `.wat` asset import can carry them.
   The obstacle is §4's asymmetry: a missing `#[returns]` is asserted rather
   than diagnosed, which is right while only compiler-owned code reaches it and
   wrong for a user-supplied declaration. Done when that assert is a diagnostic
   against the declaration everywhere outside the compiler's own library, and a
   panic nowhere.

2. Measure what the conservative reading of an indirect call costs, across the
   signatures in the corpus that take a functor with a reference parameter.
   Done when the number is known, since it says how much item 3 is worth and how
   much the gap below it would leave.

3. Give the functor type's row the inferred source §7 describes, joining the
   facts of every function reference and closure literal of one type. Done when
   every reader of an indirect call reads that join, a comparator that retains
   neither argument stops pinning them, and the disagreement in the gaps below
   is closed with it.

4. Add `into_param` as the third channel (§3), fed both by `into = q` on a
   declaration and by a body that puts a reference into one of its own
   parameters. Recording it is the smaller half: what a caller does with a
   bounded retention is a reading the published facts have no shape for today,
   since they name retained positions and not where each landed. Done when the
   facts carry retained parameter to destination parameter, the fixpoint
   propagates it, `List::push` reaches it from its body with no attribute, and a
   caller reasons about the destination's extent instead of assuming the worst.

5. Read `elements_of` and `into`, so a declaration can say which place a
   retained parameter lands in and whether what lands is the parameter or its
   elements. A copy between arrays is the case that needs both: without them the
   whole source reference is marked escaped at every call site, including the
   many whose elements are scalars that escape nothing. Done when those sites
   stop paying for it.

6. Surface every attribute a declaration carries in `wado query hover` and
   `wado doc`, with no per-attribute allowlist — `#[retain]` and `#[returns]`
   should reach a reader because attributes do. Done when a body-less
   declaration's attributes appear in both, and adding an attribute needs no
   change to either.

7. Record the effect on `benchmark/` and `wasm-size/`. Done when both READMEs
   carry the new numbers.

## Known gaps

An indirect call is read per functor type rather than per call site. Roadmap
item 3 joins every function value of one type into a single answer, so one
comparator that retains an argument coarsens every other call through the same
type. Closing it takes knowing which function values reach which call, which is
a points-to analysis and nothing here is one.

An indirect call is not read the same way everywhere. The copy analysis assumes
it retains every argument; the write-back assumes it retains none, so a
write-back through a functor is emitted on a body that may keep the reference.
The frontend used to refuse such a program, and that refusal is gone; the
conservative reading that would replace it rejects programs nothing in the
language can now make acceptable, which is worse, and unlike the other readers
it costs a refusal rather than a copy. Roadmap item 3 closes it by giving every
reader the same join.

## References

- [Rust Ownership](https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html)
- [Go Escape Analysis](https://go.dev/doc/faq#stack_or_heap)
- [Swift Value Semantics](https://developer.apple.com/swift/blog/?id=10)
