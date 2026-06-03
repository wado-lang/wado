# WEP: Constant Object Globalization

## Context

Wado has value semantics: a struct/array/tuple literal builds a fresh heap object
every time it is evaluated. A constant-shaped, read-only literal rebuilt on every
call — or loop iteration — is pure waste. In particular a
`global ORIGIN: GPoint = GPoint { x: 0, y: 0 }` was initialized at runtime inside
`__initialize_module` even though Wasm 3.0 GC allows `struct.new` /
`array.new_fixed` / `array.new_default` in constant init expressions.

Two problems blocked fixing this:

- Narrow capability. The const-expr emitter handled only scalar consts and
  `ref.null` (an `_ => i32.const 0` fallback), so aggregate literals could not be
  instantiation-time const globals.
- Scattered knowledge (smell). "Is this init a Wasm constant?" was encoded
  redundantly across `is_constant_initializer` (TIR, `lower/plan`),
  `translate_global_init` (NIR→WIR), the codegen emitter, and `is_scalar_constant`
  (the NIR `const_global_promotion` pass) — each with a different, narrower
  accepted set.

## Decision

A constant global initializer is promoted to an eager Wasm constant by one WIR
pass, gated by one predicate. The eager/lazy split is decided once, after NIR
optimization has simplified the init — "lazy iff optimize could not simplify it."

### One const predicate — `WirInstr::is_const_expressible`

A single recursive predicate (`wir.rs`) is the authority on const-ness for global
initializers. It accepts: scalar consts, `ref.null` / `ref.i31` / `ref.func`,
`struct.new` / `array.new_fixed` / `array.new_default` with const children, and a
transparent `ref.as_non_null` wrapper (aggregate constructors wrap non-null ref
fields in it, but already yield a non-null ref, so it is dropped in const
context). It excludes `global.get` (a const init never references another global,
sidestepping the core-Wasm const-expr ordering restriction) and `array.new_data`
/ `array.new_elem` — these read a data/elem segment at runtime and are _not_ valid
Wasm constant instructions. Codegen's `push_const_instrs` emits exactly this set
and mirrors the predicate; a node reaching the emitter that fails the predicate is
an ICE, not a silent `i32.const 0`.

This corrects the original premise: a `String`'s `array.new_data<u8>` repr cannot
be an eager const global, so string globals stay lazy.

### One classifier — `wir_optimize::const_global`

The eager/lazy decision lives in a single WIR pass, `promote_const_global_inits`,
run in WIR phase 7 (before guard removal). `lower/plan/globals::extract` still
emits each non-trivial init as an `__initialize_module` runtime assignment, and
NIR optimization (`array_literal`, `string_push`, `const_folding`, …) collapses
builder sequences. By WIR the assignment is `GlobalSet(G, value)` with `value`
already fully lowered. The pass:

- Considers user-immutable globals (`g.mutable && !g.wado_mutable`) — currently
  Wasm-mutable only because their init was extracted. `WirGlobal` carries
  `wado_mutable` so `global mut` is excluded.
- Resolves each assignment to a constant via `is_const_expressible`, seeing
  through the builder-temp `Seq` (`__b = struct.new …; __b`) an array literal
  leaves. If every assignment to a global is constant, it moves the value into the
  global's eager `init`, marks it immutable, and drops the `GlobalSet`(s).
- Recurses into nested instructions: when a small `__initialize_module` is inlined
  into the entry points, its `GlobalSet` lands inside an
  `__inline___initialize_modules` guard block and is duplicated once per entry
  export; a top-level-only scan would silently leave the global lazy.

The emptied init body and `__modules_initialized` guard are reclaimed by
`init_guard` / `dce` / `cleanup` in the same phase. This subsumes the scalar-only
NIR `const_global_promotion`, which is deleted — const-ness is now decided once.

Promotion keeps `lazy_init` and the nullable slot as `register_globals` set them:
a non-null const init is a valid subtype of a nullable slot, and codegen keeps
narrowing reads with `ref.as_non_null`, correct since the eager value is non-null.

### Why WIR-level, not a NIR→WIR `try_const_init`

An earlier design put a `try_const_init` classifier at the NIR→WIR boundary and
relocated `extract` / `build_initialize_modules` into `wir_build`. Two findings
redirected it to a WIR pass:

- Removing `extract` would route lazy (non-const) init code around the TIR
  `lower/plan` boxing / closure / value-copy passes it depends on — those run
  before NIR and process the synthesized `__initialize_module`. Const inits don't
  need them (a const init can't contain `&mut`/closures), but lazy ones do.
- At WIR the value is already correctly lowered — variant representation, non-null
  field wrapping, builder collapse all baked in — so the pass reuses that lowering
  instead of re-translating a NIR aggregate (which would risk divergence from the
  real translator). The decision is still made once, post-optimization.

An earlier draft planned to give `register_globals` a path to lower an
aggregate-literal initializer directly, so a body-globalized global could carry
an eager aggregate `init`. Body globalization instead routes through the existing
`__initialize_module`-style promotion (an inline `GlobalVarSet` promoted by
`wir_optimize::const_global`), so this path is not needed and was never added —
`translate_global_init` still handles only scalar / `null` inits. See "Body
globalization — landed" below.

### Body globalization — landed

`const_object_globalization` (a NIR pass,
`optimize/const_object_globalization.rs`) hoists constant-aggregate `let`
bindings out of function bodies into shared immutable globals. It was deferred
until the original soundness blocker was removed: at `-O2`, `arr[i] = v` lowered
to a builtin `array_set(arr.repr, i, v)`, a mutation through the `arr.repr`
projection that a "every use is a projection read" gate misread as a read.
[Phase 1 of the `builtin::array` redesign](./wep-2026-06-02-builtin-array-redesign.md)
gave the `builtin::array_*` ops `&` / `&mut` first parameters, so the mutation is
now an explicit `&mut arr.repr` the gate rejects.

Two independent gates:

- Closed const aggregate (`is_globalizable_const`). The initializer must be a
  side-effect-free constant with no free locals: literals, nested
  `Struct` / `Tuple` / `Array` / `Enum` / `Variant` constructors, and the
  builder-temp block (`{ let __b = …; *__b }`) an array literal leaves. Strings
  (`array.new_data`), `BytesLiteral`, calls, and reads of other globals are
  excluded — a free local or side effect would make hoisting to module scope
  unsound, and a non-const value cannot promote.
- Read-only (`is_readonly`). Every use must be a borrowing / reading position.
  Modelled on `value_copy_demote`'s element-immutability walk but stricter:
  because the _whole_ object is shared, even a spine mutation (`push`) corrupts
  it, so any `&mut self` method, any `&mut` of a projection, and any assignment
  to the binding or a projection disqualifies it. A bare whole-value read in a
  consuming position (return, block tail, `let y = xs`, an aggregate element) is
  also rejected — the value-copy machinery may have elided the copy treating `xs`
  as a movable local, which globalizing would break. By-`&` borrows, by-value
  arguments (the callee gets an independent copy), field / index reads, and
  `&self` methods are admitted.

The pass runs once after the optimizer fixpoint converges, on the stable
post-inline shape. It does **not** re-translate the aggregate or extend
`register_globals`: it emits a fresh Wasm-mutable / Wado-immutable global with a
`null` placeholder init (mirroring `extract`) plus an inline `GlobalVarSet` at
the original `let`, and rewrites the binding's reads to `GlobalVarGet`. The
existing `wir_optimize::const_global` classifier then promotes the global to an
eager Wasm constant and drops the assignment — reusing that machinery wholesale,
so soundness rests on the read-only gate alone: if a value turns out not to be
WIR-const-expressible, the global merely stays lazy-assigned each call, still
correct. (This supersedes the earlier idea of a `register_globals` aggregate-init
path, which is therefore not needed.)

Only aggregates that _survive_ optimization are reachable targets: a const struct
that is only field-read is scalarized away by SROA before this pass runs, so the
prime beneficiary is a constant `List` / `Array` indexed dynamically in a loop
(the lookup-table case), which cannot be scalarized.

### Enablers (other WEPs)

- `NirExprKind::ArrayLiteral` + const push-sequence collapse — landed; without it
  arrays reach NIR as builder push-sequences. See
  [NIR Layer WEP](./wep-2026-05-11-nir.md) and
  [`NirExprKind::ArrayLiteral` WEP](./wep-2026-05-31-nir-array-literal.md).
- `niri` folding `G.field` / `G[const]` on immutable aggregate globals — not
  landed. See [niri Evolution WEP](./wep-2026-04-27-nir-interpreter.md) (Stage 6).

## Consequences

- Constant struct/array/tuple globals build once at instantiation; reads are a
  bare `global.get`, no init flag check.
- The const predicate lives once (`WirInstr::is_const_expressible`), codegen
  mirrors it, and the scalar-only NIR promotion plus its duplicate predicate are
  gone.
- String globals stay lazy (data-segment repr is not const-expressible).
- Known limitation: a derived scalar global (`global B = A + 10`) is still
  promoted to an eager const, but its _reads_ no longer fold at use sites the way
  the in-loop NIR pass enabled — `niri`'s `GlobalEnv` keys on Wasm-mutability
  (`const_folding::build_global_env`), so an extracted (Wasm-mutable) global is
  `NonConst` to the interpreter. Keying that on `wado_mutable` would fold
  user-immutable globals' reads from their initializers directly; it overlaps the
  niri Stage 6 enabler and is left as a follow-up.
- Cost: a marginally larger global section for constants a path may never reach —
  acceptable given no access-time overhead.

## TODO

Landed:

- [x] `WirInstr::is_const_expressible` — single const predicate; codegen
      `push_const_instrs` mirrors it (fallback → ICE). `array.new_data` excluded.
- [x] `wir_optimize::const_global::promote_const_global_inits` — single classifier
      (recurses into inlined/duplicated init blocks); `WirGlobal.wado_mutable`
      added; NIR `const_global_promotion` deleted.
- [x] `const_object_globalization` NIR pass — hoists constant, read-only
      aggregate `let` bindings into shared immutable globals via an inline
      `GlobalVarSet` that `wir_optimize::const_global` promotes. Two gates
      (`is_globalizable_const`, `is_readonly`); the `&mut`-aware read-only gate
      is unblocked by builtin-array Phase 1. E2E fixture
      `const_object_globalization` (loop-indexed `List` globalized; mutated array
      and returned/escaping bindings left alone).
- [x] E2E fixtures: `const_global_object` (struct/array eager, string lazy),
      `const_global_entry` (inlined-init promotion).

Deferred:

- [ ] Fold user-immutable globals' reads from their initializers (`niri`
      `GlobalEnv` keyed on `wado_mutable`) — overlaps
      [niri Evolution WEP](./wep-2026-04-27-nir-interpreter.md) (Stage 6).
