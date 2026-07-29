# WEP 2026-07-29: Name namespaces as types

## Context

Every naming defect found while migrating to structured fq names was the same
mistake: a name from one namespace handed to a consumer that keys on another.
The compiler answers at least five distinct questions with a `String`:

| namespace | example | who keys on it |
|---|---|---|
| mangled identity | `core:prelude/list.wado/List<i32>` | `func_map`, emitted function names |
| declaration name | `List`, `MyArray`, `Wrapper<i32>` | `impl` headers, module scope, CM interface registry, go-to-definition |
| struct-list key | bare head + qualified args | the package's struct list |
| template key | `List` | monomorphize template registration |
| instance key | `List<i32>` | per-instantiation identity |

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

Structuring `FqTypeName` fixed the *representation* but not the failure mode,
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

It stores `struct_name: String` alongside `receiver` and `struct_type_args`, with
an unenforced invariant `struct_name == receiver.mangle(struct_type_args)`.
`struct_name` becomes a method over the two structural fields, and the illegal
state is gone.

Measured before attempting it: with the divergence reported from
`to_mangled_name` on every name the compiler emits, the fixtures still open
produce **no** divergence — the invariant holds today. The monomorphized-struct
defect was `fq_type_name` not knowing the arguments, which recording them at the
instantiation site already fixed. So this step is mechanical and
behaviour-preserving, not the risky one it looked like: ~108 `.struct_name`
field reads become calls, and the field and its hand-written initialisers go.

### 3. One encoding of "which type owns this method"

`inherited_from_base: Option<TypeId>`, `struct_name` and `receiver` are three
encodings of the same fact, and the generic-newtype defect is them disagreeing.
Replace with one:

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

```rust
enum TypeIdentity {
    Declaration(DeclId),
    Instance { decl: DeclId, args: Vec<TypeId> },
}
```

`ResolvedType::Struct` carries a `TypeIdentity`, never a pre-rendered name.
Today it carries `name: String` with the arguments spelled into it plus a
separate `base_name`, so "what is the base?" and "what are the args?" are not
both answerable — the defect that cost the most this session. `is_monomorphized`
and `base_name` are then redundant and go away.

`TypeHead::Builtin(String)` becomes an enum of the shapes that actually exist, so
`FqTypeName::builtin("List")` — a declaration passed off as a builtin, which the
current API accepts and which appears in a test today — stops compiling.

## Consequences

The compiler stops accepting the class of code this session kept producing. A
name cannot be built without saying which namespace it is in, and cannot be used
where another is expected.

Costs and risks:

- Steps 3 and 4 are large. Step 4 touches `ResolvedType::Struct`, matched in ~160
  places (nearly all with `..`), and changes interning keys.
- Step 1 is behaviour-preserving by construction: it changes only what compiles.
  It goes first for that reason. Steps 2–4 change behaviour and each needs the
  e2e suite as its gate — this session broke 45 fixtures with one such change
  that looked locally correct.
- The rendered-string escape hatches must go as the newtypes land, or callers
  will route around them. `split_base_name` on trait names and
  `display_type_name` at the WIR layer are the two that remain.

Order:

- [x] 1a. `DeclName` / `DeclPath` + the CM interface registry, the adapter map
      and `impl_target_of` keyed by them.
- [x] 1b. `MangledName`, so `head_key` and `decl_key` are no longer
      interchangeable — the swap behind three of the six defects.
- [ ] 1c. `MangledName` on `func_map` / emitted names; `StructListKey` on the
      struct list.
- [ ] 2. `LocalMethodName::struct_name` field → derived method.
- [ ] 3. `MethodOwner` replacing `inherited_from_base` and its sibling fields.
- [ ] 4. `TypeIdentity` in `ResolvedType`; delete `base_name` / `is_monomorphized`.
