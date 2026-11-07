# WEP: Type Identity — Equal Ids Mean One Type

## Context

`TypeTable` interns each `ResolvedType` spelling to a `TypeId`. Interning
promises one thing: two ids are equal exactly when they name one type. The
pipeline breaks that promise after monomorphization. Several phases rewrite the
table in place so that the IR's ids keep pointing where they did, and each
rewrite leaves many ids naming one type.

| Rewrite                     | Where                                                       | What it leaves behind                                                                                            |
| --------------------------- | ----------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| Newtype and `flags` erasure | `TypeTable::erase_newtypes_and_flags`, after monomorphize   | `redirects`: a `Meters` slot reads as `f64`, and `List<Meters>` stays a slot apart from `List<f64>`              |
| Settled spellings           | `settle_spelling`, run by erasure and by every later intern | `settled_spellings`: maps `List<Meters>` to `List<f64>` so the two key alike                                     |
| Boxing                      | `lower::plan::boxing`, `TypeTable::redefine_to`             | a `&i32` slot and a `&mut i32` slot both read as `Box<i32>`, and the spelling they lost is kept for `is_mut_box` |
| Dead type pruning           | `TypeTable::retain`, from NIR DCE                           | holes in the table, closed over the three maps above                                                             |

Effect rows add aliasing of their own, without any rewrite. `fn(i32) -> i32`
and `fn(i32) -> i32 with($e)` intern apart. Once the effect check is done, no
phase after lowering treats them as different types.

### One identity became five

Each fix since then has added a way to ask "are these one type?" that
tolerates the aliasing:

- `TypeKey`, from `TypeTable::type_key`: the id with `redirects`, the intern
  map and `settled_spellings` applied in turn. `same_type` compares two keys.
  `TypeMemo` caches an answer per key.
- `InstanceArg`, from `TypeTable::instance_arg`: a structural rebuild of a
  struct instance's arguments that drops effect rows. It exists because two
  instances can differ by a row and still be one type, and their keys differ.
- `box_key` in `lower::plan::boxing`: keys a function by `(arity, return type)` and a reference by `(pointee key, depth)`, so ids that box alike
  share one `Box`.
- Mangled names. `TypeTable::mangle_*` and `TypeTable::fq_type_name_*` come in
  variants with and without rows, with newtypes resolved or kept, and boxed or
  unboxed. WIR build dedups its types by these strings.
- `TypeId` itself. It still derives `Eq` and `Hash`, so `a == b` and
  `IndexMap<TypeId, _>` compile wherever they appear. The rule against them is
  a sentence in `wado-compiler/AGENTS.md`, not a check.

### The class of defect

These fixes each compared or keyed by one of the five and needed another.

| Commit     | Symptom                                                                                                           |
| ---------- | ----------------------------------------------------------------------------------------------------------------- |
| `a236f562` | `value_copy_demote` dropped a call-graph edge on a duplicate id and under-approximated `leaky`                    |
| `cd7ea6a6` | the resource-holding walk compared ids                                                                            |
| `cc97c429` | one instance spelled as `GenericInstance` and as `Struct` took two `TypeKey`s                                     |
| `ae4bb114` | `param_spec` compared a seed's struct and a parameter's struct by `TypeId`                                        |
| `29fc65b3` | an erased newtype argument keyed apart from its base                                                              |
| `7d12c8f9` | instances differing only by a newtype or a row were declared apart                                                |
| `e7939245` | `sroa_param` walked a struct's fields under one spelling only                                                     |
| `3e861c81` | `sroa_variant_return` and `labeled_block_fusion` compared ids                                                     |
| `e2c97b31` | a struct pattern lost its fields where the settled spelling was not the one a declaration keyed                   |
| `95a5918c` | a struct name minted before boxing missed one minted after, and a copy of `Holder<&mut i32>` aliased the original |

The last one is wrong code. Each fix repaired the site it was found at, and
the next site was found later.

### What `TypeKey` costs where it changes nothing

`redirects` and `settled_spellings` stay empty until erasure. Before erasure,
`type_key(id)` therefore answers `id`, and `same_type(a, b)` answers `a == b`.
The elaborator and the monomorphizer call these anyway, to follow the rule. Each
call hashes a whole `ResolvedType` to look up the intern map.

### The cause

One table serves three universes of types, and moves between them in place.

1. Source types, from elaboration through monomorphization. Newtypes, `flags`,
   references and effect rows are all part of a type's identity, because trait
   dispatch and type checking read them.
2. Monomorphic nominal types, after monomorphization and before erasure. They
   still need newtype identity, since impls are keyed under `Perms` and not
   under `u32`.
3. Representation types, from lowering on. A newtype is its base, `flags` is
   `u32`, a reference to a primitive, a variant or a function is a `Box`, and
   rows mean nothing.

Universes 1 and 2 are one universe whose ids are already identities. Universe 3
is a different set of types: a many-to-one image of the first. Storing the image
in the same table, under the same id type, is what makes an id stop being an
identity.

## Decision

### 1. An id is the type

Within a table, a type has one id, and an id names one type for the table's
whole life. `intern` is the only way to add a slot, and nothing redirects,
redefines or settles a slot afterwards. `==` on ids is the identity question,
and `Hash` on ids is the identity key. `TypeKey`, `same_type`, `TypeMemo` and
`InstanceArg` have no work left and go.

### 2. Two universes, two id types

- `TypeId` and `TypeTable` hold source types, from elaboration through
  monomorphization and synthesis. Nothing in them changes.
- `ReprId` and `ReprTable` hold representation types, from lowering on. NIR,
  the optimizer, `niri` and WIR build read only these.

The two ids are distinct Rust types with private fields, and neither converts to
the other. A source id used where a representation is expected is a type error.

### 3. One function says what representation means

`repr(TypeId) -> ReprId`, memoized, is the only place that states erasure,
dropping rows and boxing:

- a newtype is the representation of its base, and `flags` is `u32`;
- a function type keeps its parameters and return type, and drops its row;
- a reference whose target `is_boxed_reference_target` names is a `Box` of the
  target's representation, and any other reference is a reference to it;
- every other spelling is rebuilt from the representations of its parts.

Because `repr` rebuilds structurally, `List<Meters>` and `List<f64>` reach one
`ReprId` with no settling step. `ReprTable`'s constructors apply the same rules,
so a type an optimizer pass mints after lowering is boxed exactly as `repr`
would box it.

### 4. Lowering translates every type once

The TIR → NIR fold already visits every node, and already performs the boxing
rewrite of bodies. It maps each type through `repr` as it goes. The `BoxPlan`
type facts become `ReprTable` facts, and the boxing pass touches no table that
TIR reads.

A struct declaration in NIR is keyed by its `ReprId`. Two TIR instances that
`repr` maps to one id become one NIR declaration, so `List<Meters>` and
`List<f64>` need no deduplication further down.

### 5. What NIR asks of a source type is answered at lowering

After lowering, nothing asks for a type's source identity: `get_unerased` and
`is_newtype` are read only by the elaborator, the effect check and synthesis. Impl dispatch and
the names that must match an impl are resolved before lowering and recorded on
the node that needs them. A `ReprId` answers no source question.

### 6. One name per universe

A source type renders one name, and a representation type renders one name.
WIR build maps a `ReprId` to a `WirTypeId` one to one and dedups by id, not by
string.

## Alternatives considered

### Canonicalize the aliases with union-find

`redefine_to` becomes a union, and `find` answers the canonical id. This makes
`type_key` cheaper, but keeps the table changing under the IR. A key taken
before a union still differs from one taken after it, and `TypeId`'s `==` still
compiles.

### Remove `Eq` and `Hash` from `TypeId`

This turns the wrong comparison into a compile error. Interning needs both,
though: `ResolvedType` contains `TypeId`s and is the intern map's key. Constant
patterns such as `TypeTable::BOOL =>` need the derived `Eq` too.
The aliasing stays, and every comparison moves to `TypeKey`.

### Rebuild one table at each phase boundary, keeping one id type

Erasure and boxing build a fresh `TypeTable` and rewrite the IR's ids through a
map. This restores section 1 with a smaller change than section 2. It does not
stop a TIR id from being read against the NIR table, which compiles and answers
whatever type occupies that slot.

## Roadmap

Each step leaves the compiler green and is a commit series of its own.

- [ ] Before erasure, replace `type_key` and `same_type` with `==`, and assert in
  `type_key` that erasure has run. Done when no caller before erasure
  remains.
- [ ] Introduce `ReprId`, `ReprTable` and `repr`, and switch WIR build's type
  registration to them. Done when WIR build dedups by `ReprId` and no
  longer reads `mangle_type_arg_rowless_resolving_newtypes`.
- [ ] Switch NIR to `ReprId`, moving erasure into `repr`. Done when
  `erase_newtypes_and_flags`, `redirects` and `settled_spellings` are gone.
- [ ] Move the boxing type rewrite into `repr`. Done when `redefine_to`,
  `follow_redefinitions` and `box_payload_types` are gone.
- [ ] Delete `TypeKey`, `same_type`, `TypeMemo`, `InstanceArg`, `box_key`, the
  `mangle_*` and `fq_type_name_*` variants that only one universe uses, and
  the rule in `wado-compiler/AGENTS.md`.

## Known gaps

### `&T` and `&mut T` share a `Box` but not a meaning

Today both redefine to one `Box<T>` struct, which is what lets a `&mut`
argument reach a `&` parameter. `is_mut_box` tells them apart by the spelling
left in the slot, and `sroa_param`, `niri::region` and value-copy's points-to
analysis read it. Section 1 leaves no slot to read. Whether writability is part
of a representation type or of the place that holds one is undecided, and the
boxing step of the roadmap waits on it.

### Diagnostics after lowering print source types

Remarks and the NIR dump name types as the user wrote them. A `ReprId` no longer
knows that `f64` was `Meters`. What a remark should name, and where it gets that
name, is undecided.

### The size of the NIR switch

NIR and the optimizer read `TypeId` and `TypeTable` throughout. The switch is
mechanical but touches most optimizer passes at once, and it cannot be split by
pass without a period where both id types flow through NIR.
