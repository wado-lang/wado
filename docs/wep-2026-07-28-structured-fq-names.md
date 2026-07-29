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
3. **Done.** `LocalMethodName` stores `struct_type_args: Vec<FqTypeName>`, so
   `fq_struct_name` rebuilds the instantiated receiver instead of re-reading
   `struct_name`. `with_type_args` / `with_struct_type_args` take
   `&[FqTypeName]`, and `MethodName::struct_name` is an `FqTypeName` too.
4. **Done for fq type names.** `from_mangled`, `as_str` and `simple_name` are
   gone, so nothing parses one. What still splits a string parses a _different_
   namespace, and each needs its own structuring pass:
   - `split_base_name` on a **trait** name (`Stream<u8>` → `Stream`). Trait
     references are still strings; structuring them is the next WEP.
   - `display_type_name` in `trait_bound_violation_message`, which formats a
     diagnostic from a mangled call name at the WIR-build layer — the only
     thing that layer has. It goes when WIR carries structure.

## Status

The workspace builds and the fq-name namespace is structured end to end.
E2E: 3882 passed, 26 failed — 17 of those pre-date this work; three fixtures
(`serde_generic_deserialize`, `serde_json_roundtrip_complex`,
`serde_json_treemap`) are open regressions, tracked below.

### Open: a generic newtype's own method is reported as inherited

`newtype_generic_own_method` calls `MyArray<i32>::second`, an inherent method on
`type MyArray<T> = List<T>`, and WIR build reports
`core:prelude/list.wado/List<i32>::second` unresolved — the newtype was peeled
past its own `impl`.

`method_call.rs` names the receiver from
`inherited_from_base.unwrap_or(base_type_id)`, which is correct: an _inherited_
method is named by the type that defines it. So the defect is upstream, in
`lookup_method_info` reporting `inherited_from_base: Some(..)` for a method the
newtype owns. Instrumenting the inherent-impl scan shows the `MyArray` impl is
found (`entries=1`, header name matches, `inherent_impl_applies` true) for all
three receivers, but `inherent_method_info` answers `None` for one of them —
that call is the one that falls through to the base.

Ruled out since: the elaborator's scan finds the `MyArray` impl for every
receiver (the one `inherent_method_info` miss is `self.len()`, correctly
inherited), the monomorphized TIR does contain
`newtype_generic_own_method.wado/MyArray<i32>::second`, and neither
`newtype_aware_method_names` nor `receiver_keeps_newtype_own_impl` changes the
outcome once corrected.

Erasure is not involved: at rewrite time the receiver still reads as
`Newtype { name: "MyArray<i32>" }` for all three calls. What differs is the
name the _elaborator_ already put on them — printing every `second` call as it
reaches the rewrite gives

    recv=Newtype "MyArray<String>"  struct_name=…/MyArray<String>       ✓
    recv=Newtype "MyArray<i32>"     struct_name=…/MyArray<i32>          ✓
    recv=Newtype "MyArray<i32>"     struct_name=core:prelude/…/List<i32> ✗

so one of the three call sites is named after the base before monomorphize ever
runs, and everything downstream is behaving correctly on bad input. The fixture
has exactly one `second` call inside a generic function (`via_generic<U>`),
which is the obvious candidate.

Narrowed further: the fixture has three `second()` call sites, but
`lookup_method_info` is reached by only **two** of them, and both answer with
the newtype's own impl (`inherited_from_base: None`) on a receiver that reads
as `Newtype { name: "MyArray<i32>" / "MyArray<String>" }`. The third — the one
inside the generic function `via_generic<U>` — never reaches method lookup at
all, so its `List<i32>::second` name comes from some other resolution path.

Next step: find which path names `a.second()` inside a generic function body.
It is not the inherent/trait method lookup, so the receiver's newtype identity
is being dropped before that point — start from how a `let a: MyArray<i32> =
[...]` annotation is resolved inside a generic function, since the two working
sites use the identical annotation at non-generic scope.

Root cause of the three serde regressions, established by instrumentation.

`with_substituted_struct_name` used to take an instantiated spelling and a base
head as two strings; the migration collapsed them into one `FqTypeName` on the
grounds that both could be read off it. For a **monomorphized struct** they
cannot. The type table holds them as two separate facts — `name`, with the
arguments fused into the string, and `base_name` — and `fq_type_name` returns
the fused head with an empty `args`. So `head_only()` answers
`…/Wrapper<i32>`, `base_struct_name()` answers the same, the monomorphizer
looks for a template named `Wrapper<i32>^Deserialize::deserialize`, finds none,
and never queues the instance. The stub it leaves behind is what WIR build
reports as an unsatisfied bound.

Passing the base head (`fq_base_type_name`, which reads `base_name`) alongside
the instantiated one fixes all three fixtures. It is not sufficient on its own:
`function_id_for` builds a `FunctionId` from `fq_struct_name()` — base plus
`struct_type_args` — and those args are _also_ unrecoverable
(`generic_type_args` returns `None` once the originating `GenericInstance` has
left the table), so two instantiations collapse onto one id and
`serde_generic_struct_mixed_fields` trips the injectivity assert.

So the fix has to expose the base head _without_ weakening per-instantiation
identity. Two routes, neither yet tried:

- Additive: keep every identity as it is and add the base-headed name as an
  extra candidate in the monomorphize template lookup. Changes no key, so it
  cannot collide.
- Structural: give `LocalMethodName` the instantiated receiver as structure, so
  `fq_struct_name()` stays unique while `base_struct_name()` is the base.

### Open: generic-struct `Deserialize` is never instantiated

`Wrapper<i32>^Deserialize::deserialize<core:json/JsonDeserializer>` reaches WIR
build unresolved. Established by instrumentation, so none of this is inference:

- the bound-driven synth request is recorded (`Wrapper`, entry module,
  `Deserialize`) and resolves to a `TypeId`;
- `generate_struct_deserialize` succeeds and emits the template
  `serde_generic_deserialize.wado/Wrapper^Deserialize::deserialize`, carrying
  `impl_type_params` from the struct declaration;
- the `existing.contains(key)` guard does not fire;
- monomorphize nonetheless emits the instance as a bodyless stub
  (`fn "…/Wrapper<i32>^Deserialize::deserialize<…>"();`), which is what WIR
  build then fails to resolve.

`Maybe<T>` takes the same path and works — it carries an explicit
`impl<T: Deserialize> Deserialize for Maybe<T>;` marker, so the contrast is
marker-driven vs bound-driven, not variant vs struct.

Next step: instrument the template lookup in `monomorphize/func_inst.rs` for
that call and find which spelling misses.

Two shapes covered nearly every migrated site:

- The caller holds a `TypeId`: `type_table.fq_type_name(id)` in place of
  `mangle_type_name` / `mangle_type_arg_for_generic` / `type_name`. One
  producer, so a definition and a call site cannot pick different spellings.
- The caller compares a receiver against an `impl` header: `decl_key()`, not
  `head_key()`. Getting this backwards is what emptied SROA's method catalog,
  silenced the resource-capability check, and broke go-to-definition.

Two behavioural corrections fell out of the migration rather than being sought:

- `SynthesisCtx`'s methodful-impl probe keyed a bare receiver, so an impl on
  one module's `Widget` suppressed synthesis for another module's. It now keys
  the same `(module, name)` pair `ImplTargetKey::receiver` builds.
- `MethodName`'s `Display` prefixed the defining module in front of a receiver
  that already names its declaring module, printing it twice.
