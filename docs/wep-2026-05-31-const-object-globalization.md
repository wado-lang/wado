# WEP: Constant Object Globalization

## Context

Wado has value semantics: a struct/array/tuple literal constructs a fresh heap
object every time it is evaluated. When such a literal is constant-shaped (all
fields are recursively constant) and only read, re-constructing it on every
function entry — or every loop iteration — is pure waste.

The compiler already hoists non-constant user `global` initializers into a
runtime `__initialize_module` / `__initialize_modules` path, guarded by a single
`__modules_initialized` flag (see [Global Variables](./wep-2026-01-27-global-variables.md)).
But the Wasm-instantiation-time path is narrow: `is_constant_initializer`
(`lower/plan/globals.rs`) only accepts scalar literals, and `emit_const_expr`
(`codegen/emit.rs`) only emits scalar constants and `ref.null`. Even an
all-constant struct such as `global ORIGIN: GPoint = GPoint { x: 0, y: 0 }` is
forced onto the runtime path today.

Wasm 3.0 GC permits aggregate construction in constant init expressions
(`struct.new`, `struct.new_default`, `array.new`, `array.new_default`,
`array.new_fixed`, `array.new_data`, `array.new_elem`, `ref.i31`, `ref.func`).
This lets the engine build constant objects once at instantiation, with no init
function and no flag check on access.

## Decision

Globalize constant, read-only aggregate values into instantiation-time Wasm
const globals. Two coordinated changes:

### 1. Widen constant initialization

Grow the recognizer and emitter together so they agree on what is constant:

- `is_constant_initializer` accepts struct / array / tuple literals whose fields
  are recursively constant (extending the existing scalar/cast/neg cases).
- `emit_const_expr` recurses over the `WirInstr` tree, emitting GC const
  instructions (`struct.new`, `array.new_fixed`, `array.new_data`, …) for the
  shapes the recognizer now admits.

This alone upgrades existing all-constant `global` declarations from the runtime
path to instantiation-time const globals.

### 2. Globalize constant body expressions

A new NIR pass (sibling to `const_global_promotion`) hoists constant-shaped
aggregate expressions out of function bodies:

- Candidate: any aggregate expression (named binding or inline literal) whose
  initializer is recursively constant and effect-free, i.e. expressible as a
  Wasm const init expression.
- Safety: every use site must be read-only — no field mutation, no `&mut`, no
  address-taken / closure capture / store aliasing. This reuses the escape
  analysis already driving `value_copy_elide` / SROA
  (`has_field_mutation`, `address_taken_locals`, `stores_aliased_locals`).
- Rewrite: create a synthetic immutable global, replace each occurrence with
  `global.get`, and deduplicate identical constants into one global.

The read-only requirement is exactly the condition under which `value_copy` is
elidable, so the defensive copy on each shared read disappears automatically —
that elision is the payoff that makes sharing one instance observationally
identical to per-occurrence construction.

### Scope boundary

Values whose construction runs user code (constructor logic, computed values,
effectful or non-deterministic calls) are not constant expressions and are out
of scope. Those are loop-invariant runtime values, better served by in-function
hoisting (`licm`, `tmpl_hoist`) than by module globalization. Because
instantiation-time const globals carry near-zero init cost, no runtime
init-function path and no hotness heuristic is needed for the in-scope cases.

## Consequences

- Constant objects are allocated once at instantiation instead of per call /
  per iteration; reads become a bare `global.get` with no flag check.
- Identical constants are pooled into a single global.
- No per-object init flag and no extension of the runtime
  `__initialize_modules` path is required for this feature.
- Slightly larger global section and a marginal instantiation-time cost for
  constants that a given execution may never reach — acceptable given the
  per-use savings and the absence of any access-time overhead.
- `emit_const_expr` gains a recursive code path; the recognizer and emitter must
  stay in lockstep, enforced by E2E `wir_expect` fixtures asserting that
  globalized constants appear as const globals (not in `__initialize_module`).
