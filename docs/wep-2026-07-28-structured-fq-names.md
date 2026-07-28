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

1. **Done.** `FqTypeName` / `TypeHead` + `TypeTable::fq_type_name`.
   `from_mangled`, `as_str` and `simple_name` are gone; `decl_name`,
   `module`, `args`, `to_mangled`, `to_display` replace them.
2. **Done.** `Receiver::Type(FqTypeName)`, with `head_key` (mangled identity)
   and `decl_key` (what an impl header writes) as separate accessors —
   conflating those two was the direct cause of several mis-dispatches.
   `with_substituted_struct_name` now takes one `FqTypeName` instead of an
   (instantiated, base) string pair that could disagree.
3. **Done in `name.rs`; callers pending.** `LocalMethodName` stores
   `struct_type_args: Vec<FqTypeName>`, so `fq_struct_name` rebuilds the
   instantiated receiver instead of re-reading `struct_name`.
   `with_type_args` / `with_struct_type_args` take `&[FqTypeName]`, and
   `MethodName::struct_name` is an `FqTypeName` too. `name.rs` itself is
   clean; every remaining error is a caller still holding a rendered name.
4. Delete the remaining `split_base_name` / `rsplit('/')` helpers and
   `display_type_name`. Nothing should parse a rendered name afterwards.

Step 4 is the point of the exercise: while a parser exists, the next caller
will reach for it.

## Status

The build is red mid-migration, by design — the type errors are the worklist.
`name.rs` is clean; 112 errors remain across 19 files, each a site that was
handed a rendered name and must now be handed structure. The frontier,
largest first:

| file | errors |
|---|---|
| `optimize/dce.rs` | 22 |
| `elaborator/reify.rs` | 14 |
| `synthesis/traits.rs` | 11 |
| `monomorphize/func_inst.rs` | 11 |
| `elaborator/method_call.rs` | 11 |
| `tir.rs` | 7 |
| `elaborator/trait_query.rs` | 6 |
| `synthesis/serde_synth.rs` | 5 |
| others (11 files) | ≤4 each |

The count rose as `name.rs` was finished — that is the migration working:
each signature that stops accepting a `String` surfaces the callers that were
passing one.

Two recurring shapes cover most of them:

- The caller holds a `TypeId`: use `type_table.fq_type_name(id)` in place of
  `mangle_type_name(id)`.
- The caller compares a receiver against an `impl` header: use
  `Receiver::decl_key()`, not `head_key()`. Getting this backwards is what
  emptied SROA's method catalog, silenced the resource-capability check, and
  broke go-to-definition.
