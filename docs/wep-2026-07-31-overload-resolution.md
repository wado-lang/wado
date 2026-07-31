# WEP: Overload Resolution

## Context

A method call `recv.m(args)` resolves by receiver type and method name alone.
Lookup is strictly sequential and first-hit-wins — concrete ref impls, inherent
impls, trait impls on the base type, type-parameter bounds, associated-type
projection bounds (`resolve_method_call_with`,
`wado-compiler/src/elaborator/method_call.rs`). Arguments play no part in
selection; they are elaborated _after_ it, against the winner's signature, so
that parameter types drive literal coercion and default insertion. This is the
chicken-and-egg noted in
[Struct and Trait System](./wep-2026-01-13-struct-and-trait.md): selecting by
argument type would require typing the arguments first, independently of any
candidate.

The name-only rule leaves three collision shapes, with inconsistent outcomes:

| Shape                                                              | Today                                                         |
| ------------------------------------------------------------------ | ------------------------------------------------------------- |
| One trait at two argument lists (`Take<A>` / `Take<B>` for `bool`) | Rejected: `AmbiguousTraitArguments`                           |
| Two traits sharing a method name, concrete receiver                | Silently takes the first after a local-impls-first sort       |
| Two traits sharing a method name via bounds / supertraits          | Rejected: `AmbiguousTraitMethod` — and no escape but renaming |

The second row is a defect: which trait's method runs depends on an internal
sort order the user never sees. The third row is the gap
[Super Traits](./wep-2026-07-27-super-traits.md) documents — "a qualified call
form that does not exist yet".

Meanwhile, two resolution paths already select an implementation from types at
the call site:

- Indexing filters `IndexValue<I>` / `IndexRef<I>` impls by the operand's type
  (`find_indexing_trait_impl`), which is what lets `List<T>` implement
  `IndexValue<i32>`, `IndexValue<RangeExclusive<i32>>`, and
  `IndexValue<RangeInclusive<i32>>` at once.
- `Type::from(x)` / `try_from` filter `From<T>` / `TryFrom<T>` impls by the
  argument's type (`locate_static_method_impl` with an `arg_type_hint`).

Both bake the winning trait's spelling into the mangled method name
(`Type^Trait::method`), which is also how monomorphization discriminates
instances (`InstantiationKey` keys on the mangled name plus type-argument
vectors). The backend is already argument-sensitive; only named method calls
lack the mechanism.

Arithmetic operators are a third, inconsistent case: `find_arithmetic_trait_impl`
matches `Add` by base name and takes the first impl, so `Add<X>` beside `Add<Y>`
resolves by declaration order rather than by RHS type.

The static path is not merely name-only, it is circular. `Type::m(args)`
elaborates its arguments against parameter types that
`lookup_static_method_param_types_keyed` finds by (receiver, method) name —
`static_method_index` is searched with `find`, so with two conversion impls it
returns whichever comes first — and _then_ selects the impl from the resulting
argument type. Selection shapes the argument, and the argument drives selection.
Where the two disagree, no impl matches: `Wrapper::from(42)` against
`From<String>` beside `From<i64>` typed the literal `i32` and matched neither,
which is now reported rather than reaching WIR build as a trait-less
`Wrapper::from` (an ICE until this WEP's groundwork landed). Argument-directed
selection is what removes the circularity, not an extra feature layered on it.

The design constraint is the
[design philosophy](./design-philosophy.md): no function overloading. This WEP
does not add it. What it defines is which candidate sets are legal, how a unique
candidate is chosen from types available at the call site, and the explicit
syntax for naming one when the compiler cannot.

## Decision

### What can and cannot overload

Never overloadable — declaring a second one with the same name is an error, as
today:

- free functions,
- inherent methods on one type (per instantiation),
- methods within one trait declaration,
- arity of one function (default arguments cover optional parameters).

The only overload set Wado has: one trait implemented for one receiver at
several argument lists (`impl Take<A> for bool` beside `impl Take<B> for bool`).
Coherence already accepts these — they are different traits instantiation-wise,
but they share one declaration, so every member has one contract and one
signature shape, parameterized by the trait's type arguments. Selecting among
them by argument type is the same operation indexing already performs, now
available to every generic trait.

Distinct traits never form an overload set. A method name reachable through two
different trait declarations is an ambiguity error at the call site — in every
shape, including the concrete-receiver case that today resolves silently. The
rationale is twofold: impls of different traits share no contract, so a
type-directed pick is a semantic guess; and allowing it would reintroduce ad-hoc
overloading through one-method traits, which the design philosophy rules out.
The escape is the qualified call syntax below, not renaming.

### Resolution algorithm

For the trait-impl step of method lookup (inherent methods still shadow trait
methods; the ref-impl priority and the earlier steps are unchanged):

1. Collect the candidate impls providing `m` for the receiver, as today
   (impl-index buckets along the newtype chain, plus satisfied blankets; for a
   generic receiver, the transitive closure of its bounds).
2. Group candidates by trait identity: base trait name plus declaring module.
3. More than one group → ambiguity error naming every group's method as
   `Type^Trait::m` and suggesting a qualified call.
4. One group with one argument list → resolved; today's path, unchanged.
5. One group with several argument lists → argument-directed selection:
   1. Probe-type each argument (next section).
   2. Keep each candidate whose substituted parameter list matches: at a
      position where the probe produced a concrete type, the parameter must
      accept exactly that type (the standard `&mut T` → `&T` coercion counts;
      nothing else does); at a position where the argument is a literal, the
      parameter must merely _admit_ the literal (see the admissibility table);
      a parameter still containing an unsubstituted method-level type parameter
      accepts anything.
   3. All survivors naming the same trait instantiation → the existing
      [specific-impls-win rule](./spec.md#specific-impls-win) picks the impl.
      Survivors naming distinct instantiations → ambiguity error. No other
      ranking exists — there is no best match, only a unique match.
   4. A unique survivor is selected: its full trait spelling is recorded in the
      dispatch fact and mangled name, and the arguments are then elaborated
      against its signature exactly as an unambiguous call is today (literal
      coercion, range checks, defaults, effect checking).

Static calls `Type::m(args)` follow the same grouping and selection over
associated functions. `from` / `try_from` keep their dedicated hint-based path
for now (see Interactions).

Because a trait declaration fixes its methods' arity and owns their default
arguments, every candidate in an overload set has the same argument count —
arity never discriminates, and no arity filter exists.

### Probe typing

Selection needs argument types before any signature is chosen, so arguments are
probe-typed: elaborated bottom-up with no expected type, under a speculation
discipline — no facts recorded, no diagnostics emitted. Expressions with
context-free types (variables, field reads, calls with known return types,
named struct literals, ranges) produce their type. Literals produce a class,
not a type:

| Argument                       | Probe result  | Admissible parameters                          |
| ------------------------------ | ------------- | ---------------------------------------------- |
| integer literal                | IntLit        | integer types, floats, their newtypes          |
| float literal                  | FloatLit      | float types, their newtypes                    |
| string / template literal      | StrLit        | `String`, its newtypes                         |
| `null`                         | NullLit       | any `Option<T>`                                |
| `[…]` sequence literal         | SeqLit        | tuples of that arity, sequence-coercible types |
| `{…}` anonymous struct literal | MapLit        | struct types, `TreeMap`-coercible types        |
| closure literal                | its `fn` type | parameters are annotated, so the type is exact |
| unresolvable expression        | Unknown       | every parameter (e.g. a nested ambiguous call) |

A literal class admits candidates; it never selects one. `f.take(42)` against
`Take<i32>` beside `Take<i64>` is an ambiguity error — the fix is `42 as i64`,
a binding with a type annotation, or a qualified call — because letting the
literal's default type decide would make adding an `impl Take<i32>` silently
retarget every existing call that meant `Take<i64>`. The common cases are
unaffected: `l[0]` still resolves, because an integer literal does not admit
the range-typed candidates; `f.take(B { v: 1 })` resolves, because a named
struct literal has a context-free type.

Probe results exist only to filter. After selection the winning signature
drives ordinary argument elaboration, so a literal still coerces to the chosen
parameter type, and value-range checks run there — selection never depends on a
literal's value.

### Qualified calls

One form names a candidate explicitly: trait-qualified (UFCS)
`Trait::method(receiver, args…)`. The `self` parameter becomes the first
argument, spelled to match its mode — `&x` for `&self`, `&mut x` for
`&mut self`, the value for `self`:

```wado
Display::fmt(&p, f);          // p implements two traits declaring `fmt`
Base::name(&x);               // supertrait diamond inside a generic body
Take::<A>::take(f, B { … });  // one trait's argument list, pinned
```

The receiver argument supplies `Self`, so the trait's own type arguments are
all a turbofish needs to carry. That covers both collision shapes: across
traits, the trait name discriminates; within one trait, the turbofish pins the
argument list. This closes the escape gap Super Traits documents. If the named
trait still has several argument lists for the receiver and no turbofish is
written, argument-directed selection applies within it.

No new grammar: `Take::<A>::take(f, x)` is the static-path shape
`List::<i32>::with_capacity(10)` already uses. What is new is only resolution —
for a static path `Head::name(args)`, `Head` resolves in the type namespace as
today (type → associated function; `interface` → effect operation), and a trait
head, today an `UnknownFunction` error, now makes it a UFCS call. The reflect
intrinsics (`ReflectStruct::<T>::members()`) keep their existing spelling,
where the turbofish names the reflected type instead — they are compiler
intrinsics whose traits declare no type parameters, so the two readings never
meet.

Rejected: `<Type as Trait<Args>>::method(args…)`. Not a cost-benefit call — the
syntactic position is taken. A leading `<` in expression position belongs to
JSX, reserved by
[Reactive Signals](./wep-2026-04-04-reactive-signals.md#jsx-integration)
(`return <button onclick={…}>`), so Rust's spelling is unavailable to Wado at
any price. It would also pin the receiver type, which is redundant wherever a
receiver argument exists. Only the trailing turbofish position (`::<`, after
`::`) stays free, which is what the form above uses.

Rejected: `Type^Trait::method` in source. `^` is the xor operator, so
`A ^ B::m(x)` already parses as an expression; the caret form stays what it is
— the internal mangled-name and
[symbol-notation](./wep-2026-06-14-symbol-notation.md) grammar for tooling,
docs, and diagnostics.

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

This is left open. The shape is narrow — `From` / `TryFrom` keep their own
path, and the prelude's `Default::default` / `FromStr::from_str` do not collide
with each other — so renaming is an acceptable answer for a case that has not
arisen. If it does, the follow-up must name `Self` without a receiver and
without a leading `<` (JSX owns that position): either binding `Self` from the
expected type (`let c: Config = Loadable::load(p)`, which contradicts this
WEP's forward-inference non-goal and so needs its own decision), or a turbofish
naming `Self` on the trait head.

### Diagnostics

Both ambiguity errors follow
[Diagnostic Reason Chains](./wep-2026-06-02-diagnostic-reason-chains.md): the
error names every surviving candidate in symbol notation, states per candidate
why it survived or was filtered ("argument 1 is an integer literal, admitted by
both `Take<i32>` and `Take<i64>`"; "argument 2 has type `String`, parameter is
`i32`"), and carries a fix-it: a qualified call for cross-trait collisions, an
`as` cast or annotation for literal-only distinctions. The no-survivor case
(candidates exist, all filtered) reports each candidate with the position that
rejected it instead of a bare "method not found".

### Interactions

| Feature           | Interaction                                                                                                                                                                                                                                                                                                                    |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Operators         | Indexing already conforms to this rule. Arithmetic is brought under it: `Add<X>` beside `Add<Y>` selects by RHS probe type instead of declaration order. `Eq` / `Ord` take no trait arguments — unaffected.                                                                                                                    |
| `From` / `?`      | Unchanged. `?`'s conversion is target-type-directed by design; `Type::from(x)`'s hint path resolves the argument fully (a literal's default type selects), which the general rule deliberately refuses. Unifying `from` onto the general rule is follow-up, gated on accepting that `i64::from(42)`-style calls become errors. |
| Default arguments | Owned by the trait declaration, identical across an overload set — no interaction with selection.                                                                                                                                                                                                                              |
| Effects           | Never considered by selection; the chosen method's `with` clause is checked as today.                                                                                                                                                                                                                                          |
| Coherence         | Untouched. Selection picks an argument list among impls coherence already accepts; overlapping impls of one instantiation remain errors.                                                                                                                                                                                       |
| Newtypes          | Inherited impls are candidates on the newtype receiver as today; the same grouping and selection apply.                                                                                                                                                                                                                        |
| Monomorphization  | Unaffected. The chosen trait's spelling lands in the mangled name, which `InstantiationKey` already discriminates on — the same shape the indexing and `From` paths produce today.                                                                                                                                             |
| LSP / tooling     | Hover and go-to-definition read the recorded dispatch fact; probe typing leaves no persistent state.                                                                                                                                                                                                                           |

### Non-goals

- Ad-hoc overloading of free functions or inherent methods — permanently out,
  per the design philosophy.
- Expected-type or return-type directed selection. Inference stays forward;
  `?` / `From` remains the one target-directed conversion, on its own path.
- Ranking. Beyond the existing specific-impls-win rule there is no preference
  order; resolution is unique-or-error, so adding a preference later would only
  turn errors into compiles (backward-compatible), while removing one never is.
- Trait-import scoping of candidates. Trait impls stay globally visible to
  method lookup, as today; scoping candidates by `use` is a separate question.

## Implementation

Landing order — each phase keeps the suite green and is useful alone:

1. Qualified calls: trait-head resolution in `resolve_static_method_call`
   (`method_call.rs:1116`) — where a trait head is today an `UnknownFunction`
   error (`method_call.rs:1168`) — binding the first argument as the receiver
   and recording an ordinary `MethodDispatch`. No parser change: both
   `Greet::greet(&p)` and `Take::<A>::take(&f, x)` already parse, and the AST
   retains the turbofish, so the trait's argument list is available to
   resolution. Pure addition; provides the escape hatch before any tightening.

   Static-call diagnostics already name the receiver in symbol notation
   (`static_call_symbol_name`), so a `Take<A>` / `Take<B>` pair renders as two
   distinct candidates rather than one repeated string. The ambiguity errors
   below build on that helper.

   Landed so far: `MethodCallInput::required_trait` constrains which impls may
   answer, filtered in `find_trait_method_for_type_inner` before
   `select_trait_match` sees the candidates (so naming a trait resolves what
   would otherwise be reported ambiguous), and `resolve_call` routes
   `Trait::method(recv, …)` to the method dispatcher ahead of its argument
   walk, matching how `T::method(...)` already branches. Annotate types such a
   call correctly. Reify does not yet replay it: the fact lands in
   `method_dispatch` under a `Call` node's `AstId`, and reify's `Call` arm
   looks for `static_method_dispatch`, so the call reaches later phases
   untyped. Closing that — a dispatch fact reify can replay for the UFCS shape
   — is what remains of this phase.

2. Cross-trait ambiguity on concrete receivers: `select_trait_match`
   (`method_lookup.rs:2515`) stops taking the first of two trait groups and
   reports the error, extending `report_trait_argument_ambiguity` beyond
   same-base-name survivors. The local-impls-first sort survives only as the
   tie-break among identical instantiations. Requires an audit of stdlib and
   fixtures for calls that today resolve through the silent preference — the
   blanket `Inspect` / `ReflectStruct` impls put same-named methods on every
   struct, so user traits reusing stdlib method names will surface here.
3. Argument-directed selection: probe typing, then the filter in
   `select_trait_match` (which becomes `&mut self` — it runs before arguments
   are resolved today). Only calls whose candidate set has several argument
   lists pay the probe cost. `find_method_in_trait_bounds`
   (`trait_query.rs:1610`) gets the same grouping so `T: Take<A> + Take<B>`
   behaves like the concrete case.

   A scratch `ModuleSemantics` is necessary but not sufficient — the swap
   already exists (`elaborator.rs:2054`, trait default-method synthesis) and
   the annotation maps are `AstId`-keyed, so re-resolving overwrites rather
   than duplicates. Four things sit outside those maps and a discarded probe
   corrupts each:

   - Diagnostics have no seam. `emit` calls `host.emit_diagnostic` directly
     (`elaborator.rs:263` → `logger.rs:121`) and bumps a counter that fails the
     whole compilation. A probe needs a depth counter gating `emit` / `warn`
     and leaving `error_count` untouched.
   - `FunctionContext` carries the Gap 7 walk-order invariant: annotate and
     reify must allocate the same synthetic locals in the same order. A probe
     that allocates one (`__ref_*` for a mut-capturing closure, `__qm_*` for
     `?`, `__b` for a coerced literal) and is discarded desyncs every later
     local index — silently wrong code, not an error. `FunctionContext` is not
     `Clone`, so this is explicit save/restore.
   - An anonymous struct literal interns into the shared `TypeTable` while its
     `pending_anonymous_structs` push lives on the scratch. Discarding the
     scratch leaves the dedup guard (`expr.rs:3859`) satisfied, so the real
     resolve registers nothing and no `TirStruct` is emitted. The existing swap
     drains that list back (`elaborator.rs:2088`); a probe must too, or skip
     anon-struct registration.
   - `record_bound_driven_synth_request` writes the shared `TypeTable` with no
     removal API, so a probe over-synthesizes.

   `TypeId` interning itself is safe — structurally deduped, and `retain`
   tolerates unreachable ids. Closures register nothing global at annotate
   time.

   This is the phase's real cost, and it argues for probing the narrowest
   expression that answers the question rather than running a general
   speculative resolve.
4. Follow-ups: arithmetic RHS selection in `find_arithmetic_trait_impl`;
   folding the `from` / `try_from` hint path into the general mechanism. That
   fold is what turns `Wrapper::from(42)` from the diagnostic it now reports
   into a resolved call — an integer literal admits `From<i64>` and not
   `From<String>`, so exactly one candidate survives. Until then the circular
   ordering stands and the error asks for `42 as i64`.

Test surface: extend `trait_ambiguous_argument_lists.wado` (resolvable variants
with a discriminating concrete argument), a cross-trait concrete-receiver
ambiguity fixture, qualified-call fixtures for each collision shape,
`trait_query.rs` cases proving `Base::name(&x)` resolves a diamond, and a guard
that `trait_argument_lists_operators_unaffected.wado` behavior is now the
general rule rather than an exception.

## Consequences

Benefits:

- The `Take<A>` / `Take<B>` pattern becomes callable — trait-argument APIs
  (unit-typed conversions, protocol selectors) no longer dead-end at the call
  site.
- Silent wrong dispatch disappears: the undiagnosed cross-trait first-wins is
  replaced by an error with an explicit escape.
- Supertrait diamonds and bound collisions get a fix that is not "rename the
  method".
- One principle covers what were three carve-outs (indexing, `From`,
  specific-impls-win) plus named calls: within one trait, types select; across
  traits, the user selects.

Trade-offs:

- Making the concrete-receiver collision an error is a breaking change where
  code relied on the locality preference; phase 2's audit and the phase-1
  escape hatch bound the migration.
- Probe typing elaborates ambiguous-call arguments twice. Only multi-argument-
  list candidate sets pay; single-candidate calls (the overwhelming majority)
  are untouched.
- Literal-only distinctions error where other languages would pick a default.
  This is deliberate — predictability over convenience — and the diagnostic
  carries the one-token fix.
- One new call form, and no new grammar for it — but `Trait::method(recv, …)`
  makes a static path's meaning depend on whether its head is a type or a
  trait. The two never collide (a name is one or the other), and Rust sets the
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
- [Conversion Traits](./wep-2026-03-16-conversion-traits.md) — the `From` hint
  path
- [Default Arguments](./wep-2026-04-11-default-arguments.md) — why arity never
  discriminates
- [Symbol Notation](./wep-2026-06-14-symbol-notation.md) — the `Type^Trait`
  spelling that stays tooling-only
