# WEP: Reflection over an Unknown Type — `TypeKind` and `match type`

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
  `dyn` handles. The kind query is this WEP's `TypeKind`; the `dyn` half needs
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

`TypeKind` is what the root answers with:

```wado
pub enum TypeKind {
    Struct,
    Variant,
    Enum,
    Flags,
    Newtype,
    Tuple,
    Scalar,
    Opaque,
}

internal trait Reflect {
    fn kind() -> TypeKind;
    fn type_name() -> String;
    fn wire_name_policy() -> CaseStyle;
}
```

`kind()` reads a compile-time fact, so it folds to a constant at each
instantiation.

The classification is total by construction — `TypeTable::reflect_kind` already
computes the first five and answers `None` for everything else, which this WEP
splits into the last three:

| Type                                                      | Kind      |
| --------------------------------------------------------- | --------- |
| `struct` (anonymous included), `variant`, `enum`, `flags` | that kind |
| `type N = B`                                              | `Newtype` |
| the tuple family, `()`                                    | `Tuple`   |
| `i8`…`u128`, `f32`, `f64`, `bool`, `char`, `v128`         | `Scalar`  |
| a reference, a function type, a resource, `Never`         | `Opaque`  |
| the four sealed member handles                            | `Opaque`  |

`Scalar` is deliberately coarse. Nothing yet reads a primitive's width or
signedness through reflection, and a consumer that needs one today writes the
per-type impl it already writes. Splitting it later is additive: an arm that
matched `Scalar` keeps matching whatever replaces it only if the split is a
sub-classification, which is the constraint any later refinement inherits.

`Opaque` is the honest arm, not a gap. A reference and a function type have no
owning declaration, and a resource's identity is a Component Model coordinate
rather than a module symbol, so the earlier WEP declined to invent names for
them. Totality forces an answer only for `type_name()`, which renders the
type's spelling (`&Point`, `fn(i32) -> i32`); `type_info()` is where the
question actually bites, and it stays open below.

The visibility gate is unchanged: it sits on the kind traits and never on the
root ([Reflect Derivation](./wep-2026-06-13-reflect-derivation.md),
Visibility). A total root is what that split already implied — a type's name is
public the moment the type is.

### `match type` — narrowing a subject to its kind

A bound states a kind before the subject is known. The body states it after:

```wado
fn describe<T: Reflect>(v: &T) -> String {
    match type T {
        struct => return fields_of(v),
        variant => return live_case_of(v),
        enum | flags => return Reflect::<T>::type_name(),
        newtype => return describe(&B::from(*v)),
        _ => return Reflect::<T>::type_name(),
    }
}
```

Each arm proves its kind's bound for its own body. `struct =>` elaborates under
the hypothesis `T: ReflectStruct`, so `ReflectStruct::<T>::members()` resolves
inside it and nowhere else.

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

- `kind()` — what the type **is**. A fact about the declaration, independent of
  where it is asked.
- `match type` — what the type may be **opened** as. Gated on visibility, so a
  struct whose members are private elsewhere does not reach the `struct` arm
  there.

A consumer composes the two: in the fallback arm, `kind()` still reports
`Struct`, so "a struct this module may not enumerate" is expressible. Neither
construct can state it alone.

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

The value plane for an unknown type is unchanged and stays out of reflection: a
uniform walk over an unknown _value_ is `core:value::Value` through
`Serialize`, and reflection answers for the _type_. `docs/spec.md` states that
boundary so the split is not rediscovered.

## Known gaps

### The arm's pack binder has no spelling

`struct([..F])` above is a sketch. Each kind binds a different association
(`FieldTypes` / `CasePayloads` / `Members`), and an arm that needs no pack
should not be made to write one.

- [ ] Choose the spelling, and decide whether an arm may bind a pack bound
      (`struct([..F: Serialize])`) or only the pack itself.

### Exhaustiveness

Listing every kind makes adding one a breaking change across every derivation
in the ecosystem, and the tuple family is already queued. Requiring `_` costs a
line in the exhaustive case.

- [ ] Decide whether `_` is mandatory.

### Which arm an unopenable struct takes

A struct whose members are not visible here cannot satisfy `ReflectStruct`, so
it cannot reach a `struct` arm. Falling to `_` is consistent with the
visibility gate and leaves `kind()` to report what it is; the alternative is a
distinct arm naming the case.

- [ ] Decide `_` versus a named arm.

### `TypeInfo` has no answer for `Opaque`

[`TypeInfo`](./wep-2026-06-13-reflect-derivation.md) is a declaration name, a
module and the instantiation's type arguments. A reference and a function type
have no declaration and no module, and a resource's coordinate is not a module
symbol. A total root asks the question the earlier WEP deferred.

- [ ] Decide whether `TypeInfo` gains an opaque case, whether `module()`
      becomes optional, or whether `type_info()` stays partial while
      `type_name()` is total.

### `String` is a struct, not a `Scalar`

`String` is a `pub struct` in the prelude, so it classifies as `Struct` and its
private fields keep it out of the `struct` arm elsewhere — landing it in `_`
while `i32` lands in `Scalar`. Every derivation writes `impl … for String`
today, so nothing breaks, but a consumer reading the taxonomy will expect the
two to agree.

- [ ] Decide whether `Scalar` is "primitive" or "primitive or so marked", and
      if the latter, what marks it.

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
- [Jade](./wep-2026-06-13-jade.md) — the consumer of the `Shape` gap
- [Elaborator Architecture](./wep-2026-05-26-elaborator-rearchitecture.md) — where an arm's hypothesis and its expansion live
