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

Effect rows alias without any rewrite. `fn(i32) -> i32` and
`fn(i32) -> i32 with($e)` intern apart, yet every identity read after lowering
drops the row.

### One identity became five

Fixes to the aliasing have each added a way to ask "are these one type?" that
tolerates it:

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

One table serves two universes of types, and moves from the first to the second
in place.

1. Source types, from elaboration until erasure. Newtypes, `flags`, references
   and effect rows are part of a type's identity, because type checking and
   trait dispatch read them: an impl is keyed under `Perms`, not `u32`.
2. Representation types, from erasure on. A newtype is its base, `flags` is
   `u32`, a reference to a primitive, a variant or a function is a `Box`, and
   rows mean nothing.

The second universe is a many-to-one image of the first. Today the table moves
its slots from one universe to the other, and that is what makes an id stop
being an identity. The second universe needs no slots of its own: `f64` and
`List<f64>` are ordinary types the table already interns.

## Decision

The table never changes what a slot means. Erasure rewrites the IR's types
instead, and boxing changes no type at all.

### 1. The table is append-only

`intern` is the only way to add a slot, and nothing redirects, redefines or
settles one afterwards. An id names one type for the table's whole life, so
`==` on ids is the identity question and `Hash` on ids is the identity key.
`TypeKey`, `same_type`, `TypeMemo` and `InstanceArg` have no work left and go.

`Meters` keeps its id after erasure and still means `Meters`. Nothing after
erasure holds that id, so nothing asks.

### 2. Erasure is a function on types

`erase_type(TypeId) -> TypeId`, memoized, is the only place that states the
rule: a newtype becomes what its base becomes, `flags` becomes `u32`, a
function type drops its row, and every other spelling is rebuilt from what its parts become and interned. `List<Meters>`
therefore erases to the id `List<f64>` already has, with no settling step.

The erasure rewrites every `TypeId` the `FlatPackage` holds through it:
expressions, locals, parameters, declarations, and the type-keyed tables beside
them. Two TIR struct declarations that erase to one id become one declaration.

### 3. Boxing changes no type

A reference keeps its type to the end: `&i32` is `&i32` in TIR, NIR and the
analyses over them, and `&mut i32` stays apart from it. What boxing decides is
storage, which one predicate states, `is_boxed_reference_target`: a reference
to a struct, a tuple or a list points at that object, and a reference to
anything else points at a cell, an object of one field `value`.

- The boxing pass rewrites bodies only. An address-taken local `x: T` becomes
  `x: &mut T`, initialized with a new cell; a read of `x` reads `x.value`, and
  `&x` is `x`. A `*r` through a cell reads `r.value`, and `&temp` builds a cell.
- In NIR a cell is a struct literal and a field access whose type is the
  reference. The windows that answer a type's fields (`aggregate_field_info`,
  `StructDecls::of_type`, `heap_object_key`) answer `[value: T]` for a cell
  reference, so every pass that removes or forwards a one-field struct does the
  same to a cell.
- WIR build gives a cell reference the type `ref $Cell<R>`, where `R` is the
  representation of its target. A `&` and a `&mut`, a reference at any depth,
  and two function types differing by a row or a parameter all share one
  representation, so they share one cell type without a key of their own.

Since a reference never changes type, whether it is writable is its type, and
the names lowering mints do not change across boxing.

### 4. Source questions are answered before erasure

After erasure, nothing asks for a type's source identity: the newtype chain
walks and `is_newtype` are read only by the elaborator, the effect check and
synthesis.
What still reads a source spelling after erasure, the names lowering mints
(`name_unboxed`, `mangle_type_arg_unboxed`) and the reflect bridges that name a
call from the erased mangle, reads it from what the rewrite leaves: the erased
type, or a name recorded before the rewrite.

### 5. One name per type

`get` and the name renderers lose their erased, unerased, boxed and unboxed
variants, since a slot no longer has two readings. WIR build dedups its types
by id, not by mangled string.

### 6. Rejected alternatives

#### A separate id type for lowered types

`ReprId` and a `ReprTable` beside `TypeId` would make mixing the universes a
compile error. They add a third identity to a codebase that already has too
many, and duplicate the table's constructors. The append-only table already
makes every id honest. What a separate type would add, the debug assertion in
section 3 checks.

#### Canonicalize the aliases with union-find

`redefine_to` becomes a union, and `find` answers the canonical id. This makes
`type_key` cheaper, but keeps the table changing under the IR. A key taken
before a union still differs from one taken after it.

#### Remove `Eq` and `Hash` from `TypeId`

This turns the wrong comparison into a compile error. Interning needs both,
though: `ResolvedType` contains `TypeId`s and is the intern map's key. Constant
patterns such as `TypeTable::BOOL =>` need the derived `Eq` too. The aliasing
stays, and every comparison moves to `TypeKey`.

## Roadmap

Each step leaves the compiler green and is a commit series of its own.

- [x] In the modules that run only before erasure, replace `type_key` and
  `same_type` with `==`. `TypeTable`'s own helpers, which later phases share,
  wait for the step that removes the aliasing.
- [x] Add `erase_type` and rewrite the `FlatPackage` through it where erasure
  runs: functions, globals, struct and variant declarations, and the
  declared members and anonymous shapes the table keeps. From erasure on,
  `intern` answers the erased id and `iter_type_ids` yields only erased ids.
  Erasure no longer touches `redirects`, and `get_unerased` is `get`.
- [ ] Boxing changes no type: cells are references, and WIR build keys them by
  representation. Done when `redefine_to`, `redirects`, `box_payload_types`,
  `box_key`, the prelude `Box` and the `is_mut_ref` captures are gone.
- [ ] Delete `TypeKey`, `same_type`, `TypeMemo`, `InstanceArg`, `box_key`,
  the `_unboxed` and `_resolving_newtypes` renderers, and the rule in
  `wado-compiler/AGENTS.md`.

## Known gaps

### A remark after lowering has no source name to print

Remarks run on NIR and name a type through `TypeTable::name`. A lowered id does
not know that `f64` was `Meters`. What a remark should name, and where it gets
that name, is undecided.
