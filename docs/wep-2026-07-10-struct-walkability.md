# WEP: Struct Walkability — Field Walks over `Reflect` and `#[secret]` Fields

Status: Draft

## Context

Tuples are walkable: `for let v of tuple` unrolls at compile time with each
element typed individually
([Compile-Time Tuple Enumeration](./wep-2026-02-10-compile-time-tuple-enumeration.md)).
The same capability is wanted for structs — serialization, structured logging
(`core:log`'s efficient field passing walks an anonymous struct's fields in the
sink), schema derivation (Jade), and `Inspect` all need to visit each field with
its concrete type.

The substrate already exists. Type packs, `[..T::method()]` expansion, variadic
trait impls, and tuple `for-of` are implemented
([Variadic Type Parameters](./wep-2026-03-14-variadic-type-parameters.md));
`Reflect` — the compiler-synthesized projection of a struct's fields into a
typed tuple — is designed there (§10–11) and extended by
[Reflect Derivation](./wep-2026-06-13-reflect-derivation.md), but unbuilt.

This WEP settles two questions raised while reviewing that design:

1. Does struct walking need a new language primitive, in particular for nested
   structs?
2. How does field redaction interact with reflection? Today `#[secret]` affects
   only `Inspect`/`InspectAlt` and `wado doc`; serde serializes secret fields
   normally, so a redacted credential still leaks through `json::to_string`.
   Once any package can walk any struct via `Reflect`, this hole widens.

## Decision

### Struct walk is `Reflect::fields()` + tuple `for-of` — no new primitive

A struct walk is the existing tuple unrolling applied to the `Reflect`
projection:

```wado
fn walk_fields<T, ..F: Inspect>(s: &T) where T: Reflect<Fields = [..F]> {
    let names = T::field_names();
    for let [i, v] of s.fields().enumerate() {
        println(`{names[i]} = {v.inspect()}`);
    }
}
```

Fields are heterogeneous, so in each unrolled block `v` has type `F_k` and the
body can only use what the pack bound provides. A struct walk is therefore
always a walk with respect to some capability (`Inspect`, `Serialize`, a
user-defined visitor trait) — that is the meaningful form, matching Zig's
`inline for` over `@typeInfo` in a static, monomorphized, no-dynamic-reflection
model.

Direct `for let v of my_struct` (without `.fields()`) is rejected:

- It would be ambiguous with a struct implementing `IntoIterator`.
- It would make field declaration order silently semantic; reordering or adding
  a field changes behavior with no visible cue at the loop.
- The `.fields()` projection marks the walk as a deliberate act on a nominal
  type. Sugar can be added later without breaking anything.

### Nesting: trait recursion, not a recursive primitive

The walk primitive stays flat (depth 1). Nested structs are handled by a
blanket impl that recurses through the trait:

```wado
trait Walk {
    fn walk(&self, visitor: &mut Visitor);
}

impl Walk for i32    { fn walk(&self, v: &mut Visitor) { v.visit_int(*self); } }
impl Walk for String { fn walk(&self, v: &mut Visitor) { v.visit_str(self); } }

impl<T, ..F: Walk> Walk for T
where T: Reflect<Fields = [..F]>
{
    fn walk(&self, visitor: &mut Visitor) {
        let names = T::field_names();
        for let [i, f] of self.fields().enumerate() {
            visitor.enter_field(names[i]);
            f.walk(visitor);   // nested structs resolve to this same impl
            visitor.leave_field();
        }
    }
}
```

Coherence Rule 1 (non-variadic wins, WEP 2026-03-14 §5) resolves the leaf/node
split: a primitive field dispatches to its concrete impl, a struct field falls
through to the blanket impl and recurses. Termination is the ordinary generics
guarantee — monomorphization instances are memoized, so a recursive type
(`struct Node { next: Option<Node> }`) produces runtime recursion, not infinite
expansion; only type-growing recursion diverges, as anywhere else.

Consequently the only compiler work for struct walkability is finishing the
three unbuilt items already on the WEP 2026-03-14 / 2026-06-13 checklists:
`Reflect` per-struct synthesis, `where`-clause pack binding
`T: Reflect<Fields = [..F]>`, and coherence Rules 1–2.

### `#[secret]` — upgrade to a security contract

`#[secret]` today means only "not shown in inspect output". This WEP upgrades
it to: the field's value never flows out through any generic introspection
channel. "Display-only" redaction — hide from inspect but still serialize —
belongs to serde's own vocabulary (rename/skip), not here.

This is not a fourth rung on the visibility ladder. Visibility already has two
orthogonal axes
([Visibility](./wep-2026-06-25-visibility-internal-pub-export.md)): the scope
ladder (who can name a symbol) and the `export` CM flag. `#[secret]` is a third
orthogonal axis: whether the value can flow through generic channels. Direct
named access keeps following the scope ladder unchanged — credentials must be
readable by the code that uses them; `#[secret]` guards against incidental
exfiltration (logs, serialization, dumps, schemas), not against deliberate use
by authorized code.

Threat model, stated honestly: `#[secret]` seals the language-level generic
channels — `Inspect`, serde, and everything built on `Reflect`. It does not
seal the CM boundary (lowering a struct through an `export fn` signature is a
deliberate act by the type's owner, same rank as direct field access), host
memory inspection, or a debugger.

### `Secret<T>` — value-opaque, type-transparent projection

In the `Reflect` projection (and only there), a `#[secret]` field appears as
`Secret<F_k>` in the `Fields` pack. The struct declaration and direct field
access are unchanged: `self.password` is still `String`.

`Secret<T>` is compiler-synthesized with three properties:

- Value-opaque: no API returns the inner value; user code cannot define an
  unwrap. Legitimate code uses direct field access instead.
- Type-transparent: value-free static operations delegate to `T`
  (`Secret<T>::json_schema()` → `T::json_schema()`, `type_name`, …), so
  `[..F::method()]` type-level expansions — schema derivation in particular —
  keep working.
- Wrap-only: anyone can construct `Secret<T>` from a `T` (the inbound path
  `Deserialize` needs), no generic code can extract. An information diode.

This preserves the pack arity and the index correspondence with
`field_names()` / `field_meta()`. `FieldMeta` gains `secret: bool` so
value-level derivation code can branch on it.

### Channel consequences

Each channel's behavior follows mechanically from which traits `Secret<T>`
implements:

- `Inspect` / `InspectAlt`: unchanged — secret fields are omitted and a
  trailing `..` marks their presence. `assert` power-assert output goes through
  `Inspect`, so it is covered.
- `Serialize`: secret fields are skipped. For round-trip honesty a secret field
  must have a declared default (`f: T = expr`) or be `Option`; otherwise the
  struct is not serializable (compile error with a reason chain), because the
  emitted form could never deserialize back.
- `Deserialize`: reads secret fields normally — loading credentials from
  config/env is the canonical inflow and does not expose existing values.
- `Eq` / `Ord`: not auto-derived when any field is secret (equality is a
  guess-and-check oracle; comparing while excluding secret fields would be a
  semantic lie). The explicit `impl Eq for T;` marker is refused too. A manual
  impl is allowed — it uses direct field access under normal visibility, a
  deliberate act, and may choose constant-time comparison.

Why not plug the leak in `serde_synth` now: serde is slated to become
`Reflect`-based library code (WEP 2026-06-13 §5), so a secret-skip written into
the bespoke synthesizer is thrown away at that migration. The skip belongs in
the `Reflect`-based serde, where it reads `FieldMeta::secret` and is correct by
construction. The whole contract — serialize skip, `Eq`/`Ord` refusal,
`Secret<T>` — therefore lands as one change on top of `Reflect`, not dribbled
into the synthesizers ahead of the rest (which would leave a half-migrated
semantics: serialize still leaking while inspect is already hardened). The
accepted cost is that serde keeps serializing secret fields until the migration
lands — a known-open leak we choose over throwaway code.

One mechanism stays unresolved, shared by every generic derivation that must
drop a field: how to skip inside an unrolled loop when the untaken arm still
type-checks under the C++ template model. Candidates: a compile-time `if` that
drops the untaken arm, or inverting control so the value drives the entry via a
defaulted trait method that `Secret<T>` overrides to a no-op.

## Implementation checklist

The `#[secret]` attribute already exists with inspect-only semantics. Everything
below rides on the `Reflect` substrate, which this WEP does not own — it is the
three unbuilt items on the WEP 2026-03-14 / 2026-06-13 checklists (per-struct
`Reflect` synthesis, `where`-clause pack binding `T: Reflect<Fields = [..F]>`,
coherence Rules 1–2). Struct walkability needs nothing beyond them; the
`#[secret]` security upgrade needs the two staged items below.

- [ ] Extend `Reflect` to project a `#[secret]` field as `Secret<T>` in `Fields`
      and add `FieldMeta::secret` (blocked on `Reflect` synthesis).
- [ ] Land the `#[secret]` security upgrade in one step — not piecemeal in the
      bespoke synthesizers ahead of the rest: serialize skip (in the
      `Reflect`-based serde, reading `FieldMeta::secret`), require
      default/`Option` for serializability, and `Eq`/`Ord` auto-derive + marker
      refusal (a guard in trait synthesis). Resolve the unrolled-loop skip open
      question above. Deserialize is unchanged.
- [ ] Document the walk/nesting idiom in the cheatsheet once `Reflect` lands.

## Consequences

### Benefits

- Struct walkability costs no new language surface: one projection trait plus
  machinery that already exists; nesting falls out of coherence rules.
- The end state closes a leak serde has today (it serializes redacted fields)
  and gives credentials a contract that every future `Reflect` consumer
  inherits automatically — a library cannot forget to redact.
- The axis framing keeps the visibility model clean: name access, CM surface,
  and value flow stay independent.

### Trade-offs

- `fields(&self)` copies every field (value semantics). Read-only walks rely on
  value-copy elision
  ([Ownership Analysis](./wep-2026-05-21-resource-ownership.md)); a
  `fields_ref()` / `fields_mut()` reference projection is deferred until
  profiling demands it, since mutable walks also drag in write-back semantics.
- Deep walks monomorphize the whole tree; the code-size parity checks required
  by WEP 2026-06-13 §5 apply doubly here.
- A struct with a secret field loses derived `Eq`/`Ord` and (without a default)
  `Serialize`; users must opt back in manually. This friction is the point, but
  it is friction.
- The security upgrade is deferred to the `Reflect`-based serde rather than
  patched into `serde_synth` now, so secret fields stay serializable until the
  migration lands. Deliberate: an interim bespoke patch would be thrown away at
  that migration.

## Related WEPs

- [Compile-Time Tuple Enumeration](./wep-2026-02-10-compile-time-tuple-enumeration.md)
- [Variadic Type Parameters](./wep-2026-03-14-variadic-type-parameters.md)
- [Reflect Derivation](./wep-2026-06-13-reflect-derivation.md)
- [Visibility — `internal` / `pub` / `export`](./wep-2026-06-25-visibility-internal-pub-export.md)
- [Structured Logging and Tracing (`core:log`)](./wep-2026-06-25-core-log.md)
- [Serialization and Deserialization (Serde)](./wep-2026-02-28-serde.md)
