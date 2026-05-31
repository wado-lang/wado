# WEP: Constant Object Globalization

## Context

Wado has value semantics: a struct/array/tuple literal builds a fresh heap object
every time it is evaluated. A constant-shaped, read-only literal rebuilt on every
call — or loop iteration — is pure waste.

Two problems block fixing this:

- Narrow capability. `emit_const_expr` (`codegen/emit.rs`) emitted only scalar
  consts and `ref.null` (an `_ => i32.const 0` fallback), so even
  `global ORIGIN: GPoint = GPoint { x: 0, y: 0 }` could not be an
  instantiation-time const global — though Wasm 3.0 GC allows `struct.new` /
  `array.new_fixed` / `array.new_data` / … in constant init expressions.
- Scattered knowledge (smell). "Is this init a Wasm constant?" is encoded
  redundantly across `is_constant_initializer` (TIR, `lower/plan`),
  `translate_global_init` (NIR→WIR), `emit_const_expr` (codegen), and
  `is_scalar_constant` (`const_global_promotion`). The eager/lazy split is decided
  prematurely in `lower/plan` (syntactic, pre-optimization), then partially undone
  by `const_global_promotion` — a guess-lazy-then-recover round trip.

## Decision

Three coordinated changes, plus two enablers owned by other WEPs.

### Capability — recursive `emit_const_expr`

`emit_const_expr` is a faithful recursive emitter of any const-expressible
`WirInstr` tree (scalar consts, `ref.null` / `ref.i31` / `ref.func`, `struct.new`
/ `array.new_fixed` / `array.new_default` / `array.new_data`) via
`ConstExpr::extended`. The fallback is now an ICE: a non-const instruction in an
init slot is an optimizer bug. Codegen holds no policy — it emits whatever WIR put
in the slot.

### Global initialization — single boundary classifier

Make the NIR→WIR boundary the sole authority on eager-vs-lazy, via one function:

```
try_const_init(&NirExpr, ctx) -> Option<WirInstr>
```

- NIR carries each global as `{ name, type, init-expr }` and optimizes init
  expressions in place. NIR has no lazy concept and no `__initialize_module`.
- `Some(tree)` → eager Wasm const global (Wasm-immutable iff the user wrote
  `global`, not `global mut`).
- `None` → lazy: `wir_build` synthesizes the runtime init for the residue only —
  `__initialize_module` / `__initialize_modules`, the `__modules_initialized`
  guard, dependency topo-sort, and the entry-point call.

This is the de-smell. It removes `is_constant_initializer` (TIR),
`const_global_promotion` (NIR), and `NirGlobal.{lazy_init, is_nullable}`, and
relocates `extract` / `build_initialize_modules` from `lower/plan` to `wir_build`.
`try_const_init`'s `Some` set is the single const predicate; `emit_const_expr`
mirrors it structurally. It accepts, recursively: scalar literals, `Null`, `Unit`,
`String` / `Bytes`, `StructLiteral` / `TupleLiteral` / `ArrayLiteral` /
`VariantConstruct` with const children, and `EnumConstruct`. It excludes
`GlobalVarGet`, so a const init never references another global — sidestepping the
core-Wasm const-expr restriction and any init ordering.

Lazy-ness thus becomes a function of how far NIR-optimize simplified the init,
decided once. Because globals keep their real init expression (never a premature
placeholder), `-O0` stays correct and unpessimised: literal inits classify eager,
unfolded ones (`A + 10`, builder-shaped arrays) lazy — exactly "lazy iff optimize
could not simplify it."

### Body globalization — NIR pass

Hoist constant-aggregate `NirExpr` trees out of function bodies into fresh
immutable globals (named via `name.rs`, internal), replacing each occurrence with
`GlobalVarGet` and deduplicating identical trees across functions.

- Placement: late in the fixed-point loop, after `container_sroa` / `sroa` /
  `const_fold`, so decomposable constants are scalarised away first and only
  whole-value residue is hoisted.
- Soundness: sharing one instance for a fresh allocation is valid only when the
  value is read-only and does not escape into a freshness-assuming context
  (`return`, store into a longer-lived aggregate). Value semantics already inserts
  an explicit copy at every mutating consumer, so the residual gate is on the
  hoisted local itself: not in `address_taken_locals` / `stores_aliased_locals`,
  `has_field_mutation == false`, never reassigned, not returned or stored. v1
  targets named `let` bindings.
- Value: a constant used only via field reads in one function already folds away
  via `niri`'s `field_env`; the pass targets constants used as whole values,
  duplicated across functions, or rebuilt in loops — the SROA residue and the
  loop-invariant-construction hole LICM (field reads only) and `tmpl_hoist`
  (strings only) miss.

### Enablers (other WEPs)

- `NirExprKind::ArrayLiteral` + const push-sequence collapse — without it,
  arrays/strings reach NIR as builder push-sequences and stay lazy until WIR. See
  [NIR Layer WEP](./wep-2026-05-11-nir.md) (Additions).
- `niri` folding `G.field` / `G[const]` on immutable aggregate globals — turns
  globalization into a cross-function constant-propagation source (globalize →
  fold reads to scalars module-wide → DCE removes the now-unread global). See
  [niri Evolution WEP](./wep-2026-04-27-nir-interpreter.md) (Stage 6).

## Consequences

- Constant objects build once at instantiation; reads are a bare `global.get`,
  no flag check.
- The const predicate lives once (`try_const_init`), codegen is mechanical, and
  the guess-lazy round trip plus three of the four duplicated predicates are gone.
- With the `niri` enabler, aggregate-global fields/elements propagate as
  module-wide constants and often fold the object away entirely.
- Cost: a larger global section and a marginal instantiation-time cost for
  constants a path may never reach — acceptable given no access-time overhead.
- Relocating `__initialize_module` synthesis to `wir_build` is the main effort;
  the existing `global_*` e2e fixtures are the safety net. Requires confirming the
  pinned `wasmparser` / `wasmtime` accepts GC aggregate const init expressions.

## TODO

Feature:

- [x] `emit_const_expr` recursive emitter; fallback → ICE.
- [ ] `try_const_init` at the NIR→WIR boundary; relocate `extract` /
      `build_initialize_modules` to `wir_build`; delete `is_constant_initializer`,
      `const_global_promotion`, `NirGlobal.{lazy_init, is_nullable}`.
- [ ] Optimize global init expressions in place during NIR optimize.
- [ ] `const_object_globalization` NIR pass — read-only / escape gating,
      cross-function dedup, after SROA / `const_fold`.
- [ ] E2E `wir_expect` / `wir_not_expect` fixtures (struct / array / string, each
      `-Ox`): const globals are not in `__initialize_module`; field-fold + DCE
      removes globals whose every use is a field read.

Cross-cutting (other WEPs):

- [ ] `NirExprKind::ArrayLiteral` + collapse — [NIR Layer WEP](./wep-2026-05-11-nir.md).
- [ ] `niri` aggregate `GlobalEnv` + `G.field` / `G[const]` projection —
      [niri Evolution WEP](./wep-2026-04-27-nir-interpreter.md) (Stage 6).
