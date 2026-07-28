# WEP 2026-07-28: Structured fq names

## Problem

An fq name is stored as a rendered `String` (`FqTypeName(String)`), so every
consumer that needs a part of it splits the string. Eleven bugs in one refactor
came from that, in three directions:

| direction | example |
|---|---|
| a qualified head compared against a bare literal | `base_struct_name() != "List"` — SROA's method catalog came out empty |
| a bare name used where a qualified one was needed | `Receiver::Type("MyArray")` names no definition |
| the wrong split | `simple_name()` answers `i32>` for `List<i32>` because it takes the last `/` |

No split is correct in general: a `ModuleSource` may itself contain `/` and `<`,
and a type argument carries its own module path.

A rendered fq name is also **not reversible**. `ModuleSource` cannot be rebuilt
from text without `ModuleSourceInterner`, so `FqTypeName::from_mangled` cannot
honestly reconstruct what it was given — it can only re-wrap the string and
hope every later reader splits it the same way.

## Design

Keep the structure; render on demand.

```rust
pub struct FqTypeName {
    reference: Option<RefKind>,
    head: TypeHead,
    args: Vec<FqTypeName>,   // already fq themselves
}

pub enum TypeHead {
    /// A declaration, named by the module that declares it.
    Declared { module: ModuleSource, name: String },
    /// A shape no module declares — primitive, `()`, `!`, `Array`, `[]`, `Fn`.
    Builtin(String),
    /// A template's own binder (`T`, a pack member `F`).
    Binder(String),
}
```

Every question that used to need a split becomes a field access:
`decl_name()`, `module()`, `args()`, `reference()`. Rendering is
`to_mangled()`; `to_display()` gives the source-facing form for diagnostics.

The three head kinds are the distinction a `String` loses, and the one every
bug turned on: whether a module qualifies this name.

## Producer

`TypeTable` is the only thing that knows a declaration's module, so it hands
back structure:

```rust
pub fn fq_type_name(&self, id: TypeId) -> FqTypeName
```

`mangle_type_name` becomes `fq_type_name(id).to_mangled()`. Most current
`FqTypeName::from_mangled` call sites merely re-wrap a `mangle_*` result and
disappear once the producer returns structure.

## Migration

`LocalMethodName` bakes type arguments into `struct_name: String`, so it cannot
answer structurally until it stores structure:

```rust
pub struct LocalMethodName {
    receiver: Receiver,              // Receiver::Type(FqTypeName)
    struct_type_args: Vec<FqTypeName>,
    ...
}
```

`struct_name` becomes a derived rendering rather than stored state, which is
what makes the illegal state — a bare head where an fq name belongs —
unrepresentable.

Measured surface: `.struct_name` 108, `base_struct_name()` 55,
`Receiver::Type(...)` 30, `head_key()` 10. Mechanical, but one focused pass.

Order:

1. `FqTypeName` / `TypeHead` + `TypeTable::fq_type_name`.
2. `Receiver::Type(FqTypeName)`; `head_key()` returns `&FqTypeName`.
3. `LocalMethodName` stores `struct_type_args: Vec<FqTypeName>`;
   `struct_name` becomes a method.
4. Delete `from_mangled`, `simple_name`, and the `split_base_name` /
   `rsplit('/')` helpers. Nothing should parse a rendered name afterwards.

Step 4 is the point of the exercise: while a parser exists, the next caller
will reach for it.
