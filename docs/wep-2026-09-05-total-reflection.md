# WEP: Total Reflection — `TypeInfo` and `match type`

## Context

[Library-Defined Derivation over `Reflect*`](./wep-2026-06-13-reflect-derivation.md)
splits reflection into three planes, and only the first serves a subject the
author cannot name:

| Plane     | Carried by                                  | An unknown `T`                     |
| --------- | ------------------------------------------- | ---------------------------------- |
| Identity  | `Reflect` — `type_name`, `wire_name_policy` | answers                            |
| Structure | the kind sub-traits (`ReflectStruct`, …)    | needs the kind named in the bound  |
| Value     | the member bridges (`get` / `extract` / …)  | needs the payload types statically |

The value plane is inherently static: a bridge's result type is the member's,
so a walk that does not know the type cannot receive the value. The structure
plane is not — the compiler knows every type's kind — but it is reached only by
naming that kind in a bound, and a bound is written before the subject is known.

So a derivation writes one blanket per kind, which is what `Inspect` and
`core:serde` do. That works and has four costs:

- Only a trait can dispatch. A free function cannot branch on the kind, so
  reflection is unreachable from ordinary code.
- The framing common to every kind is written once per kind.
- There is no last-resort arm. A receiver the kinds do not admit produces no
  candidate rather than falling through, and a `T: Reflect` blanket written
  beside the kind ones reports at every receiver
  ([Trait Resolution](./wep-2026-09-01-trait-resolution.md), rank 3 and its
  Known gap).
- Each new kind rewrites every derivation. The tuple family is already queued.

### Prior art

- **Zig** — `switch (@typeInfo(T))` at comptime is the construct below. It
  checks the selected branch per instantiation, so an error in an unexercised
  branch surfaces at a distant call site. This WEP requires the opposite.
- **facet** (Rust) — one `SHAPE` const per type, matched on a `Def` / `Type`
  enum. Kind-agnostic and recursive, but metadata-only; it reaches values
  through raw pointers, which Wado has no counterpart for.
- **bevy_reflect** (Rust) — `reflect_kind()` and a `ReflectRef` enum over
  `dyn` handles. The kind query is this WEP's `TypeInfo`; the `dyn` half needs
  dynamic dispatch.
- **Rust specialization** (RFC 1210) and its stable-Rust stand-in, dispatching
  on an associated "kind" type. That stand-in is the alternative rejected
  below.

## Decision

### Every type carries `Reflect`

`Reflect` holds of every type, not of the five synthesized kinds alone. It is
the second such trait: `Inspect` already holds for all
(`solver_bridge.rs`, `TraitDef::holds_for_all`), so a total root introduces no
resolution shape the language does not have.

`TypeInfo` is what the root answers with, and it carries the classification
rather than sitting beside one. There is no separate kind enum: a type is
exactly one case, so the variant's own tag is the kind, and a second scalar
spelling of it would be the parallel classification
[Reflect Derivation](./wep-2026-06-13-reflect-derivation.md) refuses elsewhere.

```wado
internal trait Reflect {
    fn type_info() -> TypeInfo;
    fn type_name() -> String;
    fn wire_name_policy() -> CaseStyle;
}

/// Sealed: cases and fields minted only by `type_info()`, constructible
/// nowhere else, and not itself reflectable.
pub variant TypeInfo {
    Struct(DeclInfo),
    Variant(DeclInfo),
    Enum(DeclInfo),
    Flags(DeclInfo),
    Newtype(DeclInfo),
    Resource(DeclInfo),
    Array(DeclInfo),
    Primitive(PrimitiveKind),
    Unit,
    Never,
    Tuple(DeclInfo),
    Reference(RefInfo),
    Function(FnInfo),
}

impl TypeInfo {
    pub fn canonical_name(&self) -> String;    // symbol notation, every case
}

/// A declaration named with this instantiation's arguments.
pub struct DeclInfo { … }

impl DeclInfo {
    pub fn name(&self) -> String;              // "Pair"
    pub fn module(&self) -> String;            // "core:collections" / "./pair.wado"
    pub fn type_args(&self) -> List<TypeInfo>; // [String, i32]
}

pub struct RefInfo { … }

impl RefInfo {
    pub fn mutable(&self) -> bool;             // `&mut T`
    pub fn target(&self) -> TypeInfo;
}

pub struct FnInfo { … }

impl FnInfo {
    pub fn mutable(&self) -> bool;             // `fn mut(…)`
    pub fn params(&self) -> List<TypeInfo>;
    pub fn result(&self) -> TypeInfo;
    pub fn effects(&self) -> List<DeclInfo>;   // the `with E…` row: one interface each
    pub fn stores(&self) -> List<u32>;         // `stores[…]`, by parameter position
}

/// The Wasm primitives, at the granularity the type system has.
pub enum PrimitiveKind {
    I8, I16, I32, I64,
    U8, U16, U32, U64,
    F32, F64,
    Bool, Char, V128,
}
```

`TypeInfo` keeps the seal the earlier WEP put on it: a program reads a case and
mints none. A Wado case carries exactly one payload type, so a reference and a
function type, each holding several facts, take a sealed struct.

A function type is the whole signature or it is not the type: `fn mut(…)`
differs from `fn(…)`, and `with (Stdout, Stderr)` and `stores[data]` are row
members of the type the same way the parameters are. The effect row is where an
`interface` enters the model — it is a declaration Wado names, so it reads as a
`DeclInfo` like any other, and no separate identity is invented for it.

Every case is positive: no arm means "something the design has no answer for".
`TypeTable::reflect_kind` already computes the first five and answers `None`
for the rest, which is the classification this WEP completes:

| Type                                                      | Case        |
| --------------------------------------------------------- | ----------- |
| `struct` (anonymous included), `variant`, `enum`, `flags` | that case   |
| `String`, `i128`, `u128` — prelude structs                | `Struct`    |
| `type N = B`                                              | `Newtype`   |
| the four sealed member handles                            | `Struct`    |
| a resource, `Future<T>` / `Stream<T>`                     | `Resource`  |
| `Array<T>`                                                | `Array`     |
| `i8`…`u64`, `f32`, `f64`, `bool`, `char`, `v128`          | `Primitive` |
| `()`                                                      | `Unit`      |
| `!`                                                       | `Never`     |
| the tuple family                                          | `Tuple`     |
| `&T` / `&mut T`                                           | `Reference` |
| `fn(…) -> R with E…`                                      | `Function`  |

`()` is the unit type, not the empty tuple, and the two are held apart
everywhere else in the language: an impl for `[..T]` is never a candidate for it
([Trait Resolution](./wep-2026-09-01-trait-resolution.md)). So it carries its
own case rather than reading as a tuple of arity zero, and its declaration
`internal type ();` sits beside the primitives' in the prelude, with `!`'s
beside it.

A case carries a `DeclInfo` only where the declaration is not determined by the
case itself. `Primitive`, `Unit` and `Never` each name exactly one declaration
with no arguments, so the case is the identity and repeating it would be a
second spelling; the rest carry theirs.

The four member handles are `Struct` because the seal is on structure, not on
identity: nothing may enumerate their fields, and `match type` is where that
shows — they reach no `struct` arm. Naming them was never withheld.

A type parameter, an inference variable, a pack and an associated-type
projection carry no case: none survives monomorphization, which is where a
`Reflect` subject is concrete. `Reactive<T>` is the same — a reactive binding is
typed with its underlying value type, so the wrapper never reaches
monomorphize.

`Primitive` is named for what it holds and carries the kind at the granularity
the type system already has, so a consumer branches on `PrimitiveKind` rather
than on a name string. The granularity sits in the case's payload, not in the
case set, so `match type`'s arms are unaffected by it: one `primitive` arm, an
ordinary `match` inside where a body cares. That placement is also why the kind
is carried now rather than deferred — widening a case's payload later breaks
every pattern already written against it, so "add it when someone needs it" is
not the free option it looks like.

`String`, `i128` and `u128` read as primitives to a programmer and are none:
each is a prelude struct — `i128` and `u128` are `#[compiler_item]` structs of
two 64-bit limbs — with private fields, so they classify as `Struct` and, having
no visible members downstream, cannot be opened there either.

That they land in `opaque` rather than beside `i32` leaves nothing unanswered.
The coarser question, leaf or aggregate, is computed from these cases and never
the reverse, so this split is the superset: one leaf case would lose the
difference between `i32` and a struct the site may not open, which `type_info()`
still reports as `Struct`. Nor does any consumer branch on the coarse question,
because reflection offers one action per side and carries only one. An aggregate
is walked through the kind bound an arm proves; reading a leaf's value needs a
bound reflection never has, like `Display` or `Serialize`. So `primitive` and
`opaque` admit the same body, and a predicate telling them apart would serve no
branch.

A reference and a function type keep their components rather than rendering them
away, which a flat "name + module + args" shape could not do. Totality is what
makes that worth having: `type_args()` returns `List<TypeInfo>`, so a case with
no answer would appear inside a tree a consumer is already walking, not at its
root — `Pair<&Point>` and `struct Handler { cb: fn(i32) -> i32 }` each put one
there.

The visibility gate is unchanged: it sits on the kind traits and never on the
root ([Reflect Derivation](./wep-2026-06-13-reflect-derivation.md),
Visibility). A total root is what that split already implied — a type's name is
public the moment the type is.

### Symbol notation names a reference and a function type

[Symbol Notation](./wep-2026-06-14-symbol-notation.md) is `MODULE#SYMBOL`, and
`canonical_name()` renders in its canonical register. Most cases already have a
module. A primitive, `()` and `!` are `internal type` declarations in
`core:prelude` and resolve like any other name (`prelude/primitive.wado`,
`trait_env.rs`'s `ImplTargetKey`); `Array<T>` is the prelude's
`pub type Array<T>;`; and the tuple family is its `internal type [..T];`, whose
arguments are the element types, the family-plus-arguments split a generic
struct has.

A reference and a function type have no declaration at all. That is a gap in the
notation rather than a reason for reflection to leave them unnamed: their module
is `core:prelude` and their symbol is the type's own surface spelling.

```text
core:prelude#i32
core:prelude#()
core:prelude#!
core:prelude#Array<i32>
core:prelude#&Point
core:prelude#&mut Point
core:prelude#fn(i32) -> i32
core:prelude#[i32,String]
```

This is the rule the notation already follows one level down: a type argument
renders as its surface spelling (`core:prelude#List<String>`), not as a nested
`MODULE#SYMBOL`. A structural type is the same shape with the operator
outermost.

That rendering serves a reader — a diagnostic, a dump, a doc anchor — and is not
a key. `&Point` renders the pointee bare, so two `Point` declarations in
different modules produce one string. A registry or a `$defs` map keys on the
`TypeInfo` itself, which compares structurally over module, name and arguments
and tells them apart.

The identity a `DeclInfo` carries is the Wado module symbol, for every case
alike. A resource and an interface each have a second one, the Component Model
coordinate (`wasi:io/streams.input-stream`), and reflection carries neither it
nor a branch for the cases that have one. The friction is real and accepted: a
consumer keying a registry by CM coordinate cannot get there from `TypeInfo`,
and a WASI type reads by the name its generated Wado module gives it. One
identity of one shape is worth more than a second one only some types have.

### A `Shape` tree belongs to Jade

A schema library ultimately reads a facet-style metadata tree: kind-agnostic,
walking an unknown type to arbitrary depth through lazy `fn() -> Shape` edges.
It is deliberately not here. Written over `match type` it is ordinary library
code in `wado:jade`; synthesized in the compiler it would be a per-type metadata
list beside `members()`, which
[Reflect Derivation](./wep-2026-06-13-reflect-derivation.md) refuses.

### `match type` — narrowing a subject to its kind

A bound states a kind before the subject is known. The body states it after:

```wado
fn describe<T: Reflect>(v: &T) -> String {
    match type T {
        struct(FieldTypes = [..F]) => return fields_of(v),
        variant(CasePayloads = [..P]) => return live_case_of(v),
        enum | flags => return Reflect::<T>::type_name(),
        newtype(Base = B) => return describe(&B::from(*v)),
        resource | array | primitive | unit | never | tuple | reference
            | function | opaque => return Reflect::<T>::type_name(),
    }
}
```

A kind arm proves its kind's bound for its own body. `struct =>` elaborates
under the hypothesis `T: ReflectStruct`, so `ReflectStruct::<T>::members()`
resolves inside it and nowhere else.

Only the five kind traits have a hypothesis to push. The other arms name a
classification with no trait behind it, so they add nothing to what `T: Reflect`
already gives, and neither do they bind: an `array` arm cannot name its element
type today (Known gaps). An alternation pushes what all its kinds share, which
is nothing beyond the root, so `enum | flags` is how one body serves two kinds
and `struct | variant` buys nothing over `opaque`.

A `match type` is exhaustive and carries no `_`. Every case of `TypeInfo` is a
kind of the type system itself, so a set that closes over them closes over
everything a subject can be, and a wildcard would mostly hide the one thing
worth reporting: a body that forgot a kind. The cost is that adding a kind to
the language breaks every `match type` in the ecosystem — which is what adding
a kind is.

`opaque` is the arm exhaustiveness forces into existence, and it is not `_`
renamed. A kind arm carries a hypothesis its body relies on, so a struct the
site may not open — one with private members, or one of the sealed member
handles — cannot enter `struct` without making that hypothesis false. It enters
`opaque` instead, a named condition meaning "declared, not openable here", where
`type_info()` still reports what the type is. A future kind does not land there;
it fails exhaustiveness, as intended.

Three rules make it more than sugar over the five blankets:

Arms are checked once, at the definition, under the arm's own hypothesis, never
per instantiation. This is the requirement Zig's comptime switch does not meet,
and it keeps a diagnostic on the code that is wrong rather than on a caller that
happens to reach it.

The unselected arms are dropped at monomorphization, before substitution. The
compiler has the shape for this: `VariadicForOf` is a TIR node elaborated once
generically and expanded against the concrete pack in `monomorphize/func_inst.rs`,
and a `match type` node is expanded the same way against the concrete subject.
The call it leaves behind is already a solved problem too — a
`Reflect*` static call on a type parameter records a dispatch fact
(`is_type_param_receiver`) that monomorphization redirects to the concrete
type's synthesized impl, which is how `fn root_name_of<T: Reflect>()` works
today (`reflect_root_type_name.wado`).

An arm binds what its kind's trait associates. The pack cannot be left to a
helper function's header: a call site projects `[..F]` from the _concrete_
subject (`variadic_free_fn_assoc_pack.wado` — the reflection impls are
registered by a synthesis phase that runs after elaboration, so the projection
computes rather than reads), and inside an arm the subject is still rigid. So
the arm itself binds it, and the bound it pushes carries the association the
projection reads:

```wado
match type T {
    struct(FieldTypes = [..F]) => …,      // T: ReflectStruct<FieldTypes = [..F]>
    variant(CasePayloads = [..P]) => …,   // T: ReflectVariant<CasePayloads = [..P]>
    newtype(Base = B) => …,               // T: ReflectNewtype<Base = B>
    enum => …,                            // binds nothing; T: ReflectEnum
}
```

The binder is spelled as an impl header spells it, association named, so a
derivation moved from a blanket into an arm reads the same in both places. It
binds a plain associated type (`Base = B`) the same way it binds a pack. An arm
that reads neither writes neither, so a kind-only branch pays nothing for what
it never touches. And an arm binds without bounding: `..F: Serialize` belongs to
the header of the function the arm delegates to, which keeps the arm about
proving the kind. Both are revisitable, and a derivation that turns out to need
the bound on the arm is the reason to revisit.

The mechanism underneath is unchanged by the spelling: bounds in force are
name-keyed in `annotate_ctx.trait_ctx.type_param_bounds`, which
`reflect_pack_bound_ty` reads, so an arm pushes a synthesized bound onto that
map under RAII and every existing projection path serves it unchanged.

### What each construct answers

The two answer different questions, and the difference is load-bearing rather
than an inconsistency to remove:

- `type_info()` — what the type is. A fact about the declaration, the same from
  everywhere it is asked.
- `match type` — what the type may be opened as. Gated on visibility and on the
  member seal, so a struct whose members are private elsewhere, and a member
  handle anywhere, reach no `struct` arm.

A consumer composes the two: in the fallback arm `type_info()` still answers
`Struct`, so "a struct this module may not enumerate" is expressible. Neither
construct states it alone.

### Rejected: dispatching on an associated kind type

`Reflect` could carry `type Kind` (a marker per kind) and a derivation could
delegate to a `KindOps<T, T::Kind>` whose per-kind impls carry the kind bounds.
This is the stand-in Rust reaches for while specialization is unstable. It needs
no new syntax, and one implication rule (`Reflect<Kind = StructKind>` ⟹
`ReflectStruct`) would carry it.

It is rejected because the scaffolding is per derivation: every consumer writes
a dispatcher trait and five impls to reach one branch, which is the cost this
WEP exists to remove, and the entry blanket it forces (`impl<T: Reflect> Tr for T`)
lands on the specificity gap rather than avoiding it.

## Consequences

A derivation collapses to one blanket over the root. `Inspect` is the
measurement: its per-kind blankets become arms of one body, and
`mise run report-wasm-size` must not regress — the same test proves the
unselected arms are dropped rather than emitted.

`T: Reflect` becomes a bound that admits everything, which makes it a condition
in name only. That is `Inspect`'s existing status, so nothing new is unsound;
what follows is that two total bounds on one trait tie at rank 3 like any other
pair, and a derivation keyed on the root cannot also be keyed on `Inspect`.

The LSP path stops after `liveness` and builds no TIR
([Elaborator Architecture](./wep-2026-05-26-elaborator-rearchitecture.md)), so
it never monomorphizes and every arm stays live in the editor. Hover inside an
arm reports the hypothesis, not a selected instance.

Minting `Reflect` for the two declaration-less shapes is new work. Today the
impls are synthesized per TIR declaration in `synthesis/traits.rs`, which a
primitive and the tuple family reach like any other prelude type; a reference
and a function type have no declaration, so their root methods are minted off a
`TypeId` instead.

A `TypeInfo` is a tree where a kind enum would have been a scalar, and it costs
nothing at a use site: it is a closed constant expression, so
[Constant Object Globalization](./wep-2026-05-31-const-object-globalization.md)
hoists it to a global and the call reads from there — the same treatment
`members()` gets. Asking only for the case is therefore a load, not an
allocation, which is what makes a separate scalar query unnecessary rather than
merely redundant. `type_name()` stays beside it as the allocation-free
shorthand serde's `begin_struct` calls.

The value plane for an unknown type is unchanged and stays out of reflection: a
uniform walk over an unknown _value_ is `core:value::Value` through
`Serialize`, and reflection answers for the _type_.

## Known gaps

### The structural notation is unwritten and one-way

`core:prelude#&Point` and `core:prelude#fn(i32) -> i32` are decided above and
implemented nowhere: `symbol_notation` parses and renders declaration symbols
only. Rendering is what `canonical_name()` needs, and it comes first; resolving
one back is the harder half, since a structural type has no `AstId` for
`wado query` to land on and the notation "runs both ways" today.

- [ ] Render every `TypeInfo` case in `symbol_notation`.
- [ ] Decide what `wado query "core:prelude#&Point"` answers — the target's
      declaration, a synthesized view, or a diagnostic naming the limit.

### An arm with no trait behind it binds nothing

The five kind traits give an arm its hypothesis and its associations. The other
arms have neither, so an `array` arm cannot name its element type and a
`function` arm cannot name a parameter's — a body that needs one reads
`type_info()` and gets a value, not a type it can call a bound method on.

- [ ] Decide whether these arms bind (a second binder form, over the case's own
      components) or stay value-only.

### An anonymous struct has no declaration to report

An anonymous struct classifies as `Struct`, and `DeclInfo` reports a
declaration. It has none: the compiler keys it as `Undeclared(module, rendering)`
(`trait_env.rs`), where the module is the one that wrote the literal and the
name is the shape's rendering. Two literals of one shape are one type on
purpose, so the pair is a sound identity — but it is not a declaration, and
nothing states how it renders in symbol notation.

- [ ] State what `name()` and `module()` answer for an anonymous struct, and how
      `canonical_name()` spells one.

### The type/value split is unwritten

Reflection answers for the type and `core:value::Value` for the value; that
boundary is stated here and nowhere a reader of the language would look.

- [ ] State it in `docs/spec.md`, so it is not rediscovered as a missing
      reflection feature.

## Related WEPs

- [Library-Defined Derivation over `Reflect*`](./wep-2026-06-13-reflect-derivation.md) — the three planes and the traits this WEP reaches
- [Variadic Type Parameters](./wep-2026-03-14-variadic-type-parameters.md) — the packs an arm binds, and `VariadicForOf`
- [Trait Resolution](./wep-2026-09-01-trait-resolution.md) — why a root-bounded blanket beside the kind ones is rank 3, and why `()` is not a tuple
- [Struct Walkability](./wep-2026-07-10-struct-walkability.md) — the visibility gate an arm inherits
- [Symbol Notation](./wep-2026-06-14-symbol-notation.md) — the register `canonical_name()` renders, widened here to the declaration-less shapes
- [Jade](./wep-2026-06-13-jade.md) — where the `Shape` tree is written
- [Elaborator Architecture](./wep-2026-05-26-elaborator-rearchitecture.md) — where an arm's hypothesis and its expansion live
