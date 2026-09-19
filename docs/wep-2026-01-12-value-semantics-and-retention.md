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

A write need not name a parameter to land in one. A reference-typed local whose
every assignment roots at one of this body's own parameters points nowhere else,
so a write through it lands inside those parameters and is `into_param` at them
rather than an escape. The reading is a must-alias one: a single assignment from
anywhere the walk cannot name makes every write through that local an escape
again, and a local derived from such a one is no better off.

The fixpoint publishes where a bounded position landed and not just that it was
kept, so a reader outside it resolves the destination against its own argument
list the way the fixpoint does, by name or through a function value alike.

A position any channel keeps out of sight carries no destination, and is read as
the union of all three. One whose every channel names a parameter is read as the
locals the call's arguments name there, and pins its referent only where one of
them is still readable at a point the referent would be moved out of. A
destination that is itself a parameter, or that a reference outlives, is
readable everywhere, so it pins for the whole frame as an unbounded one does.

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
#[result(part_of = arr)]
pub fn array_get_ref<T>(arr: &Array<T>, idx: i32) -> &T;

#[retain(value, into = arr)]
pub fn array_set<T>(arr: &mut Array<T>, idx: i32, value: T);
```

`#[result(owned)]` and `#[result(part_of = p)]` state borrow-out.
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

The two spellings differ at every call, and telling them apart takes two
carrier sets per value rather than one. A value is a reference into the
positions it was derived from, and separately it holds the positions whatever
sits inside it was derived from; a value can do both, so the sets are not
disjoint, and what the value carries altogether is their union. A reference
arrives pointing into its own position and holding nothing: what it holds is
what this body put there.

A bare source hands on the union, which the argument always carries.
`elements_of = p` hands on the held set alone. Copying between two of a caller's
own arrays therefore carries what was put in them and not the arrays' own
parameters — and that holds however the argument is spelled, whether as the
parameter, a projection of it, or a local bound from one. Where a join meets
both spellings of a position — a function value of one type minted from two
declarations — the reference is the wider claim and wins.

Silence is the safe reading for `#[retain]` and not for `#[result]`: a missing
`#[result]` is taken for "allocates", which elides copies and is wrong for a
declaration that does hand out an argument's storage. A declaration that owes
one is reported at the declaration, wherever it lives.

These two are not the whole family a body-less declaration carries.
`#[immediate(p)]` sits beside them and answers a different question — how
codegen lowers the call, not what the call keeps. See
[the spec](./spec.md) for it.

Where each is accepted:

| Declaration                                        | `#[retain]` / `#[result]`    |
| -------------------------------------------------- | ---------------------------- |
| `core:builtin`, body-less                          | Yes                          |
| CM component import, WASI, `.wasm` / `.wat` import | Yes                          |
| `trait` / `interface` method requirement           | Error — the impl's body does |
| Anything with a body                               | Error — the body states it   |

A Component Model import declares no `#[result]` and owes none: the boundary
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
  Where the retention names a destination the caller owns, the pin lasts as long
  as that destination is readable and no longer, so a move past its last read
  still stands.
- A `&mut` argument is not written back at a call that retains it: the borrow
  outlives the call, so the call is no place to write it. Where no place in the
  body can be written back to, the call is refused — which is why an indirect
  call must read the same answer as a direct one, or the same program is
  accepted through a function value and refused by name. This reads the union
  and not the destination, unlike the move above: a refusal is a correctness
  rule, and the reading that would sharpen it is a gap below.
- A constant is not forwarded into a retained parameter.
- A call whose result could embed a retained reference is not folded at compile
  time, since compile-time evaluation has no reference values to embed and the
  result would be a snapshot the next write leaves stale.

What precision buys on today's corpus is nothing. The per-site row, the bounded
destination, the element gate, a Component Model import's owned result, the
anchored-local reading and following a parameter by the type its body reads it
at, all together, leave every benchmark and every size program byte-identical.

The conservatism they remove is real, and measurably so: on the Gale corpus 785
generated `_parse_*` functions keep nothing, so `fn(&mut Parser)` reads empty
where it used to read as keeping everything, and resolved call sites go from 343
of 1356 to 715. What follows from it is nothing, because the locals it stops
pinning have later uses anyway, so no move and no scalarization comes of it. The
bounded destination has little to work with for a plainer reason: 19 functions
publish one at all, and every one is a container insert — `List::push`'s `value`
into `self`, `array_copy`'s `src` into `dst`, `index_assign`'s `value` into
`self` — where the destination is read after the argument would be moved anyway.

What the facts do buy is correctness, and that is not free. Declaring what
`array_copy` and `array_clone` hand on through their elements costs 478 bytes,
in `gale_gen` (+339) and `json_catalog_v2` (+139) and nowhere else: those calls
now pin what they really reach. Retention is carried for correctness, and a
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
type. The answer comes from a source rather than a declaration: a function value
is minted in two places — a reference to a named function, and a closure literal
— and no function type crosses a Component Model boundary, so every value a call
can reach was minted somewhere in the package. Lifting rewrites the second of
those into an object the walk does not read as a mint, which is a gap below and
not a third place.

Which of them reaches a given call is read by following each minted value from
where it is minted to where it is called. A value settles in a local, is handed
to a parameter, or goes somewhere this walk does not follow — a field, a
capture, a reference taken of the local holding it. The first two are followed:
a local holds whatever was assigned to it anywhere in the body, and a parameter
holds whatever every call site passed there, which makes both a whole-program
join rather than a reading per program point. The third is not, so a function
type whose values reach one such place is read as every value of that type — the
join over every expression minting one, which is what the type alone can say. A
parameter of an exported function is filled from outside the package and takes
that same reading.

A call therefore reads the values that reach it, and a closure retaining an
argument no longer coarsens every other call through its type. A functor type
nothing mints a value at is read conservatively, which keeps "nothing mints
this" apart from "the fixpoint has not reached it yet".

What a site reads is a reading like any other, destinations included: a position
stays bounded there only where every value reaching it says where it landed, and
the caller resolves that against its own arguments as it would for a name. One
value that keeps the position out of sight makes it unbounded for the site, and
a reading that does not keep the position says nothing about it — which is what
lets a fixpoint that starts from keeping nothing arrive at a bound.

Sourcing it that way keeps retention a derived fact about a type rather than
part of its identity: two function types differing only in retention stay one
type, and nothing is checked when one coerces to the other. It is also what lets
every reader of an indirect call read the same answer — the copy analysis, the
last-use walk and the write-back — rather than each taking a reading of its own.
A reader outside the fixpoint names a call by its callee expression, and one the
fixpoint published no answer for falls back to the type.

The answer is computed twice, since those readers sit on either side of boxing,
and each is read over the same tree its reader walks. That is what makes a type
no expression mints mean "nothing mints this" rather than "not minted yet": a
function value has to be minted in a tree to reach a call site in that tree, so
a later phase minting one cannot widen an answer an earlier reader already gave.
Closure lifting does mint — a `with … do` body inside a closure becomes a
handler thunk — and those are types no earlier call site could have named.

Lifting also specializes: a function taking a function value gets a copy per
closure reaching it, and the copy declares that parameter at the one closure's
own type while its body still calls it at the function type. A parameter is
therefore followed by the type its body reads it at, not by the type its
signature gives — otherwise the copy, which has exactly one value reaching its
parameter, would be the one place the answer went back to the type.

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

## Known gaps

Each says which reading the compiler does not have. The first two want the same
shape-level points-to answer, the next two a reading of this body finer than one
answer per local, and the last a change to the lattice the rest are read over.

A function value is followed only while it stays in a local or a parameter. One
put in a field, captured by a closure, or reached through a reference taken of
the local holding it goes where the walk does not, and every call that could
reach a value of that type is then read off the type. Closing that would take
following function values through the heap, which is a shape-level points-to
answer and nothing here is one.

An element claim reads what a value holds, which a body fills but a signature
never states. A parameter arrives holding whatever the caller put there, and
nothing names that, so the claim is answered from the writes this body made and
the positions those writes came from. A caller that hands on its own argument's
elements untouched is not telling its own caller so, which is what makes the
claim useful — it stops at the frame that filled the container — and also what
bounds it. Closing that would take a summary of what a parameter holds on entry,
which is the same points-to answer.

The write-back reads the union where the move reads the destination. Refusing a
write-back is a correctness rule rather than a precision one — a `&mut` to a
detached place cannot be stored back where anything can still read the borrow —
and the destination would sharpen it as it sharpens a move. What it would take
is the reading the move has and this pass does not: whether the destination
leaves the frame. A parameter does, a local assigned from one does, and a local
this body built does not until something carries it out — which is a per-local
escape answer over this body, published where the pass can read it. Sharpening
it on the last read alone is unsound, and was: a destination that is a parameter
is read by the caller after every point in this frame.

A retention into a reference-typed local is bounded only where every assignment
to that local roots at one of this body's own parameters. That is a must-alias
reading, so one assignment from anywhere else — a call result, a global, another
local with such an assignment — makes every write through the local an escape,
however narrow the other assignments are. Closing it takes a per-program-point
reading rather than one answer per local.

A lifted closure is not read as the value it came from. Lifting rewrites the
closure literal at a call site into an object holding the lifted function, while
the call receiving it still reads its parameter at the function type — so the
argument arrives spelled at a type nothing mints, and the parameter falls back
to any value of its own type. Where one function type has several closures, that
takes each specialization back to their join, which is the coarsest reading of
the one place exactly one value arrives.

Closing it takes reading such an object as a mint of the function it holds,
which the object does say — and at a shift, which is what makes it work. The
function it holds is the lifted body, whose parameters are the closure's
preceded by its environment, so the call's position `p` is that function's
`p + 1`. A mint would therefore have to carry where its positions start, and
every reader of one — the join, the row a call resolves to, the destination a
caller resolves against its own arguments — apply it. That is a change to the
lattice rather than to one arm of the walk, and the size of it is the reason
this is open rather than done. Reading the lifting phase's own record of which
closure it specialized each copy for would need no shift, but the answer would
then come from a previous phase instead of the tree the reader walks, which is
what the decision above rests on.

## References

- [Rust Ownership](https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html)
- [Go Escape Analysis](https://go.dev/doc/faq#stack_or_heap)
- [Swift Value Semantics](https://developer.apple.com/swift/blog/?id=10)
