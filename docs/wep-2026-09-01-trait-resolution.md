# Trait Resolution — One Order, Written Down

## Context

`docs/spec.md` says coherence guarantees that "for every `(Trait, Type)` pair,
there is at most one `impl Trait for Type` that can apply."

That is false, and the orphan rules do not make it true. Both impls below are
legal, local, and apply to `Point`:

```wado
impl<T: Limit> Describe for T { … }
impl<T: ReflectStruct> Describe for T { … }

struct Point { x: i32 }
impl Limit for Point { … }          // Point satisfies both bounds
```

Several impls applying is not a defect. A trait is meant to carry several value
blankets: `Inspect`, `Serialize` and `Deserialize` each derive over the four
reflection kinds. What the language needs is a _selection order_, and until this
WEP it existed only in `select_trait_match`'s `sort_by`. Its rules were spread
across four WEPs, with one recorded in none of them:

| Rule                                        | Recorded in    |
| ------------------------------------------- | -------------- |
| Non-variadic beats variadic                 | WEP 2026-03-14 |
| Variadic overlap is a definition-time error | WEP 2026-03-14 |
| A newtype's own impls select for it first   | WEP 2026-06-25 |
| Two traits, one method name, is ambiguous   | WEP 2026-07-31 |
| A `Reflect*` bound needs visible members    | WEP 2026-06-13 |
| Locality — a local impl beats a foreign one | nowhere        |

This WEP drops locality from the order. The sort still has it: see Known gaps.

Issue #1932 was a missing rank, found by reading the sort because no document
stated the order.

## Decision

One order, stated here. Every other WEP that owns a rule keeps it and links here
for its place in the sequence. The implementation cites this document instead of
being the only place the order is written.

### Where the order applies

The order governs the trait-impl step of a _method call_ `recv.m(args)`. Method
lookup reaches it after two steps that are not part of it, and three other
dispatch paths select without it:

| Step or path                                  | What selects                                                                |
| --------------------------------------------- | --------------------------------------------------------------------------- |
| Inherent method (`impl Type { fn m }`)        | Shadows every trait method of that name, along the whole newtype chain      |
| A reference receiver's `&T` impls             | Ranked ahead of the base type's, concrete before blanket                    |
| Trait impls on the receiver                   | The order below                                                             |
| A type parameter's bounds (`T: Tr` in a body) | The first bound declaring the method; two or more is an ambiguity error     |
| `Type::m(args)` and other associated calls    | Current-module-first scan, first hit; value blankets on a separate fallback |
| Operators and indexing                        | The operand's type, unique-or-error (WEP 2026-07-31)                        |

These paths do not yet follow this order: see Known gaps.

### The candidates

A call's candidates come from three places.

First, the impls whose target matches the receiver anywhere along its newtype
chain: an impl written for the receiver's own type, for one instantiation of it
(`impl Tag for Box_<i32>`), or for its head (`impl<T> Tag for Box_<T>`).

Second, every _value blanket_ whose receiver-parameter bounds the receiver
satisfies. A value blanket is `impl<T: Bound> Tr for T`, and its receiver
parameter carries at least one bound: an unbounded `impl<T> Tr for T` names no
condition that could select it, so it is rejected where it is written rather
than accepted and never reached.

Third, every _reference blanket_ (`impl<T: Bound> Tr for &T`) whose bound a
reference receiver's referent satisfies. It ranks below a concrete `&T` impl
and above any impl on the base type.

All three lists are then gated on scope, below.

The compiler accepts a method an impl block declares beyond its trait's, and
`core:cbor` calls one on `self`; whether the language keeps that is undecided
(wado-lang/wado#1959). Selection reads the block's own names beside the
trait's, so such a method is a candidate through that impl alone
(`trait_impl_only_method.wado`).

`()` is the unit type, not the empty tuple `[]`, so an impl for `[..T]` is
never a candidate for it. Its traits are implemented for `()` directly
(`trait_unit_eq_ord.wado`).

An anonymous struct is a shape no impl can name, so only a value blanket over
its `Reflect*` facts reaches it, and every shape reads the same to the order
(`reflect_anon_struct.wado`).

Two impls of one `(Trait, Type)` pair are rejected where the second is written,
in one module or in two modules of one package. No rank distinguishes them, and
a package compiles whole, so the check needs no open-world reasoning.

### Scope

A trait's methods are candidates at a call site only where that trait's
_declaration_ is in scope. Where the impl was written does not matter; impls
stay globally visible, and one in a module the caller never named still answers
once the trait is imported.

A declaration is in scope when the calling module declares it, imports it by
name or by alias, imports it through a `pub use` re-export, or when it is one of
the prelude's. The prelude is the only exemption in the language: every other
top-level symbol a module names must be imported, and a trait is no different.
`Display`, `Eq`, `Inspect`, `IntoIterator` and the operator traits therefore
carry no import burden and nothing else is auto-used. Importing a _type_ brings
none of the traits its impls mention.

A bound is a name like any other, so `T: Sub` requires `Sub` imported and
reaches `Sub`'s own methods. A supertrait is a second name: a body calling
`Base`'s method through `T: Sub` imports `Base` as well, exactly as it would to
write `T: Base`. The bound states which contract `T` satisfies, not which names
the body may leave unwritten.

The rule exists so that a reader can resolve a call from the imports alone.
Without this gate a library's new blanket reaches every receiver in the program
and silently changes what downstream calls mean. Scope confines it to the
modules that imported the trait.

An unused-trait-import warning belongs to
[Unused Diagnostics](./wep-2026-05-16-unused-diagnostics.md) rather than here,
with one constraint this decision imposes on it: a trait imported only to enable
a dispatch never appears in an expression, so the check must count enabling a
dispatch as a use. A check that reads the source alone will tell the programmer
to delete the import that makes the module compile.

Lookup keeps searching outside the scope, for the diagnostic alone. When the
scoped candidates are empty and the unscoped ones are not, the call is an error
naming the trait that would have answered and the import that would enable it:

```text
no method 'shout' on 'String' in scope: 'Loud' declares it and is not
imported here; add `use { Loud } from "./lib_a.wado"`
```

### The order

Candidates are ranked.

#### Rank 0: variadic yields to non-variadic

Within one trait at one argument list, a variadic impl
(`impl<..T> Tr for [..T]`) is dropped when a non-variadic one is present. The
rule stays inside one trait, so a foreign blanket for trait `A` never displaces
a local variadic impl of trait `B` (WEP 2026-03-14 §5 Rule 1).

#### Rank 1: the newtype before its base

The search runs down the newtype chain and stops at the first level that
answers. A newtype is a type distinct from its base, so everything written for
it selects before anything written for the base (WEP 2026-06-25), and no later
rank can reach past a level that answered.

Depth is the level a candidate is selected _at_, so it covers both shapes: an
impl whose target is the newtype sits at 0 and one targeting the base at 1, and
a blanket sits at the level its bounds hold at.

A reference does not interrupt the chain. The newtype is always preferred, so a
call on `&W` visits `&W` and `W` before it reaches the base at all, and only
then `&Inner` and `Inner`. Within one level the reference precedes its pointee,
which is what ranks a `&T` impl ahead of the pointee's. `for let b of &bag` over
a newtype of `List<u8>` therefore takes the newtype's own impl where it has one
and the base's `impl<T> IntoIterator for &List<T>` where it does not
(`newtype_for_of_iteration.wado`).

A blanket's depth is measured over the whole derivation, not just its first
step. Take `impl<T: Base> Derived for T` answering `T: Derived`. That bound sits
at the depth the blanket's own bound holds at. A chained blanket therefore does
not report the base's bound as the newtype's.

The walk keeps the same subject throughout. If it reaches a `(type, trait)` pair
twice, that bound grounds nothing, so the walk answers no. Dispatch's query
answers the same repeat by the same rule, with one exception: a repeat with a
member descent between the two askings is a recursive type
(`struct Node { next: Option<Node> }`), the well-founded structural case, and
answers yes.

#### Rank 2: the least general impl

Within one level, the impl that names the most of the receiver answers. A target
is one of three, least general first:

| Generality | Target                       | Example                                               |
| ---------- | ---------------------------- | ----------------------------------------------------- |
| exact      | mentions no type parameter   | `impl Tr for Point`, `impl Tag for Box_<i32>`         |
| head       | mentions one, but is not one | `impl<T> Tag for Box_<T>`, `impl<T: Bound> Tr for &T` |
| any        | is a bare type parameter     | `impl<T: Bound> Tr for T`                             |

An impl written for the receiver defines the exact function the call names, so a
foreign `impl Tr for Point` beats a blanket written here. One written for the
receiver's head still names the receiver's own type constructor, where a value
blanket names only a condition the receiver happens to meet.

Both steps carry weight. Exact over head is `spec.md`'s "Specific Impls Win":
`impl Tag for Box_<i32>` beside `impl<T> Tag for Box_<T>` answers for `Box_<i32>`
and the head impl answers for the rest. Head over any is what the prelude turns
on: `RangeExclusive<T>` implements `Iterator`, so
`impl<I: Iterator> IntoIterator for I` applies to every range beside the
`impl<T: Step + Ord> IntoIterator for RangeExclusive<T>` written for it, and
without this step every `for x of 0..n` is the blanket ambiguity below.

Generality reads the target and nothing else. Which bounds an impl carries is
not part of it — see specificity, below.

#### Rank 3: anything left is ambiguous

Two shapes are reported, described below.

Every rank reads only how a candidate relates to the receiver. Where the impl
was written is not read at all: two candidates that tie at rank 2 are ambiguous
whether or not one of them is in the calling module. Ranks 1 and 2 exist so that
where the reader stands never decides what a program means. The escape is one
line: an impl written for the receiver, which names the body that runs for
every reader.

This rejects one pattern: overriding a library's blanket with a second blanket
of your own. Under this rule the two report for exactly the types that satisfy
both bounds; write `impl Tr for YourType` instead. That is the strict side taken
on purpose, and a concrete consumer of the pattern is reason to reopen it.

Specificity is not a rank either. When one blanket's bounds imply another's
(`impl<T: A + B>` beside `impl<T: A>`, or `impl<T: Ord>` beside `impl<T: Eq>`
under `Ord: Eq`), the pair is rank 3; the narrower one is not preferred. Which
associated-type bindings a caller sees when a narrower impl binds them
differently is the specialization soundness question, and answering it with a
rank would decide it by accident. The escape is the impl rank 2 puts above both.

Argument lists filter candidates before the order runs: one trait declaration at
several argument lists is an overload set, and the call's arguments choose
(WEP 2026-07-31). Distinct traits never form one.

Two same-named trait declarations from different modules are distinct traits, so
Scope above settles them: each module's `s.shout()` dispatches to the `Loud`
imported there (`cross_module_same_name_foreign_impl.wado`), and a module
importing both gets the two-trait ambiguity below.

### The two ambiguities

The two are separate diagnostics because the programmer fixes them differently.

#### Two traits, one method name

The receiver has `impl Alpha for Item` and `impl Beta for Item`, both declaring
`describe`. They share no contract, so nothing selects. The call names the
trait: `Alpha::describe(&it)` (WEP 2026-07-31).

Blankets join the collision like any other candidate. Two traits' blankets
sharing a method name and both applying is the same question with no impl to
name, and Scope is what keeps it rare: both traits have to be imported here for
both to compete.

#### Two blankets of one trait

The receiver satisfies both bounds and nothing above ranks them. A blanket has
no name, so the call _cannot_ pin one. The only answer is an impl written for
the receiver, which rank 2 puts above both:

```text
ambiguous blanket impls of 'Describe' for 'Point': 'T: Limit' and
'T: ReflectStruct' apply, and nothing ranks them;
write 'impl Describe for Point'
```

The compiler reports this at the use site, the one place the question can be
answered. Rejecting the overlap where the impls are written would mean deciding
whether two bounds can both hold. An open world cannot decide that, since
another module may write `impl Limit` for a struct at any time. So nothing is
rejected at definition time.

The report covers one trait declaration at one argument list. Two blankets of
_different_ traits are the two-trait ambiguity above.

### Eligibility gates

Ranking runs over candidates, and a bound that does not hold produces no
candidate. An impl's bound on its own parameter is read the same way wherever
the impl puts that parameter: in a type argument (`impl<T: B> Tr for List<T>`),
in a pointee (`impl<T: B> Tr for &T`), or in a pack's elements
(`impl<..T: B> Tr for [..T]`). A rigid type parameter satisfies such a bound
from the bounds in force on it and from nothing else. So `[..T]: Ord` does not
hold of `[A, B]` under `A: Inspect, B: Inspect`, and the body that wants it
says `A: Ord, B: Ord` (`trait_bound_on_rigid_param_is_checked.wado`,
`trait_error_bound_missing_on_rigid_param.wado`).

A marker on a generic declaration is the other question and keeps its own
answer: `impl<T> Eq for Pair<T>;` asks whether the _declaration_ derives, and
its body is emitted per instantiation, so a member that is one of the
declaration's own parameters is that instantiation's to satisfy
(WEP 2026-06-25, `eq_ord_explicit_request.wado`).

Two further gates decide candidacy. They are not ranking rules:

- A `Reflect*` bound holds only where every member of the receiver is visible at
  the use site (WEP 2026-06-13). This keeps `TreeMap` out of a downstream
  `T: ReflectStruct` without naming it.
- A newtype inherits its base's impls for dispatch. So a blanket keyed by a bound
  only the base carries is still a candidate for the newtype. Rank 1 places it at
  depth 1 rather than excluding it.

A blanket's receiver parameter is matched by position, not by spelling: a method
parameter named `T` inside the method is the method's own `T`.

### How the order is guaranteed

The order above is a specification. A `.wado` fixture pins what a program
prints, so it proves the whole pipeline agreed on an answer; it does not pin the
rule. A rank that never fires, a candidate list that quietly loses a shape, or a
cycle rule only one receiver kind exercises has no test that can name it. The
rules are therefore stated where they can be checked directly: as functions of a
self-contained value, under unit tests with no Wado source.

#### The solver's input

Selection reads `TypeId`s, `DefId`s, `ImplHeader` AST nodes and the annotate-time
`Scope`. Every one is an index into something only the pipeline builds, so a test
that wants to ask a question has to write Wado source and run the compiler to ask
it. The solver takes one self-contained value instead:

```text
Program {
  impls:  ImplId      -> { trait_, trait_args, target, params: [{ bounds, pins }],
                           origin: Written | Derived | Marker }
  traits: TraitDeclId -> { supertraits, holds_for_all, arg_defaults, on_ref,
                           methods: [MethodId], assoc_bounds: AssocId -> [TraitDeclId] }
  types:  TypeDeclId  -> { newtype_base }
  facts:  (TypeDeclId, TraitDeclId) -> { visible_from }
  assoc_bindings: (ImplId, AssocId) -> SolverType
  impl_methods:   ImplId -> [MethodId]
  scopes: ModuleId    -> { traits_in_scope }
  tuple:  Option<TypeDeclId>
}

SolverType = Decl(TypeDeclId, [SolverType]) | Param(u32) | Pack(u32)
           | Ref { mut, inner } | Tuple([SolverType])
           | Projection { base, trait_, assoc }
```

A declaration's kind and members are read by `derive` alone and arrive as a
`Declaration` beside the program rather than living in it. A `MethodId` is
interned by name across the program, so two traits declaring `describe` share
one and their collision is one question. Every id is a plain index, so a test
writes `TypeDeclId(0)` and the program around it, with no source and no
pipeline.

A query carries an environment beside the program: the bounds in force where the
question was asked. A generic body's `T: Tr` holds because its own signature says
so, not because any impl exists, so no query can answer from `Program` alone. A
pack's bound holds of each element, so a variadic body's `..T: Tr` answers for
the pack and for one element of it alike — an element reads as a rigid
parameter at the pack's slot (`trait_variadic_body_pack_element_receiver.wado`).
This is rustc's `ParamEnv`. It is a parameter from the start because the
annotate-time `Scope` it replaces is mutable state threaded through the
elaborator, which is hard to retrofit.

A query also carries the asking module. A `Reflect*` bound holds only where the
receiver's members are visible at the use site, and scope gates candidates by
what that module imported, so `Program` carries member visibility and no query is
a function of types alone.

#### Derivation is impl generation

Answering a bound is not, today, an impl lookup. In the compiler `T: Tr` holds
these ways:

- `Inspect` holds for everything.
- A plain `enum` derives `Display`.
- A generic body's parameter holds by its own signature.
- A reference satisfies the reference identities.
- A primitive carries `Eq` / `Ord` and the operator items.
- A struct satisfies `Eq` / `Ord` / `Default` / serde, and a variant `Eq` /
  serde, when its members do.
- A declaration satisfies its own reflection kind.
- An impl is written.
- A blanket answers and its bound is asked in turn.

Structural derivation for a struct or variant is the one that breaks the shape.
It is asked as a _query_: "is this struct `Eq`?" walks the members and asks the
full question of each, so derivation and impl search are mutually recursive, and
a repeated `(type, trait)` pair means two different things depending on how it
was reached. Through a blanket's bound it is the ungrounded cycle and answers
no; through a member it is a recursive type (`struct Node { next: Option<Node> }`)
and answers yes. That is the two-stack problem, and it exists only because `Eq`
is modelled as an auto trait.

The solver models derivation as impl generation, as Rust's `derive` does, so
trait solving is one uniform search over impls:

- A structural trait is not special in `holds`. It is answered by impls.
- Derivation is generating an impl per _declaration_: `struct D<P1..Pn>` that
  can derive `Eq` contributes `impl<Pi: Eq, …> Eq for D<P1..Pn>`, bounded on
  the parameters its members mention. A plain `enum` or `flags` has no
  members and derives unconditionally. A newtype derives nothing; it inherits.
- Whether a declaration can derive is a definition-time fixpoint over the
  declarations, which are finite and known before any body is elaborated. `D`
  derives `Tr` when every member type satisfies `Tr` under `Pi: Tr`, asked of
  `holds` with every declaration's tentative impl in place; a declaration that
  fails is removed, until none does. Assuming and then refuting is what makes a
  recursive type derive, which is the answer the compiler gives today.
- A written impl for the pair blocks derivation, and a marker
  (`impl Eq for D;`) demands it: the marker answers the bound, and the
  compiler's conformance check reports one on a declaration that cannot
  derive (WEP 2026-06-25).
- A `Reflect*`-bounded blanket of a structural trait
  (`impl<S: ReflectStruct<…>, ..F: Serialize> Serialize for S`) is the derived
  body's source, not a candidate. The lowering leaves it out, and `derive`
  answers per declaration.
- `holds` reports the derived and marker impls its answer passed through. The
  caller records the bodies to emit; the solver never records one itself.

The solver then has one recursion: `holds` reaches other questions only through
an impl's bounds, so a repeated pair is always the ungrounded cycle, and the
rule is one line with unit tests. A generic instance never needs its own entry:
`List<Point>: Eq` is the prelude's `impl<T: Eq> Eq for List<T>` and the
derived `impl Eq for Point`, assembled at the query. And the derivation rule
itself is a function of declarations, under unit test: every member satisfies
the trait, under the parameters' bounds.

The remaining ways are read off a type, and each goes where it belongs:

- A primitive's `Eq`, `Ord` and operator items are impls the lowering states.
- A trait that holds for everything is a flag on its declaration, since the
  unbounded blanket that would say so is rejected.
- How a reference answers is a flag on the trait: `Eq` of itself, `Ord` never,
  and the rest by auto-deref to the pointee unless a receiverless method names
  `Self`.
- A reflection kind is a fact stated of a declaration. It depends on the asking
  module's view of the members and on no member's trait, so the fact names the
  modules the members are visible from and answers for every instance.
- So are the three the compiler reads off a type's shape, each holding from
  everywhere: a plain `enum`'s `Display`; `Default` for a struct whose every
  field has a default, which a generic one does not get, since a default is
  elaborated against the declaration; and `Ref` — whether a reference stands in
  for the type — with `RefMut` the same minus a variant, whose case a write
  could change, and a function. Each is stated by the compiler's own predicate,
  asked through a type standing for the head, so the fact and the query cannot
  drift. A reference satisfies `Ref` and `RefMut` of itself, which is the
  reference flag above rather than a fact.

A fact is keyed by the declaration a type instantiates, and a tuple is an
instance of the tuple declaration. It lowers to its own shape rather than to a
declaration, because an impl spells one `[..T]`, so the program names that
declaration for the lookup to reach it.

A bound says two more things than a trait. It spells no arguments
(WEP 2026-07-31), so it asks for the trait at its declared defaults, and an impl
of another instantiation (`impl Mul<Inch> for Cm` against `T: Mul`) does not
answer it: the defaults are on the trait, `Self` meaning the impl's target. And
it may pin an associated type (`T: Mul<Output = T>`): the pin is on the bound,
and the impl that answers it must bind the type as the pin says, read off the
impl's own `type Output = …`. A pin naming a parameter the target leaves
unbound (`Members = [..C]`) reads the projection rather than checking it. A
bound on that parameter (`..C: Arbitrary`) waits for monomorphization
(WEP 2026-03-14).

#### The five questions

| Function                                            | Rules it owns                                                                          |
| --------------------------------------------------- | -------------------------------------------------------------------------------------- |
| `coherence_errors(program)`                         | duplicate `(Trait, Type)` pairs, an unbounded value blanket, variadic overlap, orphans |
| `derive(program, trait_, declarations)`             | which declarations derive `trait_`, and the impls that says                            |
| `holds(program, env, ty, trait_, scope)`            | bound satisfaction, supertraits, the cycle rule                                        |
| `candidates(program, env, receiver, method, scope)` | the three candidate lists, the scope gate, each candidate's depth                      |
| `rank(candidates)`                                  | ranks 0-3 and both ambiguities                                                         |

Two disciplines keep them functions rather than passes:

- A diagnostic is returned, never emitted. `rank` answers with a winner or a
  description of the ambiguity by candidate, and the elaborator turns that into a
  message with the names only `DefTable` knows. An ambiguity becomes testable for
  the same reason.
- A derivation request is returned, never recorded. Answering a bound today
  writes "synthesize `Eq` for this type" into the `TypeTable` on the way past.
  The function reports the requests its answer depends on and the caller records
  them; a caller that drops one silently loses a derived body.

Nothing flips at once. The fixture corpus is the drift detector, as
`verify_arg_synthesis` already uses it for argument synthesis (WEP 2026-07-31):
a question the solver answers is asserted against the compiler's own path in
debug builds over every fixture before the compiler's path is retired. `holds`
still runs that way beside `type_implements_trait`.

Selection has one candidate set. The order names the impls that answer, each an
impl block, or for a derived body the `Reflect*` blanket it comes from. Lookup
reads the `TraitMethodMatch` off those blocks and nothing else, so it enumerates
no impl the order will discard. A named block declares the method, or its trait
does, so it yields a match; one that yields none is an assertion in every
profile. A receiver the lowering cannot say has no trait method, since no impl
can be written for such a shape: one still carrying an inference variable, or an
error.

## Consequences

Selection is one ordered list. A new rule becomes a rank in that list, and a
missing rank is visible. Two calls that used to differ only by declaration order
now either agree or report, and the report names an impl the programmer can
write.

This document states the order, and `candidates` with `rank` implement it.
Method lookup decides nothing of its own: it asks the order, materializes a
match from each impl the order names, and reports what the order tied.

## Known gaps

### Scope gates method calls, not the bounds path

A method call is gated: an impl that applies while its trait is unimported is
reported as "not imported here", naming the trait
(`trait_error_unimported_trait_method.wado`, `trait_error_unimported_blanket.wado`).
A call through a bound is not: `T: Sub` still reaches `Base`'s methods with
`Base` unnamed, since the bounds path resolves without the order (above).

- [ ] Gate the bounds path on the supertrait's declaration being in scope
      (`trait_error_unimported_supertrait_method.wado`).

### A ref blanket never dispatches

The order ranks `impl<T: Bound> Tr for &T` as the third candidate list, and
`candidates` names one for a reference receiver. The compiler's collection still
refuses it: the reference step adopts only concrete `&T` impls
(`method_call.rs`, `is_blanket_ref_impl`), so the order's verdict is discarded
and the call reports no method (`trait_ref_blanket_dispatch.wado`). The
prelude's `Inspect for &T` works because the compiler answers that bound itself.

- [ ] Collect a reference blanket as a match, or materialize the match from the
      winning `ImplId` (above), which makes the collection moot.

### Two coherence rules still read the AST

`coherence_errors` owns four rules and answers the duplicate pair and the
unbounded value blanket. The other two still run over the AST:

- [ ] Variadic overlap, `check_variadic_impl_overlap`. Moving it needs `Program`
      to carry a pack's bounds, which the key it compares on deliberately
      ignores (WEP 2026-03-14 §5 Rule 2).
- [ ] The orphan rule. Moving it needs each declaration's module and the
      package boundary, which `Program` does not carry yet.

### The other dispatch paths do not share the order

`Type::m(args)` scans the receiver's trait impls current-module-first and takes
the first that declares the method, with value blankets on a separate fallback
behind it; operators and indexing filter by operand type and report
unique-or-error; a bound in a generic body takes the first bound declaring the
name. Ranks 0-3 exist on none of them; they agree with the order only by
coincidence of scan order. One selection function serving every path is the
fix; what stands in the way is that each path holds a different amount of the
call (a receiver type, an operand class, a bound list).

### Derivation is still a query in the compiler

`structural_conformance` still walks a receiver's members at each bound and
asks the full question of each, and the recursion guard counts the member
descents to tell a recursive type from an ungrounded cycle. The solver's
`derive` runs beside it: every declaration is lowered and derived when the
`Program` is built, and `holds` answers under the differential against
`type_implements_trait` over every fixture. One receiver the differential skips,
since only the compiler answers for it: a head the program names without members
— an anonymous struct, whose shape a literal mints after the `Program` is built,
and a struct declared in a body, whose fields annotate resolves in that body.
Such a head reaches a blanket whose bound holds of everything (`Inspect`) or an
impl written for it, and no `Reflect*` fact or derived impl
(`trait_local_struct_receiver_blanket.wado`). What is left is the flip:

- [ ] Route the derived bodies through what `holds` reports instead of
      `record_bound_driven_synth_request_for`, and retire the member walk.
- [ ] State a late declaration's members when they are known — an anonymous
      struct at its literal, a body-local struct at its statement — or lower
      them as the declaration `derive` reads.

### Specificity is refused, and now has a named cost

Rank 3 declines to rank a blanket whose bounds imply another's, and the reason
stands: a narrower impl binding an associated type differently is the
specialization soundness question, which a rank would decide by accident.

Reflection is the first pattern to pay for it. A derivation written over the
kind traits has no last-resort arm: an `impl<T: Reflect>` meant to catch
everything else reports at every struct, beside the `impl<T: ReflectStruct>`.
So a receiver the kinds do not admit (members not visible here, or a kind the
set does not cover) produces no candidate rather than falling through, and rank
2's escape, an impl written for the receiver, cannot serve a derivation whose
subject is unknown by construction.

[Total Reflection](./wep-2026-09-05-total-reflection.md) serves that side
without a rank: one `impl<T: Reflect>` branching on the kind inside its body
leaves no overlapping pair to break. This gap is therefore independent of
reflection. Reopening it takes both of:

- [ ] Answer what a caller sees when the narrower impl binds an associated type
      differently — the question rank 3 declines.
- [ ] Answer incomparable bound sets. Supertrait closure is a partial order:
      `{Constrained, Reflect}` beside `{ReflectStruct, Reflect}` is neither
      narrower nor wider, so a specificity rank leaves that pair at rank 3 and
      the ambiguity report has to keep naming it.

## Related WEPs

What each contributes to the order above.

- [Variadic Type Parameters](./wep-2026-03-14-variadic-type-parameters.md) — rank 0
- [Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md) — rank 1
- [Overload Resolution](./wep-2026-07-31-overload-resolution.md) — arguments, and the two-trait ambiguity
- [Library-Defined Derivation over `Reflect*`](./wep-2026-06-13-reflect-derivation.md) — the `Reflect*` eligibility gate
- [Super Traits](./wep-2026-07-27-super-traits.md) — what a specificity rank would read
