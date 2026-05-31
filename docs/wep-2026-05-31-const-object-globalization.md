# WEP: Constant Object Globalization

## Context

Wado has value semantics: a struct/array/tuple literal constructs a fresh heap
object every time it is evaluated. When such a literal is constant-shaped (all
fields are recursively constant) and only read, re-constructing it on every
function entry — or every loop iteration — is pure waste.

The compiler already hoists non-constant user `global` initializers into a
runtime `__initialize_module` / `__initialize_modules` path, guarded by a single
`__modules_initialized` flag (see [Global Variables](./wep-2026-01-27-global-variables.md)).
But the Wasm-instantiation-time path is narrow: `emit_const_expr`
(`codegen/emit.rs`) only emits scalar constants and `ref.null`, with a
`_ => i32_const(0)` fallback. Even an all-constant struct such as
`global ORIGIN: GPoint = GPoint { x: 0, y: 0 }` is forced onto the runtime path.

Wasm 3.0 GC permits aggregate construction in constant init expressions
(`struct.new`, `struct.new_default`, `array.new`, `array.new_default`,
`array.new_fixed`, `array.new_data`, `array.new_elem`, `ref.i31`, `ref.func`).
This lets the engine build constant objects once at instantiation, with no init
function and no flag check on access.

Today "what counts as a constant init" is duplicated across three layers:
`is_constant_initializer` (`lower/plan/globals.rs`, TIR plan),
`translate_global_init` (`wir_build`, NIR→WIR), and `emit_const_expr` (codegen).
The codegen fallback is also a policy decision that violates the crate principle
"codegen emits the `Package` as is."

## Decision

Globalize constant, read-only aggregate values into instantiation-time Wasm
const globals. Implement at the WIR layer, split into a mechanical codegen
capability and a single optimizer policy pass.

### Layering

- Capability (codegen). `emit_const_expr` becomes a faithful recursive
  structural emitter of any const-expressible `WirInstr` tree: scalar consts,
  `ref.null`, `ref.i31`, `ref.func`, `struct.new`, `array.new_fixed`,
  `array.new_default`, `array.new_data`. It carries no policy — it emits whatever
  the optimizer placed in `WirGlobal.init`. The `_ => i32_const(0)` fallback
  becomes an ICE: a non-const instruction in an init slot is an optimizer bug.
- Policy (optimizer). One WIR pass owns the decision of what becomes a const
  global and performs the rewrite. `is_constant_initializer` and
  `translate_global_init` stay scalar-only; aggregates keep flowing through the
  existing lazy-init path and are promoted by this pass. The constant-shape
  recognizer then lives once, over `WirInstr`.

Why WIR, not TIR/NIR. `emit_const_expr` produces `wasm-encoder::ConstExpr`
bytes — a codegen concern that cannot move up. The transformation cannot live at
TIR/NIR either: array literals only acquire a hoistable const form
(`array.new_fixed` / `array.new_data`) after the WIR array passes; at NIR they
are still `SequenceLiteralBuilder` push sequences. WIR is the first layer where
struct, array, and string constants share a uniform const-expressible shape, and
where value-copy elision has already settled which uses are read-only (a
surviving copy is an explicit `struct.new` reconstruction by WIR time).

### Constant-aggregate predicate

A `WirInstr` is a const-init expression iff it is recursively one of: scalar
const (`I32/I64/F32/F64Const`), `RefNull`, `RefI31(const)`, `RefFunc`,
`StructNew { fields: all const }`, `ArrayNewFixed { elements: all const }`,
`ArrayNewDefault { len: const }`, `ArrayNewData { offset/len: const }`.
Everything else (calls, locals, arithmetic beyond extended-const) is non-const.
`global.get` is deliberately excluded, so a const init never references another
global — this avoids the core-Wasm restriction (init may reference only imported
immutable globals) and any global-ordering constraint.

### Part 1 — lazy-to-const global demotion

Generalizes `const_global_promotion` to aggregates, at WIR. After the array
passes run, scan each module init function for `GlobalSet { g, value }` where `g`
is immutable (not user `global mut`) and `value` satisfies the predicate. Move
`value` into `WirGlobal.init`, clear `lazy_init`, restore the slot to its
non-null type, and drop the `GlobalSet`. This upgrades all-constant user globals
(`GPoint { x: 0, y: 0 }`, constant arrays, constant strings) from runtime
construction to instantiation-time const globals. This part has no aliasing
hazard: the global was already a shared singleton; only its construction time
moves.

### Part 2 — body constant globalization

Hoist constant aggregate `WirInstr` trees out of function bodies into fresh
immutable const globals, replace the occurrence with `global.get`, and
deduplicate identical trees (keyed by a structural hash).

Soundness. Replacing a fresh allocation with a shared reference is valid only
when the value is used read-only and does not escape into a context that assumes
freshness (notably `return`, where the caller treats a call result as fresh and
adds no defensive copy). Value semantics guarantees every mutating or
freshness-dependent consumer already carries an explicit copy by WIR time, so the
remaining hazard is in-place mutation or escape of the hoisted value itself. v1
restricts candidates to constant aggregates bound to a WIR local that the
existing read-only / escape analysis (the predicates behind
`elide_*_struct_locals`) proves is never written, never `&mut`-aliased, and never
returned or stored. The motivating win — a constant object reconstructed each
loop iteration — falls out as module-scope LICM. Aggressive cases
(inline-subexpression hoist, cross-function dedup, escape through value-copied
call args) are staged follow-ups.

Synthetic globals are named via the `name.rs` mangling utilities, internal
(non-`pub`, not exported), and immutable.

### Scope boundary

Values whose construction runs user code (constructor logic, computed fields,
effectful or non-deterministic calls) or references another global are not
const-init expressions and stay on the runtime path. Those are loop-invariant
runtime values, served by `licm` / `tmpl_hoist`, not module globalization.
Because const globals carry near-zero init cost, no per-object init flag and no
hotness heuristic is needed.

## Consequences

- Constant objects are built once at instantiation instead of per call / per
  iteration; reads become a bare `global.get` with no flag check.
- Identical constants are pooled into a single global.
- Codegen stays mechanical: policy moves out of `emit_const_expr`, and the
  constant-shape recognizer is defined once over `WirInstr`.
- No per-object init flag and no extension of the runtime `__initialize_modules`
  path is required.
- Slightly larger global section and a marginal instantiation-time cost for
  constants a given execution may never reach — acceptable given the per-use
  savings and the absence of any access-time overhead.
- Requires confirming the pinned `wasmparser` / `wasmtime` generation accepts GC
  aggregate constant init expressions; enforced by E2E `wir_expect` fixtures
  asserting globalized constants appear as const globals (not in
  `__initialize_module`).

## TODO

- [ ] Make `emit_const_expr` a recursive structural emitter; turn the fallback
      into an ICE.
- [ ] Define the `WirInstr` const-aggregate predicate (single source of truth).
- [ ] Part 1: lazy-to-const global demotion WIR pass.
- [ ] Part 2: body constant globalization with read-only / escape gating.
- [ ] E2E fixtures (`wir_expect` / `wir_not_expect`) for structs, arrays, and
      strings at each `-Ox`.
