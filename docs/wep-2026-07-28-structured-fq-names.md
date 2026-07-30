# WEP 2026-07-28: Structured fq names

## Problem

An fq name is stored as a rendered `String` (`FqTypeName(String)`), so every
consumer that needs a part of it splits the string. Eleven bugs in one refactor
came from that, in three directions:

| direction                                         | example                                                                      |
| ------------------------------------------------- | ---------------------------------------------------------------------------- |
| a qualified head compared against a bare literal  | `base_struct_name() != "List"` — SROA's method catalog came out empty        |
| a bare name used where a qualified one was needed | `Receiver::Type("MyArray")` names no definition                              |
| the wrong split                                   | `simple_name()` answers `i32>` for `List<i32>` because it takes the last `/` |

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
    /// A shape no module declares — primitive, `()`, `!`, `Array`, `Fn`.
    Builtin(String),
    /// A template's own binder (`T`, a pack member `F`).
    Binder(String),
    /// A tuple, spelled `[a,b]` rather than `Head<a,b>`.
    Tuple,
}
```

Every question that used to need a split becomes a field access:
`decl_name()`, `module()`, `args()`, `reference()`. Rendering is
`to_mangled()`; `to_display()` gives the source-facing form for diagnostics.

The head kinds are the distinction a `String` loses, and the one every
bug turned on: whether a module qualifies this name.

A tuple is its own head because it is the one instantiated shape spelled by
surrounding its arguments rather than following its head. Every mangler goes
through this: `mangle_generic_name` renders the tuple head as `[a,b]` too, so
an instantiated tuple receiver and an `impl Trait for [i32, i32]` name one
function. While the two spellings coexisted, that impl and the variadic
template both claimed the name and the compile aborted on the duplicate.

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

`from_mangled`, `as_str` and `simple_name` are gone, so nothing parses an fq
name. What still splits a string parses a _different_ namespace, and each needs
its own structuring pass:

- `split_base_name` on a **trait** name (`Stream<u8>` → `Stream`). Trait
  references are still strings; structuring them is WEP 2026-07-29.
- `display_type_name` in `trait_bound_violation_message`, which formats a
  diagnostic from a mangled call name at the WIR-build layer — the only thing
  that layer has. It goes when WIR carries structure.

## Consequences

Two shapes covered nearly every migrated site:

- The caller holds a `TypeId`: `type_table.fq_type_name(id)` in place of
  `mangle_type_name` / `mangle_type_arg_for_generic` / `type_name`. One
  producer, so a definition and a call site cannot pick different spellings.
- The caller compares a receiver against an `impl` header: `decl_key()`, not
  `head_key()`. Getting this backwards is what emptied SROA's method catalog,
  silenced the resource-capability check, and broke go-to-definition.

Two behavioural corrections fell out of the migration rather than being sought:

- `SynthesisCtx`'s methodful-impl probe keyed a bare receiver, so an impl on one
  module's `Widget` suppressed synthesis for another module's. It now keys the
  same `(module, name)` pair `ImplTargetKey::receiver` builds.
- `MethodName`'s `Display` prefixed the defining module in front of a receiver
  that already names its declaring module, printing it twice.

A monomorphized struct is where the structuring stops paying off on its own: the
type table held its instantiated spelling and its base as two separate facts, so
one `FqTypeName` could not answer both. That is what WEP 2026-07-29 splits.
