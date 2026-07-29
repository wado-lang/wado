# WEP 2026-07-29: Name namespaces as types

## Context

Every naming defect found while migrating to structured fq names was the same
mistake: a name from one namespace handed to a consumer that keys on another.
The compiler answers at least five distinct questions with a `String`:

| namespace        | example                            | who keys on it                                                        |
| ---------------- | ---------------------------------- | --------------------------------------------------------------------- |
| mangled identity | `core:prelude/list.wado/List<i32>` | `func_map`, emitted function names                                    |
| declaration name | `List`, `MyArray`, `Wrapper<i32>`  | `impl` headers, module scope, CM interface registry, go-to-definition |
| struct-list key  | bare head + qualified args         | the package's struct list                                             |
| template key     | `List`                             | monomorphize template registration                                    |
| instance key     | `List<i32>`                        | per-instantiation identity                                            |

Observed failures, all of this shape:

- A resource method looked up as `{head_key}::{method}` against a registry keyed
  by the declared `Resource::method` — no import adapter synthesized, WIR build
  hit an unresolved `Descriptor::open_at`.
- A reflect bound checked with a mangled head against module-scope lookups — every
  `ReflectEnum` / `ReflectFlags` / `Deserialize` blanket silently stopped holding.
- `T::method()` rewritten to `{mangle_type_name(T)}::{method}` and fed to
  `locate_static_method_impl`, which compares against what a header writes.
- `with_substituted_struct_name` given one name to answer both "what is the base?"
  and "what is the instance?".
- `function_id_for` putting the receiver's type arguments in the id and dropping
  the method's, collapsing `field<T>` onto `field<i32>`.
- `FqTypeName` rendering a tuple as `[]<i32,i32>` where every other mangler
  spells `[i32,i32]`.

Structuring `FqTypeName` fixed the _representation_ but not the failure mode,
because every accessor still hands back a `String` and `Receiver::head_key` /
`decl_key` are interchangeable to the type checker. Splitting them made the
distinction expressible; nothing makes it enforced. Each of the bugs above was
caught by a test, never by the compiler.

## Decision

Make the namespace part of the type, so a name from one cannot reach a consumer
that keys on another. Four changes, ordered so each is independently valuable
and the cheapest one catches the most.

### 1. Namespaced name types

```rust
pub struct MangledName(String);
pub struct DeclName(String);
pub struct StructListKey(String);
```

No `Deref<Target = str>`, no `AsRef<str>`, no `From<String>`. They are minted
only by the authority that knows the namespace — `FqTypeName` and `TypeTable` —
and converted only through named methods that state why the change is sound.
`Display` goes on `DeclName` alone, the one form meant for humans.

Registries then demand their own key type:

```rust
impl CmInterfaceRegistry {
    fn get_function(&self, key: &DeclPath) -> Option<&FunctionInfo>;
}
impl TraitEnv {
    fn impl_headers(&self, target: &DeclName) -> ...;
}
```

`DeclPath` is built as receiver + method, never with `format!("{head}::{method}")`,
so the assembly sites disappear along with the chance to assemble from the wrong
half.

This step alone makes five of the six failures above fail to compile.

### 2. `LocalMethodName` derives its rendered name

It stored `struct_name: String` alongside `receiver` and `struct_type_args`, with
an unenforced invariant `struct_name == receiver.mangle(struct_type_args)`.
`struct_name` is a method over the two structural fields, and the illegal state
is gone.

### 3. One encoding of "which type owns this method"

`inherited_from_base: Option<TypeId>`, `struct_name` and `receiver` were three
encodings of the same fact, and the generic-newtype defect was them disagreeing.
One replaces them:

```rust
enum MethodOwner {
    Own(TypeIdentity),
    Inherited { via: TypeIdentity, owner: TypeIdentity },
}
```

Naming reads `owner`. The newtype-override question reads the discriminant,
rather than comparing a declaration name against an instantiated one — which is
what made that guard never fire.

### 4. Declaration and instantiation separated in the type table

`ResolvedType::Struct` carries `decl_name` plus `type_args: Vec<TypeId>`; the
rendered spelling is derived by `TypeTable::struct_rendered_name`. No fused name,
so nothing to mistake.

Interning keeps the rendered spelling as its identity. Holding the argument ids
as identity instead would mint two types where equivalent-but-distinct `TypeId`s
meet — such ids demonstrably exist, which is why
`Monomorphizer::try_queue_function` dedupes a blanket instance reached from two
dispatch sites. What this step buys is that head and arguments are separately
readable, not that identity changes.

`make_monomorphized_struct` carries a `debug_assert_eq!` that the caller's
rendering matches `struct_rendered_name`, so a divergence surfaces in tests
rather than as a wrong mangled name.

Converting a site is not a rename. The old `name` was the rendered spelling, so
`struct_rendered_name(decl_name, type_args)` is the behaviour-preserving answer
and `decl_name` is a behaviour change — where `decl_name` is right, the old code
was wrong. A recurring shape,
`FqTypeName::declared(module_source, name)` built from the _rendered_ name, is
the fusion written out longhand and collapses to `fq_type_name(id)`.

## The rules the split establishes

### A struct registry is keyed by the rendering, not the declaration

`struct_fields_map`, `struct_fields`, `struct_index`, `single_field` and
`package.structs` hold one entry per instantiation, so `decl_name` misses every
one of them. `TypeTable::struct_list_name` owns this namespace — the rendered
name for a `Struct` and a `GenericInstance` alike — and replaces
`struct_decl_name`, whose two arms disagreed.

A rendered name is a lossy encoding of a pair, so any reader that decodes it back
into a pair is a silent dependency on the encoding.
`get_struct_info_from_type` reverse-looked its rendering up in `mangled_to_key`
to recover `(name, impl_type_args)`; those are exactly what the struct now
stores, and the round trip is deleted.

### One derivation on both sides

A name minted for a definition and a name built to look it up must come from one
function, or nothing makes them agree:

- A reference receiver was spelled by `Receiver::mangle_with_ref` when a
  definition was named and by `FqTypeName::to_mangled` when a call site looked it
  up. `to_mangled` applied the receiver's arguments to a `&` head, giving
  `&<List<i32>>` against `&List<i32>`, so every ref-impl candidate was silently
  dead. A reference is a pointee carrying a kind, not a head with arguments.
- DCE keyed definitions on `full_method_name` and call sites on `method_name`, so
  a method with method type args was keyed two ways and could be collected while
  live.
- DCE retention compares a name fixed at monomorphize time against a reachability
  set rendered after newtype / flags erasure, so one type spells two ways
  (`FlagsBit<Perms>` against `FlagsBit<u32>`). Retention derives the same
  rendering from the instantiation the struct records, so both sides read the
  arguments through the erased view.

The regression test for the first asserts the two functions agree rather than
pinning either one's output — a test that pins one spelling passes throughout.

### A surviving type must be readable

`TypeTable::retain` guarantees `get(id)` never panics for a surviving id. A
monomorphized struct records its arguments as they were before erasure while the
reachability walk reaches types through the erased view, so nothing kept a flags
argument's own id alive: the struct survived spelling itself with an id that no
longer resolved. `retain` closes over each surviving struct's `type_args`
transitively, the same reasoning that motivated the `redirects` closure.

### The fusions that remain

`ResolvedType::Newtype` bakes its arguments into the head (`MyArray<i32>`), so
`impl_receiver_key` and `newtype_own_name` hand the impl index a name no `impl`
header writes. The guard stopping a newtype's own method from being retargeted at
its base therefore never fires for a generic newtype. Both sites split the head
by hand; step 6 is the honest fix.

A method name records its module twice —
`core:prelude/string.wado/core:prelude/string.wado/String::with_capacity` —
because `struct_name` returns a module-qualified head and `MangledName::in_module`
prefixes the defining module again. It is redundant, not wrong: the key is
`(impl module, qualified struct, trait, method)` and both sides build it the same
way. Neither half is removable alone. Without the module prefix a builtin
receiver loses its only qualifier, so two modules implementing `Display for i32`
collide; with a local struct head, `impl Foo for a/T` and `impl Foo for b/T`
written in `c` collide as `c/T^Foo::m`. Only a key carrying the two modules as
separate fields removes it.

## Consequences

The compiler stops accepting the class of code this refactor kept producing. A
name cannot be built without saying which namespace it is in, and cannot be used
where another is expected.

Costs and risks:

- Step 1 is behaviour-preserving by construction: it changes only what compiles.
  Steps 2–4 change behaviour and each needs the e2e suite as its gate.
- Step 4 touches `ResolvedType::Struct`, matched in ~160 places. Its nominal
  variants are matched in or-patterns binding one `name` across
  `Struct | Enum | Variant | Newtype | Flags | Resource | GenericInstance`, so
  each such group needs `Struct` lifted into its own arm. That is the point: a
  monomorphized struct is the one nominal type whose `name` is not a declaration
  spelling, and the or-patterns are what let it be read as though it were.
- The rendered-string escape hatches must go as the newtypes land, or callers
  route around them. `split_base_name` on trait names and `display_type_name` at
  the WIR layer are the two that remain.

## Remaining work

- [ ] 5. `StructListKey` as a type, so a registry cannot be keyed by
      `(decl_name, module)` at all. Sixteen readers took the split field for the
      stored name; a newtype makes that a compile error rather than a convention.
- [ ] 6. Split the fused spelling out of `ResolvedType::Newtype` the way 4b did
      for `Struct`.
- [ ] 7. Carry the struct's `TypeId` on `TirStruct` / `NirStruct`, so DCE
      retention asks identity instead of deriving a name that has to match one
      built elsewhere. Retention still accepts the stored name, because a
      non-monomorphized struct has no `monomorph_info` to derive from.
- [ ] 8. A method key holding the impl module and the receiver's module as
      separate fields. Blocked on 5: the redundancy is load-bearing until the key
      is structured.
