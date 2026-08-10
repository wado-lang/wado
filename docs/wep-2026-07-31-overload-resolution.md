# WEP: Overload Resolution

## Context

A method call `recv.m(args)` resolves by receiver type and method name. Lookup
is sequential and first-hit-wins — concrete ref impls, inherent impls, trait
impls on the base type, type-parameter bounds, associated-type projection
bounds (`resolve_method_call_with`,
`wado-compiler/src/elaborator/method_call.rs`). Arguments are elaborated
_after_ the winner is known, against its signature, so that parameter types
drive literal coercion and default insertion. That ordering is the
chicken-and-egg noted in
[Struct and Trait System](./wep-2026-01-13-struct-and-trait.md): selecting by
argument type requires typing the arguments first, independently of any
candidate.

Name alone leaves three collision shapes, and they need one answer, not three:

- one trait implemented for one receiver at several argument lists
  (`Take<A>` / `Take<B>` for `bool`),
- two traits sharing a method name on a concrete receiver,
- two traits sharing a method name reached through bounds or supertraits — the
  gap [Super Traits](./wep-2026-07-27-super-traits.md) documents as "a qualified
  call form that does not exist yet".

Two resolution paths already select an implementation from types at the call
site:

- Indexing filters `IndexValue<I>` / `IndexRef<I>` impls by the operand's type
  (`find_indexing_trait_impl`), which is what lets `List<T>` implement
  `IndexValue<i32>`, `IndexValue<RangeExclusive<i32>>` and
  `IndexValue<RangeInclusive<i32>>` at once.
- `Type::from(x)` / `try_from` filter `From<T>` / `TryFrom<T>` impls by the
  argument (`conversion_preselect`, `locate_static_method_impl`).

Both bake the winning trait's spelling into the mangled method name
(`Type^Trait::method`), which is also how monomorphization discriminates
instances (`InstantiationKey` keys on the mangled name plus type-argument
vectors). The backend is already argument-sensitive; named method calls are
what lacked the mechanism.

The static path was not merely name-only, it was circular. `Type::m(args)`
elaborated its arguments against parameter types found by (receiver, method)
name — `static_method_index` is searched with `find`, so with two conversion
impls it returns whichever comes first — and _then_ selected the impl from the
resulting argument type. Selection shaped the argument and the argument drove
selection; where the two disagreed no impl matched (`Wrapper::from(42)` against
`From<String>` beside `From<i64>` typed the literal `i32` and matched neither,
an ICE at WIR build). Argument-directed selection is what removes the
circularity, not a feature layered on it.

Selection therefore needs a type for each argument before any signature exists,
and how good that answer is decides whether the feature is usable at all. A
classifier that types only literals and locals leaves the most common argument
forms — a call, a method call, a field read, an operator, a range — admitting
every candidate, which makes the one overload set every program touches
unusable:

```wado
fn pick() -> i32 { return 1 }

export fn run() {
    let l = [1, 2, 3] as List<i32>;
    let v = l.index_value(pick());   // must resolve to IndexValue<i32>
}
```

Arithmetic operators cannot exhibit any collision yet: the prelude's operator
traits take no type parameters (`trait Add { type Output; fn add(&self, rhs:
&Self) … }`), so `impl Add<X>` is not writable and an operator trait has exactly
one argument list. RHS-directed selection there is gated on parameterizing the
operator traits (`trait Add<Rhs = Self>`), a stdlib redesign outside this WEP;
`find_arithmetic_trait_impl`'s first-impl scan is safe until then because
coherence permits only one impl.

The design constraint is the [design philosophy](./design-philosophy.md): no
function overloading. This WEP does not add it. What it defines is which
candidate sets are legal, how a unique candidate is chosen from types available
at the call site, and the explicit syntax for naming one when the compiler
cannot.

## Decision

### What can and cannot overload

Never overloadable — declaring a second one with the same name is an error:

- free functions,
- inherent methods on one type (per instantiation),
- methods within one trait declaration,
- arity of one function (default arguments cover optional parameters).

The only overload set Wado has: one trait implemented for one receiver at
several argument lists (`impl Take<A> for bool` beside `impl Take<B> for bool`).
Coherence already accepts these — they are different traits instantiation-wise,
but they share one declaration, so every member has one contract and one
signature shape, parameterized by the trait's type arguments. Selecting among
them by argument type is the same operation indexing performs, now available to
every generic trait.

Distinct traits never form an overload set. A method name reachable through two
different trait declarations is an ambiguity error at the call site, in every
shape, including the concrete-receiver case. The rationale is twofold: impls of
different traits share no contract, so a type-directed pick is a semantic guess;
and allowing it would reintroduce ad-hoc overloading through one-method traits,
which the design philosophy rules out. The escape is the qualified call syntax
below, not renaming. One tie-break before the error: when the colliding
declarations share a bare name and exactly one is in scope at the call site
(declared or imported by the calling module), that one is selected — a
same-named foreign trait a module never imported does not break its calls, which
is what keeps two libraries' private `trait Loud` from colliding through a
shared receiver type.

One exception: a blanket candidate does not count toward the collision. Wado
does not scope method candidates by which traits are imported, so a foreign
`impl<T: Bound> Foreign for T` reaches every receiver in the program — counting
it would make adding a blanket impl to a library a breaking change for every
downstream method of that name, which is the cost Rust pays with trait-import
scoping instead. A blanket is the general case and loses to any impl written for
the receiver: the same specific-impls-win ordering the language already applies
within one trait, read across traits. Two blankets colliding with each other
resolve by the existing order rather than erroring; if that turns out to bite,
the answer is candidate scoping, not counting blankets here.

### Resolution algorithm

For the trait-impl step of method lookup (inherent methods still shadow trait
methods; the ref-impl priority and the earlier steps are unchanged):

1. Collect the candidate impls providing `m` for the receiver (impl-index
   buckets along the newtype chain, plus satisfied blankets; for a generic
   receiver, the transitive closure of its bounds).
2. Group candidates by trait identity. Identity is structural, never a spelling:
   the trait is its declaration key (declaring module + declared name), so two
   modules' same-named traits stay distinct and an alias compares equal to the
   original; an argument list is its resolved types, so `Take<Alias>` and
   `Take<A>` name one list. Each candidate's identity is resolved in its impl's
   own frame — the impl module's imports, with the impl's bound type parameters
   substituted.
3. More than one group → ambiguity error naming every group's method as
   `Type^Trait::m` and suggesting a qualified call.
4. One group with one argument list → resolved.
5. One group with several argument lists → argument-directed selection:
   1. Classify each argument (next section).
   2. Keep each candidate whose substituted parameter list matches: at every
      position the parameter type must lie in the argument's denoted set. A
      parameter still containing an unsubstituted method-level type parameter
      accepts anything.
   3. All survivors naming the same trait instantiation → the existing
      [specific-impls-win rule](./spec.md#specific-impls-win) picks the impl.
      Survivors naming distinct instantiations → ambiguity error. No other
      ranking exists — there is no best match, only a unique match.
   4. A unique survivor is selected: its full trait spelling is recorded in the
      dispatch fact and mangled name, and the arguments are then elaborated
      against its signature exactly as an unambiguous call is (literal coercion,
      range checks, defaults, effect checking).

Static calls `Type::m(args)` follow the same grouping and selection over
associated functions. `from` / `try_from` add a preselect step on top of it (see
Conversions).

Because a trait declaration fixes its methods' arity and owns their default
arguments, every candidate in an overload set has the same argument count —
arity never discriminates, and no arity filter exists.

### Argument synthesis

Selection needs argument types before any signature is chosen, so each argument
is classified by a judgement over its expression — argument synthesis. It is
total (every `ast::Expr` variant has a rule) and side-effect-free (it never runs
`resolve_expr`; see Side-effect discipline).

A class denotes the set of types the argument could elaborate to at this call
site, under any candidate's parameter type:

| Class            | Denotes                                                          |
| ---------------- | ---------------------------------------------------------------- |
| `Exact(t)`       | `{t}` — plus a `&mut t` argument answering a `&t` parameter      |
| `Head(h)`        | every type whose head declaration is `h`, and newtypes over them |
| `IntLit`         | integer and float types, and their newtypes                      |
| `FloatLit`       | float types and their newtypes                                   |
| `StrLit`         | `String` and its newtypes                                        |
| `NullLit`        | every `Option<T>`                                                |
| `Opaque(reason)` | every type                                                       |

`Head` is what lets a partially known type still select. `0..<n` is
`RangeExclusive<T>` for a `T` the endpoints need not pin, but its head is
`RangeExclusive` whatever `T` is — enough to reject `IndexValue<i32>` and
`IndexValue<RangeInclusive<i32>>` and admit exactly one candidate. It also
carries a generic call's return type (`fn ids() -> List<T>` synthesizes
`Head(List)`) and a generic named struct literal.

`Opaque` names why no type was produced, from a closed set:

| Reason            | Arises from                                                                |
| ----------------- | -------------------------------------------------------------------------- |
| `Closure`         | a closure whose parameters or return type are unannotated                  |
| `CompoundLiteral` | `[…]`, `{…}`, a spread — typed by the expected type through builder traits |
| `Inference`       | `?`, `resume`, a tuple comprehension, branches that disagree               |
| `Unresolved`      | a name or type that did not resolve — error recovery                       |

There is deliberately no "unsupported" reason. The judgement matches on
`ast::Expr` with no wildcard arm (per the crate's rules), so a new expression
form cannot be added without either giving it a rule or naming the
language-level reason it has none. Incompleteness that remains is stated, never
implied by a catch-all.

### The soundness invariant

> For every argument `e` and every parameter type `p` a candidate declares at
> that position, if synthesis produces class `C`, then the type `e` elaborates
> to under `expected = p` lies in `C`'s denotation.

Over-approximation (a wider set than the truth) costs a resolution and yields an
ambiguity error asking the user to disambiguate. Under-approximation selects the
wrong impl or rejects a well-typed call, and is a compiler bug. Every rule is
admissible only because it over-approximates.

Two corollaries the design leans on:

- Sharpening a class is backward compatible. A well-typed call whose set shrinks
  keeps its true type in the set, so a candidate that survived still survives; a
  call that stops resolving was ill-typed and would have failed at argument
  typecheck with a worse message.
- The invariant is checkable. After the winner's signature elaborates the
  arguments, every position whose class was `Exact` / `Head` can be compared
  with the type the argument actually received. A mismatch is a synthesis bug,
  and it is asserted rather than trusted.

### Synthesis rules

Receiver, operand and branch sub-expressions are synthesized recursively; a rule
applies only when its premises produce `Exact` / `Head`, and yields
`Opaque(Inference)` otherwise. A type mentioning an unsubstituted type parameter
yields `Head` when its head is known and `Opaque(Inference)` when it is not.

| Expression                                   | Class                                                                                    |
| -------------------------------------------- | ---------------------------------------------------------------------------------------- |
| integer / byte literal                       | `IntLit`                                                                                 |
| float-only literal                           | `FloatLit`                                                                               |
| string, template string, `#include_str`      | `StrLit`                                                                                 |
| `#file`, `#function`, `#data`                | `StrLit`; `#line` → `IntLit`                                                             |
| char / bool / unit literal                   | `Exact`                                                                                  |
| `null`                                       | `NullLit`                                                                                |
| bytes literal, `#include_bytes`              | `Exact(List<u8>)` — the elaborator ignores the expected type here                        |
| identifier — local, parameter, global, const | `Exact` of its declared type                                                             |
| identifier — enum / flags / variant case     | `Exact` of the declaring type                                                            |
| identifier — named function                  | `Opaque(Inference)` — a function reference is instantiated by the expected type          |
| identifier — resolves to nothing             | `Opaque(Unresolved)`                                                                     |
| `&e` / `&mut e`                              | the reference over `e`'s class                                                           |
| `*e`                                         | the referent of `e`'s class                                                              |
| `-e`, `!e`, `~e`                             | primitive: `e`'s class; otherwise the operator impl's `Output`                           |
| `e as T`                                     | `Exact` / `Head` of `T`, including generic `T`                                           |
| comparison, logical, `matches`, chain        | `Exact(bool)`                                                                            |
| arithmetic / bitwise                         | primitive: the operand classes met; otherwise the impl's `Output`                        |
| shift                                        | the left operand's class                                                                 |
| assignment, compound assignment              | `Exact(unit)`                                                                            |
| call — named function, variant constructor   | the declared return type                                                                 |
| call — `fn`-typed value                      | the function type's return type                                                          |
| method call                                  | the resolved method's return type                                                        |
| static method call                           | `Opaque(Inference)` — a generic receiver head takes its arguments from the expected type |
| field access, tuple field                    | the field's type                                                                         |
| `e[i]`                                       | the `IndexValue` / `IndexRef` `Output` for the key's type                                |
| `a..<b`, `a..=b`                             | `Exact` when both endpoints agree; otherwise `Head(Range*)`                              |
| named struct literal                         | `Exact` when non-generic, else `Head`                                                    |
| anonymous struct / tuple literal / spread    | `Opaque(CompoundLiteral)`                                                                |
| block, labeled block, `with … do`            | the tail expression's class; no tail → `Exact(unit)`                                     |
| `if`, `match`                                | the join of the branch classes                                                           |
| closure                                      | `Opaque(Closure)`                                                                        |
| `e?`, `resume`, tuple comprehension          | `Opaque(Inference)`                                                                      |
| parse-error placeholder                      | `Opaque(Unresolved)`                                                                     |

A subscript is the one position read differently from an argument: its key is
resolved with no expected type before the indexing impl is selected, so a
literal key takes its default type there and synthesis must read it the same
way.

Branches widen and operands sharpen, so the two composite rules move opposite
ways through the lattice. The join for `if` / `match` is the least upper bound:
when one side's set contains the other's the wider wins (an integer literal
beside an `i32` is an integer literal), two exact types sharing a head widen to
that `Head`, and anything else widens to the top. The meet for a primitive binary operator is the greatest
lower bound, admissible because both operands and the result share one type
there: an `Exact` operand pins the result, two literals keep the literal class.

Every rule delegates its hard question to a query that already exists —
`lookup_function_return_type`, `lookup_method_info`,
`find_trait_method_for_type`, `find_indexing_trait_impl`,
`find_arithmetic_trait_impl`, the struct-field tables, `resolve_type`. Synthesis
composes those answers; it does not re-derive them. That is what keeps it a
judgement rather than a second type checker.

A literal class admits candidates; it never selects one. `f.take(42)` against
`Take<i32>` beside `Take<i64>` is an ambiguity error — the fix is `42 as i64`, a
binding with a type annotation, or a qualified call — because letting the
literal's default type decide would make adding an `impl Take<i32>` silently
retarget every existing call that meant `Take<i64>`. The common cases are
unaffected: `l[0]` resolves, because an integer literal does not admit the
range-typed candidates; `f.take(B { v: 1 })` resolves, because a named struct
literal has a context-free type.

Classes exist only to filter. After selection the winning signature drives
ordinary argument elaboration, so a literal still coerces to the chosen
parameter type, and value-range checks run there — selection never depends on a
literal's value.

### Side-effect discipline

Synthesis runs during lookup, before any decision is committed, so it must leave
no trace but interning. It never calls `resolve_expr`, never takes
`&mut FunctionContext`, never builds TIR, and never records a dispatch or
desugar fact.

A speculative `resolve_expr` against a scratch `ModuleSemantics` is not an
option, now or later. It would corrupt three things living outside the
`AstId`-keyed annotation maps: the `FunctionContext` local-index walk that reify
replays in lockstep (a discarded synthetic local — `__ref_*`, `__qm_*`, `__b` —
desyncs every later index into silently wrong code), the anonymous-struct dedup
guard in the shared `TypeTable` (a speculative literal would satisfy it and the
real resolve would register nothing), and `record_bound_driven_synth_request`
(no removal API).

The lookup queries synthesis does call are not free of writes, and each is
closed:

- Use→def edges. The lookups already run under
  `with_reference_recording_suppressed`, for their own reason: they walk foreign
  declarations whose edges belong to the declaring module.
- Diagnostics. A nested lookup can report its own ambiguity or a missing method.
  Reporting it from inside synthesis would duplicate the diagnostic the real
  elaboration is about to emit, and would bump the error count on an argument
  that may still compile. `Logger` carries a suppression scope — a depth counter
  beside `error_count`, checked in `emit_error` — entered for the duration of a
  synthesis.
- Interning and bound-driven synthesis requests. Both are monotone, and
  synthesis only ever examines arguments the same call is about to elaborate for
  real, so every request it triggers is one the real path triggers too. They are
  left alone.

The discipline is guarded, not merely documented: in debug builds a synthesis
scope snapshots the lengths of the `sem` annotation maps on entry and asserts
them unchanged on exit. Interning and the `TypeTable`'s synthesis requests are
outside the snapshot by design.

### Cost

Synthesis is consulted only where selection needs it: after the candidate set is
collected, and only when it holds several concrete candidates of one trait
declaration at different argument lists. The classes of one call's arguments are
computed at most once and memoized across the (up to three) lookup attempts a
method call makes.

Classification must not be eager and universal: a single-candidate call — which
is effectively all of them — pays nothing, and that is what pays for the depth
that makes the multi-candidate case resolve.

### Qualified calls

One form names a candidate explicitly: trait-qualified (UFCS)
`Trait::method(receiver, args…)`. The `self` parameter becomes the first
argument, spelled to match its mode — `&x` for `&self`, `&mut x` for
`&mut self`, the value for `self`. A mismatched mode is an error, not a
coercion: passing a value where the method takes `&mut self` would mutate a copy
and silently drop the change. The one exception is the language's one reference
coercion — `&mut x` also answers a `&self` method:

```wado
Display::fmt(&p, f);          // p implements two traits declaring `fmt`
Base::name(&x);               // supertrait diamond inside a generic body
Take::<A>::take(f, B { … });  // one trait's argument list, pinned
```

The receiver argument supplies `Self`, so the trait's own type arguments are all
a turbofish needs to carry. That covers both collision shapes: across traits,
the trait name discriminates; within one trait, the turbofish pins the argument
list. The head and the turbofish arguments resolve to identities (declaration
key, resolved types) before filtering, so aliases and same-named foreign traits
behave like everywhere else — and only the named declaration may answer: the
auto-derived `Eq` / `Ord` bodies answer a qualified call only when it names
them, never a user trait that happens to share the method name. This closes the
escape gap Super Traits documents. If the named trait still has several argument
lists for the receiver and no turbofish is written, argument-directed selection
applies within it.

No new grammar: `Take::<A>::take(f, x)` is the static-path shape
`List::<i32>::with_capacity(10)` already uses. Only resolution differs — for a
static path `Head::name(args)`, `Head` resolves in the type namespace (type →
associated function; `interface` → effect operation), and a trait head makes it
a UFCS call. The reflect intrinsics (`ReflectStruct::<T>::members()`) keep their
existing spelling, where the turbofish names the reflected type instead — they
are compiler intrinsics whose traits declare no type parameters, so the two
readings never meet.

A qualified call is filed as a _static_ dispatch, not a method dispatch: it
spells its receiver's mode itself, so no receiver adjustment is owed and the
call is an ordinary one whose first argument happens to be the receiver — the
shape reify's `Call` arm already replays. Two record-keeping consequences are
load-bearing: the static record carries the resolved signature's real facts
(defaults, `is_mut`, parameter types, with the receiver prepended as slot 0),
never fabricated ones; and every downstream pass that positions a `Call`'s
arguments against its callee's parameters must account for the receiver in
`args[0]`, keying on value-argument indices rather than parameter slots so both
call spellings produce one key.

### The uncovered case

An associated function with no `self` has no receiver argument, so UFCS cannot
supply `Self`. When one type implements two traits that each declare an
associated function of the same name, the call is ambiguous with no escape:

```wado
trait Loadable   { fn load(p: String) -> Self; }
trait Restorable { fn load(p: String) -> Self; }
impl Loadable for Config { … }
impl Restorable for Config { … }

Config::load(p);    // ERROR: ambiguous, and unspellable — rename one
```

This is left open. The shape is narrow — `From` / `TryFrom` keep their own path,
and the prelude's `Default::default` / `FromStr::from_str` do not collide with
each other — so renaming is an acceptable answer for a case that has not arisen.
If it does, the follow-up must name `Self` without a receiver and without a
leading `<` (JSX owns that position): either binding `Self` from the expected
type (`let c: Config = Loadable::load(p)`, which contradicts this WEP's
forward-inference non-goal and so needs its own decision), or a turbofish naming
`Self` on the trait head.

### Conversions

`Type::from(x)` preselects among the receiver's conversion impls _before_ the
argument is elaborated, which is what removes the circular ordering at its root.
Admissibility is the same table selection uses, applied to each impl's source
type resolved in the impl's own frame (`conversion_impl_survey`) — so an integer
newtype admits an integer literal here exactly as it does there, and `From<i64>`
beside `From<Meters>` is ambiguous rather than silently primitive. One admitted
impl supplies the argument's expected type; several report
`AmbiguousConversionArgument`, whose fix is the cast — `from` has no `self`, so
the trait-turbofish escape cannot apply.

An argument that synthesizes a type selects through it. The matcher compares the
head (un-aliased in the impl's module) and, for a generic argument spelling, the
full spelling with whitespace ignored, so same-head impls (`From<List<i32>>`
beside `From<List<String>>`) are told apart; nested aliasing that changes the
rendered arguments is the name-based mechanism's remaining ceiling, with full
`TypeId` matching the eventual replacement.

Two shapes stay carved out at the gate rather than guessed at: an inherent
static `from` beside `From` impls answers on the trait-less path, and a
conversion reachable only through a blanket generic in its source type
(`impl<T: Display> From<T> for Wrapper`) is rejected with its own diagnostic —
it has never compiled, and selecting its instantiation needs generic-impl
monomorphization, not name matching.

`?`'s conversion stays target-type-directed by design and is unaffected.

### Diagnostics

Both ambiguity errors follow
[Diagnostic Reason Chains](./wep-2026-06-02-diagnostic-reason-chains.md): the
error names every surviving candidate in symbol notation, states per candidate
why it survived or was filtered — "argument 1 is an integer literal, admitted by
both `Take<i32>` and `Take<i64>`"; "argument 2 has type `String`, parameter is
`i32`"; "argument 1 is a closure, so it admits every candidate" — and carries a
fix-it: a qualified call for cross-trait collisions, an `as` cast or annotation
for literal-only distinctions. An `Opaque` argument reports its reason, which is
what tells the user whether the call can be fixed by annotating it. The
no-survivor case (candidates exist, all filtered) reports each candidate with
the position that rejected it instead of a bare "method not found".

### Interactions

| Feature           | Interaction                                                                                                                                                                                                                                                               |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Operators         | Indexing selects by the operand's type, which is the same rule read through a different entry point; the two must agree for every index expression. Arithmetic and bitwise operators select over `Add<Rhs>` and friends by the right operand's class. `Eq` / `Ord` take no trait arguments — unaffected. |
| `From` / `?`      | See Conversions. `?` stays target-type-directed.                                                                                                                                                                                                                          |
| Default arguments | Owned by the trait declaration, identical across an overload set — no interaction with selection.                                                                                                                                                                         |
| Effects           | Never considered by selection; the chosen method's `with` clause is checked afterwards.                                                                                                                                                                                   |
| Coherence         | Untouched. Selection picks an argument list among impls coherence already accepts; overlapping impls of one instantiation remain errors.                                                                                                                                  |
| Newtypes          | Inherited impls are candidates on the newtype receiver; the same grouping and selection apply.                                                                                                                                                                            |
| Monomorphization  | Unaffected. The chosen trait's spelling lands in the mangled name, which `InstantiationKey` already discriminates on.                                                                                                                                                     |
| LSP / tooling     | Hover and go-to-definition read the recorded dispatch fact; synthesis leaves no persistent state.                                                                                                                                                                         |

### Non-goals

- Ad-hoc overloading of free functions or inherent methods — permanently out,
  per the design philosophy.
- Expected-type or return-type directed selection. Inference stays forward;
  `?` / `From` remains the one target-directed conversion, on its own path.
- Ranking. Beyond the existing specific-impls-win rule there is no preference
  order; resolution is unique-or-error, so adding a preference later would only
  turn errors into compiles (backward-compatible), while removing one never is.
- Trait-import scoping of candidates. Trait impls stay globally visible to
  method lookup; scoping candidates by `use` is a separate question.

### Rejected alternatives

A fallback that picks a candidate when every argument is `Opaque`. The pick
would be made by candidate order, which is the silent wrong dispatch this design
exists to remove; on `List` it would compile `l.index_value(x)` to a range index
or an element index depending on impl order, and adding an impl would retarget
existing calls. What the pick lands on is already visible: report-and-continue
hands the arguments to the first candidate, so an ambiguous
`l.index_value(0..<2)` is followed by `expected 'i32', found
'RangeExclusive<i32>'` — the fallback would keep that selection and drop the
error. An overload set that cannot be told apart is an error with two escapes
(`as`, the trait turbofish), not a coin flip.

Speculative `resolve_expr` with rollback. Neither the `TypeTable` nor
`FunctionContext` is transactional, and making them so to serve classification
inverts the cost.

Elaborate-then-select — elaborating the arguments for real before selection and
keeping the results. It cannot type the fragment where selection matters most
(literals must not select, compound literals and closures cannot be typed
without a parameter), so it would not replace synthesis; it reorders local-index
allocation against reify's replay; and it would have to thread pre-elaborated
arguments through every call path. Indexing does exactly this for its single
argument, including a second `resolve_expr` of the same node when the selected
key type differs — the direction of travel is for that path to adopt synthesis,
not the reverse.

Joint constraint solving over selection and inference (Swift, C#). It is the
principled answer to "select and infer at once", and it contradicts Wado's
forward-inference non-goal.

`<Type as Trait<Args>>::method(args…)` as the qualified form. Not a
cost-benefit call — the syntactic position is taken. A leading `<` in expression
position belongs to JSX, reserved by
[Reactive Signals](./wep-2026-04-04-reactive-signals.md#jsx-integration)
(`return <button onclick={…}>`), so Rust's spelling is unavailable to Wado at
any price. It would also pin the receiver type, which is redundant wherever a
receiver argument exists. Only the trailing turbofish position (`::<`, after
`::`) stays free, which is what the adopted form uses.

`Type^Trait::method` in source. `^` is the xor operator, so `A ^ B::m(x)`
already parses as an expression; the caret form stays what it is — the internal
mangled-name and [symbol-notation](./wep-2026-06-14-symbol-notation.md) grammar
for tooling, docs, and diagnostics.

## Implementation

Where the pieces live:

- Grouping, ranking and the ambiguity reports: `select_trait_match`,
  `report_trait_argument_ambiguity`, `report_cross_trait_ambiguity`
  (`elaborator/method_lookup.rs`). Cross-trait collisions reuse the bounds
  path's `AmbiguousTraitMethod` so the shape a collision arrives in does not
  change the answer.
- Qualified calls: trait-head resolution in `resolve_static_method_call` — the
  branch where a head resolving to no type is otherwise an `UnknownFunction`
  error — binding the first argument as the receiver. `resolve_call` routes
  `Trait::method(recv, …)` to the method dispatcher ahead of its argument walk,
  and `MethodCallInput::required_trait` constrains which impls may answer,
  filtered in `find_trait_method_for_type_inner` before `select_trait_match`
  sees the candidates. No parser change: both `Greet::greet(&p)` and
  `Take::<A>::take(&f, x)` already parse, and the AST retains the turbofish.
- Conversions: `conversion_preselect` and `conversion_impl_survey`
  (`elaborator/method_call.rs`).
- Argument synthesis: `elaborator/synth.rs` — `ArgClass`, the admissibility
  table, the judgement, and `ArgProbe`, the per-call handle holding the
  argument ASTs, the `FunctionContext` and the memo. It is threaded through
  `find_trait_method_for_type` so `select_trait_match` classifies on demand,
  and the classes come back out (`take_classes`) to be checked against the
  elaborated arguments.
- The side-effect discipline: `Logger::quiet` (`logger.rs`) and, in debug
  builds, `ModuleSemantics::fact_count` asserted unchanged across a synthesis.
- The soundness invariant: `verify_arg_synthesis`, run in debug builds over
  *every* method-call argument — the ones selection asked about and the ones it
  did not — so the whole fixture corpus is the drift detector.
- Operators: `find_arithmetic_trait_impls` collects the receiver's impls of the
  operator's trait whose right-hand parameter admits the operand's class, and
  `find_arithmetic_trait_impl` is the unique-or-error view of it. The right
  operand's class is its resolved type at dispatch and its literal class at the
  earlier coercion lookup (`find_operator_rhs_type`), which is what lets `m * 2`
  select `Mul<i32>` before the literal has a type. A `&Self` parameter still
  expects the receiver's own type, so a newtype dispatched to its base's impl
  type-checks as the newtype.

The bounds counterpart of an overload set (`T: Take<A> + Take<B>`) cannot arise
yet — positional trait arguments do not parse in bound position.

Test surface: the `ufcs_*`, `trait_argument_*`, `from_overload_*` and
`cross_module_same_name_*` fixture families in `wado-compiler/tests/fixtures/`
— one fixture per rule above (selection, each ambiguity shape, each escape, the
scope tie-break, the receiver-mode errors), plus `error_*` fixtures pinning
every diagnostic's message. Argument synthesis adds one fixture per class of
rule: a free-function call, a method call (`l.index_value(v.len())`), a field
read, an operator (`i + 1`), a range, an `if` with agreeing branches, a cast to
a generic type, a const, an enum case; negative fixtures pin what stays
ambiguous and why (a closure, a compound literal, a generic call with an open
return type); and a pair fixture asserts `l[e]` and `l.index_value(e)` select
the same impl for every `e` above. `operator_rhs_selection.wado` pins the
operator counterpart: `Add` at two right-hand types, and a literal selecting
`Mul<i32>`.

## Consequences

Benefits:

- The `Take<A>` / `Take<B>` pattern is callable — trait-argument APIs
  (unit-typed conversions, protocol selectors) no longer dead-end at the call
  site — and the overload set every program touches, `List`'s three
  `IndexValue` impls, resolves for the argument forms programs actually write.
- Silent wrong dispatch disappears: the undiagnosed cross-trait first-wins is
  replaced by an error with an explicit escape.
- Supertrait diamonds and bound collisions get a fix that is not "rename the
  method".
- One principle covers what were three carve-outs (indexing, `From`,
  specific-impls-win) plus named calls: within one trait, types select; across
  traits, the user selects.
- What selection cannot see is data — an `Opaque` reason from a closed set —
  rather than a catch-all, and it reaches the diagnostic.

Trade-offs:

- Making the concrete-receiver collision an error is a breaking change where
  code relied on the locality preference; the qualified-call escape bounds the
  migration.
- Synthesis restates typing rules the elaborator also implements. The soundness
  assertion bounds the risk; removing the duplication means separating typing
  from lowering in `resolve_expr`, which is its own WEP.
- Classifying an argument re-runs the lookups its sub-expressions need, so a
  method call in argument position is looked up twice. Only multi-argument-list
  candidate sets pay; single-candidate calls are untouched. Debug builds pay it
  on every method-call argument, which is the price of the drift detector.
- Parameterizing the operator traits changes their declared shape, so a
  `T: Add` bound now means `Add<Self>` and an impl written against the old
  spelling still reads the same. What it buys is that `+` stops being the one
  operator whose impl a program cannot choose.
- Literal-only distinctions error where other languages would pick a default.
  This is deliberate — predictability over convenience — and the diagnostic
  carries the one-token fix.
- Partially known types select through their head alone, so a hypothetical
  `IndexValue<RangeExclusive<i32>>` beside `IndexValue<RangeExclusive<i64>>`
  would be ambiguous where an exact class would resolve. No such pair exists,
  and the fix is the ordinary escape.
- One new call form, and no new grammar for it — but `Trait::method(recv, …)`
  makes a static path's meaning depend on whether its head is a type or a trait.
  The two never collide (a name is one or the other), and Rust sets the
  precedent.
- Colliding associated functions stay unspellable (see The uncovered case). The
  spec's "not yet implemented" list keeps that entry, narrowed.

## See Also

- [Struct and Trait System](./wep-2026-01-13-struct-and-trait.md) — the
  name-only resolution rule this WEP extends
- [Super Traits](./wep-2026-07-27-super-traits.md) — the bound-collision rule
  and the escape gap closed here
- [Operator Overloading](./wep-2026-01-18-operator-overloading.md) and
  [Indexing Traits Design](./wep-2026-01-20-indexing-traits.md) — the operand-
  type selection precedent
- [Conversion Traits](./wep-2026-03-16-conversion-traits.md) — the `From` path
- [Default Arguments](./wep-2026-04-11-default-arguments.md) — why arity never
  discriminates
- [Diagnostic Reason Chains](./wep-2026-06-02-diagnostic-reason-chains.md) — the
  shape the ambiguity errors take
- [Symbol Notation](./wep-2026-06-14-symbol-notation.md) — the `Type^Trait`
  spelling that stays tooling-only
