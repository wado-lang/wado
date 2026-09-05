# WEP: Reflection over an Unknown Type — `TypeInfo` and `match type`

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
- There is no last-resort arm. A receiver the kinds do not admit — members not
  visible here, or a type outside the five kinds — produces no candidate rather
  than falling through, and a `T: Reflect` blanket written beside the kind ones
  reports at every receiver ([Trait Resolution](./wep-2026-09-01-trait-resolution.md),
  rank 3 and its Known gap).
- Each new kind rewrites every derivation. The tuple family is already queued.

`Reflect` alone therefore answers a name and nothing else, and the friction is
not that the value plane is static — it is that the structure plane is gated on
a name the author does not have.

### Prior art

- **Zig** — `switch (@typeInfo(T))` at comptime is the same shape as the
  construct below, with one difference this WEP takes as a requirement: Zig
  checks the selected branch per instantiation, so an error in an unexercised
  branch surfaces at a call site far from the code that is wrong.
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

/// A declaration named with this instantiation's arguments.
pub struct DeclInfo { … }

impl DeclInfo {
    pub fn name(&self) -> String;              // "Pair"
    pub fn module(&self) -> String;            // "core:collections" / "./pair.wado"
    pub fn type_args(&self) -> List<TypeInfo>; // [String, i32]
}

pub variant TypeInfo {
    Struct(DeclInfo),
    Variant(DeclInfo),
    Enum(DeclInfo),
    Flags(DeclInfo),
    Newtype(DeclInfo),
    Resource(DeclInfo),
    Tuple(DeclInfo),
    Scalar(DeclInfo),
    Array(TypeInfo),
    Reference { mutable: bool, target: TypeInfo },
    Function {
        mutable: bool,                 // `fn mut(…)`
        params: List<TypeInfo>,
        result: TypeInfo,
        effects: List<DeclInfo>,       // the `with E…` row: one interface each
        stores: List<i32>,             // `stores[…]`, by parameter position
    },
    Never,
}

impl TypeInfo {
    pub fn canonical_name(&self) -> String;    // symbol notation, every case
}
```

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
| the tuple family, `()`                                    | `Tuple`     |
| `i8`…`u64`, `f32`, `f64`, `bool`, `char`, `v128`          | `Scalar`    |
| `Array<T>`                                                | `Array`     |
| `&T` / `&mut T`                                           | `Reference` |
| `fn(…) -> R with E…`                                      | `Function`  |
| `!`                                                       | `Never`     |

The four member handles are `Struct` because the seal is on structure, not on
identity: nothing may enumerate their fields, and `match type` is where that
shows — they reach no `struct` arm. Naming them was never withheld.

A type parameter, an inference variable, a pack and an associated-type
projection carry no case: none survives monomorphization, which is where a
`Reflect` subject is concrete. `Reactive<T>` is the same — a reactive binding is
typed with its underlying value type, so the wrapper never reaches
monomorphize.

`Scalar` names the Wasm primitives, which is a fact about representation rather
than about being a leaf. `String`, `i128` and `u128` read as scalars to a
programmer and are prelude structs with private fields, so they land in
`Struct` and, having no visible members downstream, cannot be opened there
either. That gap between what `Scalar` covers and what a consumer means by
"leaf" is recorded below.

Within what it does cover, `Scalar` is deliberately coarse: nothing yet reads a
primitive's width or signedness through reflection, and a consumer that needs
one writes the per-type impl it already writes. Splitting it later is additive
only as a sub-classification, which is the constraint any refinement inherits.

A structural case keeps its components rather than rendering them away, which
is why the flat "name + module + args" shape the earlier WEP sketched does not
serve. `type_args()` returns `List<TypeInfo>`, so a type with no answer is not
contained at the root — `Pair<&Point>` and `struct Handler { cb: fn(i32) -> i32 }`
put one inside a tree a consumer is already walking. Totality is what keeps
that tree readable end to end.

The visibility gate is unchanged: it sits on the kind traits and never on the
root ([Reflect Derivation](./wep-2026-06-13-reflect-derivation.md),
Visibility). A total root is what that split already implied — a type's name is
public the moment the type is.

### Symbol notation names a structural type too

[Symbol Notation](./wep-2026-06-14-symbol-notation.md) is `MODULE#SYMBOL`, and
`canonical_name()` renders in its canonical register. A reference, a function
type and a primitive have no module of their own, which is a gap in the
notation rather than a reason for reflection to leave them unnamed: the module
is `core:prelude`, and the symbol is the type's own surface spelling.

```text
core:prelude#i32
core:prelude#&Point
core:prelude#&mut Point
core:prelude#fn(i32) -> i32
core:prelude#[i32,String]
core:prelude#()
core:prelude#!
```

This is the rule the notation already follows one level down: a type argument
renders as its surface spelling (`core:collections#List<String>`), not as a
nested `MODULE#SYMBOL`. A structural type is the same shape with the operator
outermost, and the tuple family's anchor — `core:prelude#[i32,String]`, the
prelude's `pub type [...T];` — was already decided this way.

### `match type` — narrowing a subject to its kind

A bound states a kind before the subject is known. The body states it after:

```wado
fn describe<T: Reflect>(v: &T) -> String {
    match type T {
        struct([..F]) => return fields_of(v),
        variant([..P]) => return live_case_of(v),
        enum | flags => return Reflect::<T>::type_name(),
        newtype => return describe(&B::from(*v)),
        tuple | scalar | array | reference | function | resource | never | opaque
            => return Reflect::<T>::type_name(),
    }
}
```

Each arm proves its kind's bound for its own body. `struct =>` elaborates under
the hypothesis `T: ReflectStruct`, so `ReflectStruct::<T>::members()` resolves
inside it and nowhere else.

A `match type` is exhaustive and carries no `_`. Every case of `TypeInfo` is a
kind of the type system itself, so a set that closes over them closes over
everything a subject can be, and a wildcard would mostly hide the one thing
worth reporting: a body that forgot a kind. The cost is that adding a kind to
the language breaks every `match type` in the ecosystem — which is what adding
a kind is.

`opaque` is the arm exhaustiveness forces into existence, and it is not `_`
renamed. The other arms carry a hypothesis their body relies on, so a subject
that is a struct the site may not open — private members, or one of the sealed
member handles — cannot enter `struct` without making that hypothesis false. It
enters `opaque` instead: a named condition ("declared, not openable here"),
where `type_info()` still reports what the type is. A future kind does not land
there; it fails exhaustiveness, as intended.

Three rules make it more than sugar over the five blankets:

Arms are checked once, at the definition, under the arm's own hypothesis —
never per instantiation. This is the requirement Zig's comptime switch does not
meet, and it is what keeps a diagnostic on the code that is wrong rather than
on a caller that happens to reach it.

The unselected arms are dropped at monomorphization, before substitution. The
compiler has the shape for this: `VariadicForOf` is a TIR node elaborated once
generically and expanded against the concrete pack in `monomorphize/func_inst.rs`,
and a `match type` node is expanded the same way against the concrete subject.
The call it leaves behind is already a solved problem too — a
`Reflect*` static call on a type parameter records a dispatch fact
(`is_type_param_receiver`) that monomorphization redirects to the concrete
type's synthesized impl, which is how `fn root_name_of<T: Reflect>()` works
today (`reflect_root_type_name.wado`).

An arm binds the payload pack its kind carries. The pack cannot be left to a
helper function's header: a call site projects `[..F]` from the _concrete_
subject (`variadic_free_fn_assoc_pack.wado` — the reflection impls are
registered by a synthesis phase that runs after elaboration, so the projection
computes rather than reads), and inside an arm the subject is still rigid. So
the arm itself binds it, and the bound it pushes carries the association the
projection reads:

```wado
match type T {
    struct([..F]) => …,     // T: ReflectStruct<FieldTypes = [..F]>
    variant([..P]) => …,    // T: ReflectVariant<CasePayloads = [..P]>
}
```

The spelling is open (Known gaps). The mechanism it feeds is not: bounds in
force are name-keyed in `annotate_ctx.trait_ctx.type_param_bounds`, which
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
delegate to a `KindOps<T, T::Kind>` whose per-kind impls carry the kind bounds
— the stand-in Rust reaches for while specialization is unstable. It needs no
new syntax, and one implication rule (`Reflect<Kind = StructKind>` ⟹
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

Minting `Reflect` for a type with no declaration is new work. Today the impls
are synthesized per TIR declaration in `synthesis/traits.rs`; a primitive, a
reference and a function type have none, so the root's three methods are minted
off a `TypeId` instead. The solver side already admits them —
a primitive lowers to `DeclKey::Builtin(name)`, so the fact table can carry it.

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
`Serialize`, and reflection answers for the _type_. `docs/spec.md` states that
boundary so the split is not rediscovered.

## Known gaps

### The arm's pack binder has no spelling

What the binder does is settled: the arm pushes `T: ReflectStruct<FieldTypes =
[..F]>` with `..F` in scope for that arm's body, which is what
`reflect_pack_bound_ty` reads and what every projection below it already
serves. What it looks like is not. Three shapes, each with a cost:

- Positional, as above — `struct([..F])`. Shortest, and the association is
  implied by the arm's kind, so `FieldTypes` / `CasePayloads` / `Members` never
  appear. A reader cannot tell from the arm which association was bound.
- Named — `struct(FieldTypes = [..F])`. Matches an impl header exactly, so a
  derivation moved from a blanket into an arm reads the same. Verbose on the
  arm that binds nothing.
- Inferred — no binder, the pack implicit under a reserved name. Rejected on
  sight: an implicitly-named type parameter is nothing else in the language.

Two questions ride on the choice. Whether an arm may bound the pack
(`struct([..F: Serialize])`) or only bind it, leaving bounds to the callee it
delegates to; and whether an arm that binds nothing may still be written
`struct` bare — which it must, or every kind-only branch pays for a pack it
never reads.

- [ ] Choose the spelling and answer both.

### The structural notation is unwritten and one-way

`core:prelude#&Point` and `core:prelude#fn(i32) -> i32` are decided above and
implemented nowhere: `symbol_notation` parses and renders declaration symbols
only. Rendering is what `canonical_name()` needs, and it comes first; resolving
one back is the harder half, since a structural type has no `AstId` for
`wado query` to land on and the notation "runs both ways" today.

- [ ] Render every `TypeInfo` case in `symbol_notation`.
- [ ] Decide what `wado query "core:prelude#&Point"` answers — the target's
      declaration, a synthesized view, or a diagnostic naming the limit.

### A resource is named twice

`Resource(DeclInfo)` names a resource by its Wado module symbol, which the
compiler has (`ResolvedType::Resource` carries a `DefId`). Its Component Model
coordinate (`wasi:io/streams.input-stream`) is a second identity for the same
type, and the two do not render alike.

- [ ] Decide whether `DeclInfo` on a resource reports the CM coordinate, the
      Wado symbol, or both.

### `Scalar` is representation, and a consumer means "leaf"

`String`, `i128` and `u128` are prelude structs, so they classify as `Struct`
and reach `opaque` downstream, while `i32` reaches `scalar`. Nothing breaks —
every derivation writes `impl … for String` — but the split a consumer wants is
leaf versus aggregate, and `Scalar` is the Wasm primitives, which is a
different line. The two arms it can land in already say "do not walk this", so
the gap is in what the taxonomy communicates rather than in what it permits.

Marking the three (`#[scalar]` on the declaration, folding them into `Scalar`)
closes it, at the price of an attribute any type may claim, which moves a
third-party type between arms.

- [ ] Decide whether `Scalar` stays "a Wasm primitive" or becomes "primitive or
      so marked", and if the latter, what marks it and who may.

### `Scalar` granularity

Nothing reads a primitive's width, signedness or floating-ness through
reflection yet. A later split has to be a sub-classification of `Scalar` for
existing arms to keep meaning what they meant.

- [ ] Revisit when a consumer needs it.

### A `Shape` tree is a library, not a synthesis

A facet-style metadata tree — kind-agnostic, walking an unknown type to
arbitrary depth through lazy `fn() -> Shape` edges — is what a schema library
(Jade, Layer B) ultimately reads. Written over `match type` it is ordinary
library code and adds no synthesized per-type metadata; synthesized in the
compiler it would be the parallel metadata list
[Reflect Derivation](./wep-2026-06-13-reflect-derivation.md) refuses beside
`members()`.

- [ ] Write it in `wado:jade` over this WEP's mechanism, not in the compiler.

## Related WEPs

- [Library-Defined Derivation over `Reflect*`](./wep-2026-06-13-reflect-derivation.md) — the three planes and the traits this WEP reaches
- [Variadic Type Parameters](./wep-2026-03-14-variadic-type-parameters.md) — the packs an arm binds, and `VariadicForOf`
- [Trait Resolution](./wep-2026-09-01-trait-resolution.md) — why a root-bounded blanket beside the kind ones is rank 3
- [Struct Walkability](./wep-2026-07-10-struct-walkability.md) — the visibility gate an arm inherits
- [Symbol Notation](./wep-2026-06-14-symbol-notation.md) — the register `canonical_name()` renders, widened here to structural types
- [Jade](./wep-2026-06-13-jade.md) — the consumer of the `Shape` gap
- [Elaborator Architecture](./wep-2026-05-26-elaborator-rearchitecture.md) — where an arm's hypothesis and its expansion live
