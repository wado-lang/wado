# WEP: Constant Object Globalization

## Context

Wado has value semantics: a struct/array/tuple literal constructs a fresh heap
object every time it is evaluated. When such a literal is constant-shaped (all
fields are recursively constant) and only read, re-constructing it on every
function entry — or every loop iteration — is pure waste.

The compiler already hoists non-constant user `global` initializers into a
runtime `__initialize_module` / `__initialize_modules` path, guarded by a single
`__modules_initialized` flag (see [Global Variables](./wep-2026-01-27-global-variables.md)).
The instantiation-time path is narrow: `emit_const_expr` (`codegen/emit.rs`) only
emits scalar constants and `ref.null` (with an `_ => i32_const(0)` fallback), so
even an all-constant struct such as `global ORIGIN: GPoint = GPoint { x: 0, y: 0 }`
is forced onto the runtime path. Wasm 3.0 GC permits aggregate construction in
constant init expressions (`struct.new`, `array.new_fixed`, `array.new_data`,
`ref.i31`, `ref.func`, …), so the engine can build such objects once at
instantiation with no init function and no flag check on access.

NIR is Wado's primary optimizer, run as a fixed-point loop (`optimize.rs`). Two
facts make NIR the right home for this work:

- The NIR interpreter (`niri`) already carries a `GlobalEnv` that folds
  `GlobalVarGet` of an immutable scalar global to its constant value, and a
  per-function `field_env` that folds `FieldAccess(Local, f)` of a struct literal
  to the field constant. These two environments are not yet connected for global
  field access.
- `const_global_promotion` already demotes a runtime-initialized immutable global
  to a compile-time constant once its initializer folds to a scalar.

Globalizing a constant at NIR is therefore not merely "allocate once": connected
to `niri`, it becomes a module-wide constant-propagation source that compounds
with `const_fold`, `const_global_promotion`, `branch_prune`, and DCE in the same
fixed-point loop. A WIR placement would be terminal — no fixed point, no `niri` —
and would forgo all of that.

## Decision

Implement constant object globalization as a NIR optimization pass (plus one
codegen capability and one `niri` keystone extension).

### Layering

- Capability (codegen, layer-agnostic). `emit_const_expr` becomes a faithful
  recursive structural emitter of any const-expressible `WirInstr` tree (scalar
  consts, `ref.null`, `ref.i31`, `ref.func`, `struct.new`, `array.new_fixed`,
  `array.new_default`, `array.new_data`). The `_ => i32_const(0)` fallback becomes
  an ICE: a non-const instruction in an init slot is an optimizer bug.
  `translate_global_init` (NIR→WIR build, today scalar-only → `RefNull`
  placeholder) gains the matching recursive aggregate translation.
- Policy (NIR). One shared const-aggregate predicate over `NirExpr`, consumed by
  Part 1 and Part 2; one `niri` keystone extension. Codegen stays mechanical and
  holds no policy.

### Enabler: first-class constant arrays at NIR

Add `NirExprKind::ArrayLiteral` and collapse `SequenceLiteralBuilder` push
sequences into it during NIR optimization (porting WIR's
`collapse_array_push_sequences` earlier in the pipeline). Without this, arrays and
strings carry no const-recognizable shape until WIR. The collapse runs early in
the fixed-point loop (alongside `container_sroa`) so later passes see the literal
form. This pays off beyond this feature: CSE of arrays, constant-index folding,
and bounds-check elimination all benefit from a first-class constant array.

### Constant-aggregate predicate

A `NirExpr` is a const-init expression iff it is recursively one of: scalar
literal (`IntLiteral` / `FloatLiteral` / `BoolLiteral` / `CharLiteral`), `Null`,
`Unit`, `StringLiteral` / `BytesLiteral`, `ArrayLiteral { all const }`,
`StructLiteral { all fields const }`, `TupleLiteral { all const }`,
`VariantConstruct { payload const }`, `EnumConstruct`. Everything else (calls,
locals, arithmetic beyond extended-const) is non-const. `GlobalVarGet` is
deliberately excluded, so a const init never references another global — avoiding
the core-Wasm const-expr restriction (init may reference only imported immutable
globals) and any global-ordering constraint. Defined once and reused by Part 1,
Part 2, and `translate_global_init`; `emit_const_expr` mirrors it structurally.

### Part 1 — user-global const-init (generalize `const_global_promotion`)

Extend `const_global_promotion`'s predicate from `is_scalar_constant` to the
const-aggregate predicate. The pass already scans `__initialize_module` for
`GlobalVarSet { g, value }` with a constant `value` and demotes `g` to an
immutable const-init global, dropping the `GlobalSet`. Generalizing the predicate
upgrades all-constant user globals (`GPoint { x: 0, y: 0 }`, constant arrays,
constant strings) from runtime construction to instantiation-time const globals,
with no new pass. This part has no aliasing hazard: the global was already a
shared singleton; only its construction time moves. (Arrays require the
`ArrayLiteral` enabler to be recognized at NIR; the collapse runs earlier in the
loop, and the fixed point lets the demotion fire on a later iteration.)

### Part 2 — body constant globalization (new pass)

Hoist constant-aggregate `NirExpr` trees out of function bodies into fresh
immutable globals, replace each occurrence with `GlobalVarGet`, and deduplicate
identical trees (structural-hash keyed, across functions).

Placement: late in the fixed-point loop, after the SROA / scalarization passes
(`container_sroa`, `sroa`) and `const_fold`, so decomposable constants are
scalarized away first and only the whole-value residue is hoisted.

Soundness: replacing a fresh allocation with a shared reference is valid only when
the value is read-only and does not escape into a freshness-assuming context —
notably `return` (the caller treats a call result as fresh and adds no defensive
copy) or a store into a longer-lived aggregate. Value semantics guarantees every
mutating or freshness-dependent consumer already carries an explicit copy, so the
residual hazard is in-place mutation or escape of the hoisted value itself. Gate
with the analyses already present at NIR: the bound local is not in
`address_taken_locals`, not in `stores_aliased_locals`, has
`has_field_mutation == false` (`value_copy_elide`'s usage analysis), is never
reassigned, and is not returned or stored. v1 targets named `let` bindings;
inline-subexpression hoisting and escape-through-value-copied call arguments are
staged follow-ups.

A constant used only via field reads within a single function is already folded
away by `niri`'s `field_env`, so it needs no global. Part 2's value is for
constants used as whole values, duplicated across functions, or built inside
loops — the SROA residue, plus the loop-invariant-construction hole that LICM
(field reads only) and `tmpl_hoist` (strings only) do not cover.

Synthetic globals are named via the `name.rs` mangling utilities, internal
(non-`pub`, not exported), and immutable.

### Keystone — fold aggregate-global access in `niri`

Extend `niri`'s `GlobalEnv` beyond scalar `Lattice` to carry a structural
snapshot of immutable aggregate globals, and fold
`FieldAccess(GlobalVarGet(G), f)` and `Index(GlobalVarGet(G), const)` to the
field/element constant — mirroring `field_env`'s local folding. This converts
globalization into a cross-function constant-propagation source: globalize → fold
field/element reads to scalars module-wide → feed `const_global_promotion` /
`const_fold` / `branch_prune` → the global often becomes dead and DCE removes it,
leaving pure scalar propagation that SROA (intra-function) could never achieve
across function boundaries. Without this keystone, NIR globalization still yields
cross-function dedup and loop-invariant-construction hoisting, but not the
compounding cascade.

### Interaction summary

| Interaction                                                      | Effect                                                    | Basis                                                                   |
| ---------------------------------------------------------------- | --------------------------------------------------------- | ----------------------------------------------------------------------- |
| `niri` `GlobalEnv` + `field_env` connected for aggregate globals | strong positive (new cross-function constant propagation) | infrastructure exists, only unconnected                                 |
| Loop-invariant _construction_ hoisting                           | strong positive (fills a real hole)                       | LICM hoists field reads only; `tmpl_hoist` is string-only               |
| Cross-function aggregate dedup                                   | strong positive (new capability)                          | `cse` handles scalars/binary only; aggregates return `None`             |
| `inline` exposes constant constructions (often into loops)       | positive                                                  | place the pass after `inline` in the loop                               |
| SROA / scalarization tension                                     | neutral via ordering; self-heals via keystone             | run after SROA; field reads on a mis-hoisted global fold + DCE          |
| Escape precision                                                 | available at NIR                                          | `address_taken_locals` / `stores_aliased_locals` / `has_field_mutation` |

### Scope boundary

Values whose construction runs user code (constructor logic, computed fields,
effectful or non-deterministic calls) or that reference another global are not
const-init expressions and stay on the runtime path, served by `licm` /
`tmpl_hoist`. Because const globals carry near-zero init cost, no per-object init
flag and no hotness heuristic is needed.

## Consequences

- Constant objects are built once at instantiation; reads become a bare
  `global.get` with no flag check.
- With the keystone, aggregate-global fields/elements propagate as module-wide
  constants and compound in the fixed point; objects frequently fold away
  entirely, leaving cross-function scalar propagation.
- Identical constants are pooled into a single global across functions.
- `NirExprKind::ArrayLiteral` and the NIR collapse benefit CSE, constant-index
  folding, and bounds-check elimination beyond this feature.
- Codegen stays mechanical: policy is out of `emit_const_expr` (fallback → ICE),
  and the constant-shape recognizer is defined once over `NirExpr`.
- Slightly larger global section and a marginal instantiation-time cost for
  constants a given execution may never reach — acceptable given the per-use
  savings and the absence of any access-time overhead.
- The NIR collapse duplicates logic currently in WIR
  (`collapse_array_push_sequences`); the two should share a helper or the WIR pass
  should be retired once the NIR `ArrayLiteral` subsumes it.
- Requires confirming the pinned `wasmparser` / `wasmtime` generation accepts GC
  aggregate constant init expressions; enforced by E2E `wir_expect` fixtures.

## TODO

- [ ] `emit_const_expr`: recursive structural emitter; fallback → ICE.
- [ ] `translate_global_init`: recursive aggregate translation (NIR→WIR).
- [ ] `NirExprKind::ArrayLiteral` + NIR push-sequence collapse (early in the loop).
- [ ] Shared const-aggregate predicate over `NirExpr`.
- [ ] Part 1: generalize `const_global_promotion` to aggregates.
- [ ] Part 2: `const_object_globalization` pass — read-only / escape gating,
      cross-function dedup, placed after SROA / `const_fold` in the loop.
- [ ] Keystone: `niri` `GlobalEnv` aggregate snapshots; fold `G.field` / `G[const]`.
- [ ] E2E `wir_expect` / `wir_not_expect` fixtures for structs, arrays, and
      strings at each `-Ox`; assert const globals (not `__initialize_module`), and
      that field-fold + DCE removes the global where every use is a field read.
