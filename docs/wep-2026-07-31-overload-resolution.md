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

Arithmetic operators cannot exhibit the collision at all today: the prelude's
operator traits take no type parameters (`trait Add { type Output; fn add(&self,
rhs: &Self) … }`), so `impl Add<X>` is not writable and an operator trait has
exactly one argument list. RHS-directed selection there is gated on
parameterizing the operator traits (`trait Add<Rhs = Self>`), a stdlib redesign
outside this WEP; `find_arithmetic_trait_impl`'s first-impl scan is safe until
then because coherence permits only one impl.

The static path was not merely name-only, it was circular. `Type::m(args)`
elaborated its arguments against parameter types that
`lookup_static_method_param_types_keyed` finds by (receiver, method) name —
`static_method_index` is searched with `find`, so with two conversion impls it
returns whichever comes first — and _then_ selected the impl from the
resulting argument type. Selection shaped the argument, and the argument drove
selection; where the two disagreed, no impl matched (`Wrapper::from(42)`
against `From<String>` beside `From<i64>` typed the literal `i32` and matched
neither — an ICE at WIR build). Argument-directed selection is what removes
the circularity, not an extra feature layered on it.

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
shape, including the concrete-receiver case that used to resolve silently. The
rationale is twofold: impls of different traits share no contract, so a
type-directed pick is a semantic guess; and allowing it would reintroduce ad-hoc
overloading through one-method traits, which the design philosophy rules out.
The escape is the qualified call syntax below, not renaming. One tie-break
before the error: when the colliding declarations share a bare name and exactly
one is in scope at the call site (declared or imported by the calling module),
that one is selected — a same-named foreign trait a module never imported does
not break its calls, which is what keeps two libraries' private `trait Loud`
from colliding through a shared receiver type.

One exception: a blanket candidate does not count toward the collision. Wado
does not scope method candidates by which traits are imported, so a foreign
`impl<T: Bound> Foreign for T` reaches every receiver in the program — counting
it would make adding a blanket impl to a library a breaking change for every
downstream method of that name, which is the cost Rust pays with trait-import
scoping instead. A blanket is the general case and loses to any impl written
for the receiver: the same specific-impls-win ordering the language already
applies within one trait, read across traits. Two blankets colliding with each
other still resolve by the existing order rather than erroring; if that turns
out to bite, the answer is candidate scoping, not counting blankets here.

### Resolution algorithm

For the trait-impl step of method lookup (inherent methods still shadow trait
methods; the ref-impl priority and the earlier steps are unchanged):

1. Collect the candidate impls providing `m` for the receiver, as today
   (impl-index buckets along the newtype chain, plus satisfied blankets; for a
   generic receiver, the transitive closure of its bounds).
2. Group candidates by trait identity. Identity is structural, never a
   spelling: the trait is its declaration key (declaring module + declared
   name), so two modules' same-named traits stay distinct and an alias
   compares equal to the original; an argument list is its resolved types, so
   `Take<Alias>` and `Take<A>` name one list. Each candidate's identity is
   resolved in its impl's own frame — the impl module's imports, with the
   impl's bound type parameters substituted.
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

Selection needs argument types before any signature is chosen, so arguments
are probe-typed. The probe is a shallow, side-effect-free scan
(`probe_arg_class`), deliberately not a speculative `resolve_expr`: a full
speculative resolve would desync `FunctionContext`'s local-index walk with
reify, leak anonymous structs into the shared `TypeTable`, and emit
diagnostics through a channel with no suppression seam. The scan reads scopes
and the type table and mutates nothing. Expressions whose types are pinned at
the call site — a local (scope lookup), a named non-generic struct literal, a
cast to a named type, `&`/`&mut` of those, bool and char literals — produce
their type. Literals produce a class, not a type:

| Argument                  | Probe result | Admissible parameters                 |
| ------------------------- | ------------ | ------------------------------------- |
| integer literal           | IntLit       | integer types, floats, their newtypes |
| float literal             | FloatLit     | float types, their newtypes           |
| string / template literal | StrLit       | `String`, its newtypes                |
| `null`                    | NullLit      | any `Option<T>`                       |
| everything else           | Admit        | every parameter                       |

"Everything else" — compound literals, closures, calls, field reads — admits
every candidate. The approximation direction is the invariant: over-admitting
can only leave the set ambiguous (an error asking the user to disambiguate),
while under-admitting could select the wrong candidate. Sharpening a class —
teaching the scan field reads or call return types — is therefore always a
backward-compatible refinement: it can only turn errors into resolutions.

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
`&mut self`, the value for `self`. A mismatched mode is an error, not a
coercion: passing a value where the method takes `&mut self` would mutate a
copy and silently drop the change. The one exception is the language's one
reference coercion — `&mut x` also answers a `&self` method:

```wado
Display::fmt(&p, f);          // p implements two traits declaring `fmt`
Base::name(&x);               // supertrait diamond inside a generic body
Take::<A>::take(f, B { … });  // one trait's argument list, pinned
```

The receiver argument supplies `Self`, so the trait's own type arguments are
all a turbofish needs to carry. That covers both collision shapes: across
traits, the trait name discriminates; within one trait, the turbofish pins the
argument list. The head and the turbofish arguments resolve to identities
(declaration key, resolved types) before filtering, so aliases and same-named
foreign traits behave like everywhere else — and only the named declaration
may answer: the auto-derived `Eq` / `Ord` bodies answer a qualified call only
when it names them, never a user trait that happens to share the method name. This closes the escape gap Super Traits documents. If the named
trait still has several argument lists for the receiver and no turbofish is
written, argument-directed selection applies within it.

No new grammar: `Take::<A>::take(f, x)` is the static-path shape
`List::<i32>::with_capacity(10)` already uses. Only resolution differs — for a
static path `Head::name(args)`, `Head` resolves in the type namespace (type →
associated function; `interface` → effect operation), and a trait head makes
it a UFCS call. The reflect
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

| Feature           | Interaction                                                                                                                                                                                                                                                                                                                                           |
| ----------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Operators         | Indexing already conforms to this rule. Arithmetic cannot collide yet — the prelude's operator traits take no type parameters, so RHS selection is gated on parameterizing them (`trait Add<Rhs = Self>`). `Eq` / `Ord` take no trait arguments — unaffected.                                                                                         |
| `From` / `?`      | `?`'s conversion stays target-type-directed by design. `Type::from(x)` preselects by the literal's class before the argument is elaborated, independent of impl declaration order: one admitted impl supplies the expected type, several admitted impls are an error asking for a cast, and a non-literal argument selects through its resolved type. |
| Default arguments | Owned by the trait declaration, identical across an overload set — no interaction with selection.                                                                                                                                                                                                                                                     |
| Effects           | Never considered by selection; the chosen method's `with` clause is checked as today.                                                                                                                                                                                                                                                                 |
| Coherence         | Untouched. Selection picks an argument list among impls coherence already accepts; overlapping impls of one instantiation remain errors.                                                                                                                                                                                                              |
| Newtypes          | Inherited impls are candidates on the newtype receiver as today; the same grouping and selection apply.                                                                                                                                                                                                                                               |
| Monomorphization  | Unaffected. The chosen trait's spelling lands in the mangled name, which `InstantiationKey` already discriminates on — the same shape the indexing and `From` paths produce today.                                                                                                                                                                    |
| LSP / tooling     | Hover and go-to-definition read the recorded dispatch fact; probe typing leaves no persistent state.                                                                                                                                                                                                                                                  |

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

Four phases, each independently useful — the escape hatch landed before any
tightening:

1. Qualified calls: trait-head resolution in `resolve_static_method_call` —
   the branch where a head resolving to no type is otherwise an
   `UnknownFunction` error — binding the first argument as the receiver. No
   parser change: both `Greet::greet(&p)` and `Take::<A>::take(&f, x)`
   already parse, and the AST retains the turbofish, so the trait's argument
   list is available to resolution.

   Static-call diagnostics already name the receiver in symbol notation
   (`static_call_symbol_name`), so a `Take<A>` / `Take<B>` pair renders as two
   distinct candidates rather than one repeated string. The ambiguity errors
   below build on that helper.

   `MethodCallInput::required_trait` constrains which impls may answer,
   filtered in `find_trait_method_for_type_inner` before `select_trait_match`
   sees the candidates — so naming a trait resolves what would otherwise be
   reported ambiguous — and `resolve_call` routes `Trait::method(recv, …)` to
   the method dispatcher ahead of its argument walk, matching how
   `T::method(...)` already branches.

   The decision is filed as a _static_ dispatch, not a method dispatch: a
   qualified call spells its receiver's mode itself (`&x` for `&self`), so no
   receiver adjustment is owed and the call is an ordinary one whose first
   argument happens to be the receiver. That is the shape reify's `Call` arm
   already replays, and it is why the form needs no reify work of its own —
   `method_dispatch` would have been the wrong drawer, since reify reads it
   only for a `MethodCallExpr` node.

   Two record-keeping consequences of that shape, both load-bearing: the
   static record must carry the resolved signature's real facts (defaults,
   `is_mut`, parameter types — the receiver prepended as slot 0), not
   fabricated ones; and every downstream pass that positions a `Call`'s
   arguments against its callee's parameters must account for the receiver in
   `args[0]`. Fn-param closure specialization was the pass caught by the
   latter: its keys are value-argument indices, and the `Call` collector's
   full-list zip recorded parameter slots instead, so the specialized clone's
   rewrite shifted past the last parameter and silently no-op'd — the clone
   kept its `fn`-typed param and its canonical cast trapped on the functor the
   rewritten call site passes. Both call spellings produce the same key,
   which is also what keeps one specialized clone per callee instead of two
   same-named ones.

2. Cross-trait ambiguity on concrete receivers: `report_cross_trait_ambiguity`
   fires when the non-blanket survivors span more than one trait, reusing the
   bounds path's `AmbiguousTraitMethod` so the shape a collision arrives in
   does not change the answer. Selection itself is untouched — the check
   decides only what is reported, which keeps the change's blast radius to the
   set of programs that error.

   Two shapes fall under it. `x.fmt(&mut f)` on a primitive is a real
   collision (`Display`, `Binary`, `Octal`, `LowerHex`, `UpperHex` all declare
   `fmt`) — spelled `Display::fmt(&x, &mut f)`, the migration this phase
   exists to enable. A foreign blanket beside a local impl is not a
   collision, which is what the blanket exception above records.

3. Argument-directed selection: `probe_arg_class` runs per argument before
   lookup (cheap, side-effect-free), and `select_trait_match` filters a
   homogeneous-base candidate set by admission — exactly one admitted
   candidate wins; zero or several fall through to the ambiguity report,
   whose message names the two escapes (`42 as i64`, the trait turbofish).
   The turbofish spelling `Take::<A>::take(recv, …)` routes through the same
   qualified-call engine with the full trait spelling as the constraint.
   The bounds counterpart (`T: Take<A> + Take<B>`) cannot arise yet —
   positional trait arguments do not parse in bound position.

   The shallow scan is the load-bearing choice, not a stopgap. A speculative
   `resolve_expr` against a scratch `ModuleSemantics` would still corrupt four
   things that live outside the `AstId`-keyed annotation maps: diagnostics
   (`emit` reaches `host.emit_diagnostic` directly, with no suppression seam,
   and bumps a counter that fails the compilation), the `FunctionContext`
   local-index walk that reify replays in lockstep (a discarded synthetic
   local — `__ref_*`, `__qm_*`, `__b` — desyncs every later index into
   silently wrong code), the anonymous-struct dedup guard in the shared
   `TypeTable` (a probed literal would satisfy it and the real resolve would
   register nothing), and `record_bound_driven_synth_request` (no removal
   API). Any future sharpening of the probe must stay inside the
   side-effect-free scan; upgrading it to a real resolve re-opens all four.

4. Conversion fold: `conversion_preselect` runs the literal's probe class
   over the receiver's conversion impls _before_ the argument is elaborated —
   removing the circular ordering at its root. Admissibility is
   `probe_admits` over each impl's source type _resolved in the impl's own
   frame_ (`conversion_impl_survey`), the same table argument-directed
   selection uses — so an integer newtype admits an integer literal here
   exactly as it does there, and `From<i64>` beside `From<Meters>` is
   ambiguous rather than silently primitive. One admitted impl supplies the
   argument's expected type; several report `AmbiguousConversionArgument`,
   whose fix is the cast — `from` has no `self`, so the trait-turbofish
   escape cannot apply. A non-literal argument passes through: its resolved
   type selects via the name hint, whose matcher compares the head
   (un-aliased in the impl's module) and, for a generic argument spelling,
   the full spelling with whitespace ignored — so same-head impls
   (`From<List<i32>>` beside `From<List<String>>`) are told apart; nested
   aliasing that changes the rendered arguments is the name-based
   mechanism's remaining ceiling, with full `TypeId` matching the eventual
   replacement. Two shapes stay carved out at the gate rather than guessed
   at: an inherent static `from` beside `From` impls answers on the
   trait-less path, and a conversion reachable only through a blanket
   generic in its source type (`impl<T: Display> From<T> for Wrapper`) is
   rejected with its own diagnostic — it has never compiled, and selecting
   its instantiation needs generic-impl monomorphization, not name matching.

   Remaining follow-up: operator-trait parameterization
   (`trait Add<Rhs = Self>`), which is what would make RHS-directed operator
   selection expressible in the first place.

Test surface: the `ufcs_*`, `trait_argument_*`, `from_overload_*`, and
`cross_module_same_name_*` fixture families in `wado-compiler/tests/fixtures/`
— one fixture per rule above (selection, each ambiguity shape, each escape,
the scope tie-break, the receiver-mode errors), plus `error_*` fixtures
pinning every diagnostic's message.

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
