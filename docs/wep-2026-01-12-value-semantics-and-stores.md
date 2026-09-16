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

### Structs Have Value Semantics

A struct is copied on assignment, on parameter passing, and on return.

```wado
let a = Point { x: 1, y: 2 };
let b = a;       // a copy
b.x = 10;        // `a` is unchanged
let c = move a;  // ownership transferred, `a` invalidated
```

Value semantics are the model with no aliasing surprises to reason about, and
`move` is the escape hatch where a copy is too expensive to take.

### There Is No Stack, So Nothing Is Promoted

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

### Retention Is Inferred, Never Declared

A reference parameter leaves a call in two ways, and they are different claims:

- Borrow-out — the result aliases the parameter's storage, as `StrSlice::sub`
  and `List::as_slice` do. The caller already owns the referent.
- Retain — the reference reaches a global, or is written through a `&mut` the
  caller still holds, as `array_set` does.

Neither is a safety condition, for the reason
[There Is No Stack, So Nothing Is Promoted](#there-is-no-stack-so-nothing-is-promoted)
gives: there is nothing for a declaration to prevent. Both are optimizer inputs,
and [What Retention Buys](#what-retention-buys) says what they buy.

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
A caller resolves `into_param` against its own argument at that position —
into a local of its own the reference is carried, not escaped; through one of
its own reference parameters it is `into_param` again, one level up. So a
retention that ends in a local stops there instead of propagating out of every
frame it passes through.

What a caller cannot resolve, it must assume: the published facts name retained
positions and not where each landed, so a reader outside the fixpoint — the
last-use walk, the write-back — takes the union of all three channels.

Keeping the channels apart is what makes each precise. An iterator holds a
reference to what it walks, so folding `into_result` into `escapes` would make
every `&List` parameter retained the moment a body iterates it, while a
`collect()` that drops the iterator keeps nothing.

A function with a body therefore declares nothing. There is no retention row on
a function declaration, in a function type, or on a closure, and no obligation
for a programmer to discharge.

### A Declaration Only Where There Is No Body

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
q` names where it lands; without it the destination is unknown, so the reference
takes both unbounded channels, the result being one of the places it could be.
Every form names a parameter rather than a position, reusing the shape `part_of`
already has, and an argument naming no parameter is reported.

The two spellings differ at every call. A bare source hands on the reference
itself, which the argument always carries. `elements_of = p` hands on what the
referent holds instead: nothing where the element type cannot hold a reference,
and never the parameter the argument is rooted at, which is a carrier by being
the reference to the container rather than by anything the container holds.
Copying between two of a caller's own arrays therefore carries what was put in
them and not the arrays' own parameters. Where a join meets both spellings of a
position — a function value of one type minted from two declarations — the
reference is the wider claim and wins.

Silence is the safe reading for `#[retain]` and not for `#[returns]`: a missing
`#[returns]` is taken for "allocates", which elides copies and is wrong for a
declaration that does hand out an argument's storage. A declaration that owes
one is reported at the declaration, wherever it lives.

Where each is accepted:

| Declaration                                        | `#[retain]` / `#[returns]`   |
| -------------------------------------------------- | ---------------------------- |
| `core:builtin`, body-less                          | Yes                          |
| CM component import, WASI, `.wasm` / `.wat` import | Yes                          |
| `trait` / `interface` method requirement           | Error — the impl's body does |
| Anything with a body                               | Error — the body states it   |

A Component Model import declares no `#[returns]` and owes none: the boundary
copies ([Component Model Boundaries](#component-model-boundaries)), so its
result is owned by construction. The answer is the same for every one of them,
so it is read off the declaration rather than written on each.

A trait method requirement has no body of its own, but every call to it is
statically dispatched to an impl that has one, and monomorphization resolves
that before the fixpoint runs. An attribute there would never be read, and one
contradicting the impl would never be caught.

A program has no body-less function of its own to put these on — the one it can
write is a [declared absence](./wep-2026-09-13-declared-absence.md), which
reserves a name and is never called — so in practice the attributes belong to
the compiler's own declarations and to imports. That is the sense in which
inference removes retention from user code entirely rather than making it
optional.

They are attributes rather than a `with` row because retention is not part of a
function's type. Two declarations differing only in what they retain are one
type, and a call resolves against the declaration it names.

### What Retention Buys

Nothing is promoted to reach a retained reference, so what the facts buy is what
the compiler may stop doing to the argument:

- A local passed where the callee retains it or hands it out cannot be moved out
  of, so it is copied; one passed where the callee keeps nothing can be moved.
- A `&mut` argument is not written back at a call that retains it: the borrow
  outlives the call, so the call is no place to write it. Where no place in the
  body can be written back to, the call is refused — which is why an indirect
  call must read the same answer as a direct one, or the same program is
  accepted through a function value and refused by name.
- A constant is not forwarded into a retained parameter.
- A call whose result could embed a retained reference is not folded at compile
  time, since compile-time evaluation has no reference values to embed and the
  result would be a snapshot the next write leaves stale.

What it buys on today's corpus is nothing. Taking the row, the bounded
destination, the element gate and a Component Model import's owned result all
together leaves every benchmark and every size program byte-identical. The
conservatism they remove is real — 785 generated `_parse_*` functions keep
nothing, so `fn(&mut Parser)` reads empty where it used to read as keeping
everything — but the locals it stops pinning have later uses anyway, so no move
and no scalarization follows. Retention is carried for correctness, and a
precision claim about it is worth only what a measurement says it is.

### Closures Capture by Reference

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
has value semantics like any other and is copied when assigned or passed; since
its environment holds references, every copy observes the same bindings.

A closure that escapes its declaring scope needs no lifetime rule of its own.
Its captured referents are GC allocations the environment holds references to,
so the collector keeps them; the escape changes only what the optimizer may do
to the bindings.

A closure declares no more about what it retains than a named function does, and
no effects either — those are inferred from the body. A `with` row written where
one would go is read and reported as not yet carried.

The term for what a closure does to an outer binding is capture, and retention
is about the reference parameters a call is handed. They are separate
mechanisms.

### The Functor Type Carries No Retention

Retention is a fact about a function, not about its type, so a function type
says nothing about it and an indirect call has no row on the type to read.

```wado
fn apply<T, R>(f: fn(T) -> R, x: T) -> R {
    return f(x);
}
```

`apply` keeps nothing of its own, and what `f` does with `x` is not in `f`'s
type. The answer comes from a source rather than a new analysis: a function
value of a given type is minted in exactly two places — a reference to a named
function, and a closure literal — and no function type crosses a Component Model
boundary, so joining the facts of every such expression of one type bounds every
call through a value of that type, with no points-to analysis. A functor type
nothing mints a value at is read conservatively, which keeps "nothing mints
this" apart from "the fixpoint has not reached it yet".

Sourcing it that way keeps retention a derived fact about a type rather than
part of its identity: two function types differing only in retention stay one
type, and nothing is checked when one coerces to the other. It is also what lets
every reader of an indirect call read the same answer — the copy analysis, the
last-use walk and the write-back — rather than each taking a reading of its own.

The row is computed twice, since those readers sit on either side of boxing, and
each sees only the tree of its own phase. A pass minting a function value
between the two would leave the earlier reader's answer too narrow, so the count
of minting expressions is carried forward and asserted not to grow. Closure
lifting is the one pass there, and it mints nothing: it rewrites a reference to
a named function into a zero-capture closure forwarding to that same function.
The count is what the assertion compares, because boxing rewrites the types the
row is keyed on.

### Component Model Boundaries

Retention is a within-component concern. A call crossing a Component Model
boundary copies — `struct` to `record`, `List<T>` to `list<T>`, `String` to
`string`, with resources passed as explicit `borrow<T>` / `own<T>` — so it
carries none of the caller's storage across. Whatever the other component keeps,
it keeps its own copy.

A CM import is still a body-less declaration, so the attributes above belong on
one; nothing needs them today, because the copy already answers. They are there
for an import whose lowering does not copy.

### Retention Is Not an Effect

An effect is what a function does, retention is what it keeps, and a
capability-based reading makes the two look close: a retained reference can be
mutated later, so tracking retention resembles tracking a potential effect.

They stay apart because an effect is authority a caller grants and a handler can
intercept, which is why it belongs to the signature and the caller must see it.
Retention grants nothing and intercepts nothing; it only tells the compiler what
it may stop doing to an argument. So an effect is declared in the `with` row and
is part of the function's type, while retention is read from the body — or,
where there is none, stated as an attribute the type does not carry.

### Reference, Not Pointer

`&T` is a reference and Wado does not call it a pointer: it is GC-managed and
non-null, with no arithmetic on it, and `Option<&T>` is how a nullable one is
written. Wasm GC uses the same word for the same thing.

## Roadmap

Named rather than numbered, so a reference to one from the code survives the
list changing around it.

### A bounded destination the caller can read

The fixpoint resolves `into_param` against its own arguments, so a retention
that ends in a local stops there. Outside the fixpoint it does not: the published
facts name retained positions and not where each landed, so the last-use walk and
the write-back take the union and assume the worst even with the argument list in
hand.

Done when a reader outside the fixpoint can ask where a position landed, an
argument retained into a local the caller owns is pinned only for that local's
extent rather than for the frame, and a fixture shows the difference.

What it has to work with is small: 19 functions across the Gale corpus publish a
bounded retention at all, and every one is a container insert — `List::push`'s
`value` into `self`, `array_copy`'s `src` into `dst`, `index_assign`'s `value`
into `self` — where the destination outlives the argument anyway. Pinning to the
destination's extent and pinning to the frame are the same answer there.

## Known gaps

An indirect call is read per functor type rather than per call site. The row
joins every function value of one type into a single answer, so one comparator
that retains an argument coarsens every other call through the same type.
Closing it takes knowing which function values reach which call, which is a
points-to analysis and nothing here is one.

An element claim on a local hands on everything that local carries. There is one
carrier set per value, so "what was written into this array" and "what a
reference to it was derived from" are the same set: an `elements_of` source
rooted at a local that carries a reference for some other reason hands that on
too. Only a parameter root is told apart, that being the one case where the two
readings provably differ. Closing it takes separating a reference from what it
points at in the carrier model.

A retention into a reference-typed local reads as an escape. The bounded channel
resolves a destination rooted at one of this body's own parameters; a local
holding a reference derived from a parameter is not one, and the walk cannot say
it holds only that, since a carrier set records what a local may hold and not
what it must. Closing it takes a must-alias reading of such a local.

## References

- [Rust Ownership](https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html)
- [Go Escape Analysis](https://go.dev/doc/faq#stack_or_heap)
- [Swift Value Semantics](https://developer.apple.com/swift/blog/?id=10)
