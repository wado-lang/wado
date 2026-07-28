# WEP: Canonical Type Identity for Synthesized Helpers

Design for [#1588](https://github.com/wado-lang/wado/issues/1588). Scope: the
identity of `$value_copy$` helpers and the array-clone element edge. The
canonicalizer it introduces is general; adopting it for the other
mangle-as-identity families is deliberately left out of scope (see
[Later adopters](#later-adopters)).

## Context

A `$value_copy$T` helper is synthesized once per value-semantic type
(`lower/plan/value_copy/synthesize.rs`) and referenced from two kinds of site:

- an ordinary call, emitted by the fold or rewritten from a
  `builtin::copy_value::<T>` marker — resolved through
  `ValueCopyPlan::name_for_type: IndexMap<TypeId, (ModuleSource, String)>`;
- a per-element edge of `builtin::array_clone::<T>`, which is not a call in any
  IR. Codegen emits the clone loop and calls the helper between `array.get` and
  `array.set`.

The second kind has no id to travel on, so it travels as a rendered string —
`TypeTable::mangle_type_arg_for_generic` — recomputed independently at each
phase that must recognise the edge:

| Site                                      | Role                                      |
| ----------------------------------------- | ----------------------------------------- |
| `lower/plan/value_copy/synthesize.rs:165` | names the helper; dedups helpers by name  |
| `wir_build/functions.rs:506`              | stamps `WirFunction::value_copy_mangle`   |
| `wir_build/calls.rs:185`                  | stamps `ArrayClone::element_copy_mangle`  |
| `optimize/dce.rs:300-372`                 | NIR DCE virtual rooting (a fixpoint loop) |
| `wir_optimize/dce.rs:37-60,192`           | WIR DCE rooting                           |
| `wir_optimize/util.rs:58-84,285`          | SROA pinning + the mangle→index map       |
| `codegen/emit.rs:52,663,2310,2714`        | `ArrayClone` helper resolution            |

Seven derivations of one fact. Two of them (`optimize/dce.rs`,
`wir_optimize/util.rs`) exist only to rebuild a map that the phase that _knew_
the answer already had.

### The mangle is not the defect; identity-by-rendering is

The mangle is load-bearing in a way any replacement must preserve: it is a pure
function of type _structure_, so a `TypeId` the synthesizer never saw still
resolves to the right helper. That property is required, because `TypeTable`
interns one logical type under several ids by design — a `GenericInstance` and
its monomorphized `Struct` coexist, and `List`/tuples stay `GenericInstance` and
re-intern whenever their argument ids diverge.

The defect is that the structural key is a _display rendering_, and a rendering
is not injective:

| Rendered as                                               | Loses                                                                                                                                                               |
| --------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Fn<{param_count},{ret}>`                                 | every parameter type; `ret` unqualified                                                                                                                             |
| `Reactive<{inner}>`                                       | `inner`'s module (falls to `mangle_type_name`)                                                                                                                      |
| `{name}<{args}>` for a generic resource                   | the base name's module                                                                                                                                              |
| `{module}/{name}`, `Name<a,b>`, `[a,b]`, `Array<x>`, `&x` | escaping — no grammar separates a name containing `/`, `<`, `,` from a structural delimiter, and monomorphized `Struct::name` _is_ a rendered generic (`List<i32>`) |

Whether a given collision miscompiles today depends on a second coincidence: the
WIR type registry keys on the _same_ function, so when two types collapse there,
they collapse consistently in both places and nothing validates as invalid. A
collision becomes a miscompile exactly when the two renderings _disagree_ —
which is what the `Array<Foo>` regression was (the array arm delegated to the
unqualified `mangle_type_name` while the type registry used the qualified form),
and why `mangle_type_arg_for_generic` now carries 50 lines of doc comment asking
every site to agree byte-for-byte. Correctness rests on a convention that
nothing checks.

### The structural link monomorphization throws away

`make_monomorphized_struct(name, module, base_name)` interns
`Struct { name: "List<i32>", base_name: Some("List") }`. The type _arguments_ are
not stored — they survive only inside the rendered `name`. Everything that later
needs them recovers them by re-rendering and comparing strings:

```rust
// TypeTable::generic_type_args — for a monomorphized struct
for tid in self.iter_type_ids() {
    if ... && gi_name == base_name && self.mangle_type_name(tid) == *name { ... }
}
```

A full-table scan, rendering every type, to answer "what are this struct's type
args" (`find_type_args_by_mangled_name` is the same scan without the base-name
filter). This is the root of the whole issue: the moment a `GenericInstance`
becomes a `Struct`, the only surviving link between them is a string, so every
consumer that must treat the two as one type is forced back onto strings.

## Decision

Three rules, in dependency order.

1. **Structured instantiation is never discarded.** A monomorphized struct
   records the base and the argument `TypeId`s it was built from.
2. **Identity is an interned id, not a rendering.** One authority hash-conses
   type structure into a `TypeKey`; the `$value_copy$` name becomes a
   human-readable label derived from it, which no consumer parses or rebuilds.
3. **A cross-phase function reference is a function reference.** The array-clone
   element edge stops being a name to look up and becomes a `FuncId` /
   `WirFuncId` — the same channel every other call uses — so the generic
   call-graph walkers see it and no phase needs the type→helper map at all.

## Architecture

### `TypeKey` — hash-consed structural identity

New module `wado-compiler/src/type_canon.rs`, holding an interner independent
of `TypeTable`'s id space:

```rust
pub struct TypeKey(u32);

pub struct TypeCanon {
    nodes: IndexSet<CanonNode>,       // position == TypeKey
    memo: IndexMap<TypeId, TypeKey>,
}

enum CanonNode {
    Primitive(PrimitiveType),
    Unit,
    Never,
    Nominal { kind: NominalKind, module: ModuleSource, name: Rc<str> },
    Instance { module: ModuleSource, name: Rc<str>, args: Vec<TypeKey> },
    Tuple(Vec<TypeKey>),
    Array(TypeKey),
    Ref(TypeKey),
    MutRef(TypeKey),
    Function { params: Vec<TypeKey>, ret: TypeKey, is_mut: bool },
    Reactive(TypeKey),
    // …one node per `ResolvedType` variant; no catch-all arm.
}

impl TypeCanon {
    pub fn key(&mut self, tt: &TypeTable, id: TypeId) -> TypeKey;
}
```

Properties, each of which the rendering lacks:

- **Injective by construction.** Distinct structure ⇒ distinct node ⇒ distinct
  key. There is no flattening step in which a name and a delimiter can be
  confused, no erased component (function parameter types are carried), and no
  unqualified nominal.
- **Structure-pure.** `key` reads only `ResolvedType`, so an unseen duplicate
  `TypeId` for a seen structure returns the seen key. This is the property the
  mangle had and the reason a plain `TypeId` cannot replace it.
- **Normalizing across the monomorphization boundary.** `Struct` carrying a mono
  origin canonicalizes to the same `Instance` node as its `GenericInstance`
  form. This is what makes the two intern paths one identity — and it needs
  rule 1, because the args are otherwise unavailable without parsing the name.
- **Allocation-free after the first visit.** A memo hit is one hash lookup; the
  current code allocates a recursive `String` per rooting and per codegen site.

`TypeCanon` is owned by the package (`FlatPackage` → `NirPackage`), beside
`type_table`, and is built lazily. Pruning: DCE prunes the type table, so the
memo must be dropped or filtered at the same point; by the end of Phase 4 no
post-lowering phase calls `key` at all, which retires the concern.

### `ValueCopyRegistry` — the single authority

`ValueCopyPlan::name_for_type` becomes:

```rust
pub struct ValueCopyRegistry {
    helper: IndexMap<TypeKey, FuncId>,   // assigned once, at synthesis
    label: IndexMap<TypeKey, Rc<str>>,   // display only
}
```

Synthesis assigns; everyone else asks by key. The current dedup-by-generated-name
set in `synthesize_helpers` disappears — a second `TypeId` for a synthesized
structure is a key hit.

The registry also removes a silent miss. `wrap_value_copy` today falls through
to _no copy_ when `name_for_type` lacks the `TypeId`, which for a
duplicate-interned id is a missed deep copy (aliasing), not a collision. Keyed by
structure, that lookup cannot miss for a structurally-synthesized type; the
residual case (a type minted after the seed walk) becomes an explicit decision
— on-demand synthesis or an ICE — rather than a silent behavioral fork.

### Reference plumbing

| Edge                                  | Today                         | After                                     |
| ------------------------------------- | ----------------------------- | ----------------------------------------- |
| fold / `copy_value` marker → helper   | `TypeId` → name → interner    | `TypeKey` → `FuncId`                      |
| NIR `array_clone::<T>` → helper (DCE) | mangle map + fixpoint         | ordinary call edge (Phase 4)              |
| WIR `ArrayClone` → helper             | `element_copy_mangle: String` | `element_copy: Option<WirFuncId>` (Ph. 3) |
| WIR helper self-identification        | `value_copy_mangle: String`   | deleted                                   |
| codegen `ArrayClone` → wasm index     | private mangle→index map      | existing `func_index_map`                 |

`WirFuncId` resolution at WIR build costs nothing new: functions are all
registered in Step 2 before any body is translated in Step 3, and
`WirContext::funcid_map` already maps `nir::FuncId → WirFuncId`, which is how
every stamped call resolves.

Once `ArrayClone` carries a `WirFuncId`, the two bespoke WIR collectors
(`wir_optimize/dce.rs::collect_array_clone_helper_refs`,
`wir_optimize/util.rs::collect_array_clone_helpers`) and the
`value_copy_helper_mangles` map delete outright: the edge is picked up by
`collect_func_refs_recursive` / `collect_ref_funcs`, and compaction remaps it in
`remap_func_ids` — one arm each, in walkers that already exist.

### Phase 4 — the element edge becomes an ordinary call

After Phase 3, one special case survives: NIR DCE must still root a helper
reachable only through an `array_clone::<T>` site, because the reference does not
materialize until WIR build. The structural fix is to give the deep clone a
body instead of an instruction.

`needs_value_copy(Array<T>)` is already `true`, so `$value_copy$Array<T>` already
exists — its body is `array_clone::<T>(&v)`, and the loop that codegen emits
_is_ that helper's body inlined at every site. Synthesize the loop as the
helper's TIR body instead:

```
$value_copy$Array<T>(v: Array<T>) -> Array<T> {
    let dst = array_new::<T>(array_len(&v));
    let mut i = 0;
    while i < array_len(&v) {
        // slots past `used` are null in a List repr, and `dst` is already
        // default-initialized, so a null slot is skipped
        if let Some(e) = array_get_nullable::<T>(&v, i) {
            array_set(&mut dst, i, copy_value::<T>(e));
        }
        i = i + 1;
    }
    return dst;
}
```

Every other `array_clone::<T>` site with value-typed `T` lowers to a call to
this helper (one rewrite site, in `lower/translate`'s `convert_call`, beside the
existing `copy_value` rewrite). Consequences:

- the element edge is an ordinary `copy_value` marker → an ordinary call. NIR
  DCE's rooting block, its fixpoint, and its `TODO(optimizer)` delete;
- `WirInstr::ArrayClone` keeps only the shallow/bulk form
  (`build_bulk_array_clone`), and codegen's deep branch — including the
  `__copy_arr_elem` scratch-local bookkeeping — deletes;
- the loop becomes visible to the NIR/WIR optimizers (inlining, LICM, BCE)
  rather than being emitted opaquely by codegen, and is shared per type instead
  of duplicated per site.

`array_get_nullable::<T>(&Array<T>, i32) -> Option<T>` is the one new intrinsic:
an `array.get` whose result is typed nullable. `Option<ref>` already lowers to a
null niche (`wir_optimize/nullable_ref.rs`), and `synthesis/effect_dispatch.rs`
already synthesizes `TirExprKind::Null` against a `make_option(ref)` type, so
this is an existing representation, not a new one.

This phase is gated (see [Risks](#risks-and-mitigations)); Phases 1–3 stand on
their own if it is dropped.

## Implementation

Each phase is independently shippable and independently revertible.

- [ ] P0 — pin the invariant, red first:
      a unit test over a corpus of structurally-distinct types asserting
      `key(a) == key(b) ⟺ structurally_equal(a, b)`, covering the vectors the
      rendering loses (`fn(i32)->A` vs `fn(String)->A`; same-named nominals from
      two modules under `Reactive` / a generic resource / a function return; a
      nominal whose name contains `<`, `,`, or `/`); a debug assertion in
      synthesis that no two distinct keys produce the same helper label (the
      convention `mangle_type_arg_for_generic`'s doc comment asks for and nothing
      enforces); and an e2e fixture per reachable vector (`tests/fixtures/`, with
      `sub/` modules for the cross-module halves). A vector that cannot be made
      to miscompile today still earns a fixture — it pins behavior the
      canonicalizer must keep.
- [ ] P1 — structured mono origin. Add the instantiation to the monomorphized
      struct: `type_args: Vec<TypeId>` on `ResolvedType::Struct`, or a
      `mono_origin` side table on `TypeTable` if the enum's `Hash`/`Eq` identity
      must stay as-is (the side table loses interning-by-origin; the field costs
      a `Vec` on every struct type — decide when writing it). Three call sites
      mint these. Reroute `generic_type_args` to read it, deleting the full-table
      name-matching scan; `find_type_args_by_mangled_name` follows. A strict
      improvement on its own.
- [ ] P2 — `TypeCanon` + `ValueCopyRegistry`. Introduce the interner and the
      registry; convert synthesis and the two fold lookups. The seven mangle
      sites keep working — this phase changes _who computes_ identity, not what
      travels.
- [ ] P3 — ids across WIR. `ArrayClone::element_copy_mangle` →
      `Option<WirFuncId>`; delete `WirFunction::value_copy_mangle`, the codegen
      map and its resolver, the two WIR collectors, and
      `value_copy_helper_mangles`. NIR DCE roots through the registry by key —
      still a virtual edge, but one lookup against one authority instead of a
      locally rebuilt string map and its fixpoint. `wir_unparse` prints the
      callee's fq name from the function table so dumps stay readable; golden WIR
      fixtures churn here.
- [ ] P4 — the real call edge (above), gated on benchmark parity.
- [ ] P5 — cleanup. `mangle_type_arg_for_generic` keeps its remaining callers and
      its role as the _naming_ function; its doc comment loses the "all sites
      must agree" contract for value copy. `name::value_copy_helper_name` takes
      the label, not the identity.

### Later adopters

`$case_extract$`, `$case_construct$`, `$variant_tag$`, `$field_get$`, and the
monomorphizer's `InstantiationKey` all key on the same rendering and can move to
`TypeKey` one at a time. None is in scope here; the point of Phases 1–2 is that
the authority exists for them to adopt.

## Rejected alternatives

- **Deduplicate the interning instead.** The issue's own constraint:
  `List`/tuples staying `GenericInstance`, and monomorphization needing raw
  `GenericInstance`s, are intentional. Canonicalization is the layer that
  reconciles intentional duplicates without collapsing them.
- **Keep the string, make it injective** — escape delimiters, qualify every
  nominal, carry function parameter types. Cheapest patch, and it re-buys a
  property that must then be re-verified at every future edit to a
  display-shaped function. It also keeps seven derivations and the per-site
  allocation.
- **Canonical `TypeId` via union-find in `TypeTable`.** Merging duplicate ids
  into a representative changes what every existing `TypeId` consumer sees,
  including the monomorphizer that needs the duplicates. A side interner touches
  nobody who does not ask for it.
- **Registry keyed by the existing mangle string.** One authority, no
  injectivity gain, still allocating. Half the fix at nearly the full cost of
  the plumbing.

## Consequences

### Benefits

- Identity is assigned once and referenced by id; the "must agree byte-for-byte"
  contract across seven sites is gone.
- Injectivity is a property of the representation, not of review discipline, and
  Phase 0 pins it with a test.
- Deletes: two WIR collectors, one codegen map and resolver, one DCE fixpoint,
  one full-table name-matching scan in `TypeTable`, and (Phase 4) codegen's deep
  `ArrayClone` branch.
- No recursive string allocation on rooting or codegen paths.
- The missed-copy fall-through in `wrap_value_copy` becomes an explicit decision.

### Trade-offs

- One more interner in the package. It is a memo over the type table, so it must
  be invalidated wherever the table is pruned — bounded, because after Phase 4 no
  post-lowering phase queries it.
- Phase 1 costs a `Vec<TypeId>` per monomorphized struct type (or a side table).
- Phase 4 turns an inline loop into a call: better code size and optimizer
  visibility, one call per clone site of runtime cost.
- Golden WIR fixtures churn in Phases 3 and 4.

### Risks and mitigations

- **Phase 4 regresses a hot path.** `List`/`String` clone is on the serde, JSON,
  and zlib paths. Gate on `mise run benchmark-all` and
  `mise run report-wasm-size`; if the call is not recovered by inlining, keep
  Phase 3's state, which already removes the strings from WIR and codegen.
- **The synthesized loop gets extra copies from the fold.** Helper bodies are
  already special-cased in the ownership/confinement analyses
  (`confine.rs::Kind::ValueCopy`); the loop's `array_set(.., copy_value(e))`
  shape must be checked against that, with a WIR fixture asserting exactly one
  element copy.
- **Mono-origin normalization is incomplete**, leaving a `Struct` and its
  `GenericInstance` on two keys — two helpers, one label, and a function-registry
  collision. The Phase 0 label-uniqueness assertion catches this at synthesis,
  before it reaches codegen.
- **`array_get_nullable` interacts with the null-niche lowering.** Confine it to
  synthesis (not a surface builtin) and cover it with a fixture over a `List<T>`
  whose capacity exceeds its length — the exact shape the current codegen
  null-branch exists for.

## See also

- `docs/compiler.md` — pipeline phases
- `docs/optimizer.md` — NIR/WIR DCE
- `docs/wep-2026-06-13-reference-representation.md` — reference vs value shapes
- Issue [#1588](https://github.com/wado-lang/wado/issues/1588)
