# WEP: Constant Object Globalization

## Context

Wado has value semantics: a struct / array / tuple literal builds a fresh heap
object every time it is evaluated. A constant-shaped, read-only value rebuilt on
every call — or every loop iteration — is pure waste, and Wasm 3.0 GC allows
`struct.new` / `array.new_fixed` / `array.new_default` in constant initializer
expressions, so such a value can be built once at instantiation instead.

## Decision

Const-ness is decided once, by one predicate, in one WIR pass that runs after
NIR optimization has simplified initializers — "lazy iff optimize could not
simplify it". A NIR pass feeds that machinery by hoisting qualifying values out
of function bodies into globals.

### The const predicate — `WirInstr::is_const_expressible`

One recursive predicate (`wir.rs`) is the authority on const-ness for global
initializers. It accepts scalar consts, `ref.null` / `ref.i31` / `ref.func`,
`struct.new` / `array.new_fixed` / `array.new_default` with const children, and
a transparent `ref.as_non_null` wrapper (aggregate constructors wrap non-null
ref fields in it but already yield a non-null ref, so it is dropped in const
context).

It excludes `global.get`, keeping a const init clear of the core-Wasm
const-expr ordering restriction, and `array.new_data` / `array.new_elem`, which
read a segment at runtime and are not valid Wasm constant instructions.

Codegen's `push_const_instrs` emits exactly this set. A node that reaches the
emitter and fails the predicate is an ICE, never a silent `i32.const 0`.

### String representation

A string literal lowers to `StructLiteral String { repr: PackedArray(bytes),
used: <len> }`, a bytes literal to the same shape over `List<u8>`;
`ExprKind::PackedArray` is a raw constant `Array<u8>`. Strings and bytes are
therefore ordinary const aggregates, with no string-specific code in the passes
below.

`PackedArray`'s WIR lowering picks the repr by size. A string of at most
`NirPackage::string_inline_max_bytes` UTF-8 bytes gets a constant
`array.new_fixed<u8>` repr — one `i32.const` per byte — and registers no data
segment, so it can promote to an eager const global. A longer string keeps the
compact `array.new_data` repr and stays lazy, since spelling every byte as an
operand would bloat code unboundedly.

The threshold is opt-level-driven (`optimize::string_inline_max_bytes`): 4 bytes
by default, including `-Os`, and 8 at `-O3`. It is measured to be roughly
size-neutral — `array.new_fixed` of N bytes offsets the dropped data segment and
its header — so it tunes how many string globals go eager rather than overall
size.

### The classifier — `wir_optimize::const_global`

`promote_const_global_inits` runs in WIR phase 7, before guard removal.
`lower/plan/globals::extract` emits every non-trivial initializer as an
`__initialize_module` runtime assignment, NIR optimization collapses builder
sequences, and by WIR the assignment is a `GlobalSet(G, value)` with `value`
fully lowered. The pass:

- Considers user-immutable globals (`g.mutable && !g.wado_mutable`), which are
  Wasm-mutable only because their init was extracted. `global mut` is excluded.
- Resolves each assignment through `is_const_expressible`, seeing through the
  builder-temp `Seq` (`__b = struct.new …; __b`) an array literal leaves. When
  every assignment to a global is constant, it moves the value into the global's
  eager `init`, marks it immutable, and drops the `GlobalSet`s.
- Recurses into nested instructions: an inlined `__initialize_module` puts its
  `GlobalSet` inside an `__inline___initialize_modules` guard block, duplicated
  per entry export, which a top-level-only scan would leave lazy.

`init_guard` / `dce` / `cleanup` reclaim the emptied init body and the
`__modules_initialized` guard in the same phase. Promotion leaves `lazy_init`
and the nullable slot as `register_globals` set them — a non-null const init is
a valid subtype of a nullable slot.

The classifier sits at WIR because the value is already correctly lowered there
— variant representation, non-null field wrapping and builder collapse all baked
in — so it reuses the real translator's output instead of re-translating a NIR
aggregate. Keeping `extract` in place also keeps lazy initializers flowing
through the TIR `lower/plan` boxing / closure / value-copy passes they depend
on; a const init needs none of those.

### Body globalization — `const_object_globalization`

This NIR pass (`optimize/const_object_globalization.rs`) hoists a qualifying
value out of a function body into a shared immutable global. It runs once after
the optimizer fixpoint converges, on the stable post-inline shape.

It emits a Wasm-mutable / Wado-immutable global with a `null` placeholder init,
mirroring `extract`, plus an inline `GlobalVarSet` where the value was built,
and rewrites the binding's reads to `GlobalVarGet`. The classifier above then
promotes the global and drops the assignment. Soundness therefore rests on the
gates alone: a value that turns out not to be const-expressible merely leaves
the global assigned at runtime, still correct.

Two shapes are matched, collected in a single exhaustive walk:

- A `let` binding of a qualifying value.
- A qualifying value referenced via `&` directly at an expression position with
  no enclosing `let` — the shape a synthesized `serde` field key takes
  (`st.field(&"id_str", …)`). It is rewritten in place: the `Unary::Ref`'s inner
  expression becomes `{ GlobalVarSet(G, …); GlobalVarGet(G) }`.

The walk skips into a qualifying `let`'s own value, because hoisting both would
nest one global's `GlobalVarSet` inside another's initializer — a shape the
single-assignment classifier cannot see through.

#### Gate: closed constant expression

`is_globalizable_const` requires a side-effect-free constant with no free
locals: literals, nested `Struct` / `Tuple` / `Array` / `Enum` / `Variant`
constructors, `PackedArray`, and the builder-temp block an array literal leaves.

A pure call on such constants qualifies too — it is deterministic and
side-effect free, so it is a closed constant expression in the same sense.
Purity comes from `optimize::mod_ref::FnEffect`, a per-callee summary resolved
as a least fixpoint over the call graph, tracking globals, linear memory and
component-model I/O. It deliberately excludes the GC heap: a callee that mutates
objects it allocated itself stays deterministic to its caller, and `stores` is
what would let a reference escape. Without that exclusion no `String`-building
function would qualify.

Reads of other globals are excluded — a non-const value cannot promote.

#### Gate: read-only

`is_readonly` requires every use to be a borrowing or reading position. It is
modelled on `value_copy_demote`'s element-immutability walk but stricter:
because the whole object is shared, even a spine mutation (`push`) corrupts it.
Any `&mut self` method, any `&mut` of a projection, and any assignment to the
binding or a projection disqualifies it.

A bare whole-value read in a consuming position (return, block tail, `let y =
xs`, an aggregate element) is also rejected: the value-copy machinery may have
elided the copy treating the binding as a movable local, which globalizing would
break. By-`&` borrows, by-value arguments, field / index reads and `&self`
methods are admitted.

#### Gate: profitability

Hoisting costs a global, a guard branch, and an object that stays live for the
whole program, so it is restricted to values that own heap storage — those that
transitively own a GC array, as `String` and `List` do. A small aggregate of
scalars owns nothing: `multi_value_return` already lifts such a return into Wasm
multi-values and allocates nothing, so hoisting it would trade zero allocations
for a global.

#### Lazy-init guard

A call initializer is never Wasm-const-expressible, so the classifier cannot
promote it to an eager `init`; the inline assignment survives and, unguarded,
re-runs on every activation. Such an assignment is wrapped in
`if builtin::is_uninitialized(G)`, which reads the global's slot at a nullable
type and tests the `null` placeholder — the slot itself records whether
initialization has happened, so no companion flag global is needed.

The guard also pins the semantics: initialization happens at the first execution
of the expression it replaced, so a callee that traps or diverges still does so,
at the same point. Moving the work to module init would drag both to
instantiation time.

A literal initializer keeps the unguarded shape, since the classifier deletes
its assignment outright.

#### Representation and scope

A global created from the inline-`&` case is marked
`NirGlobal::prefer_fixed_string_repr` — a field rather than a name-prefix guess,
so it cannot misidentify a user-declared global sharing the pass's
`__const_obj_*` naming convention. WIR build gives only such a global's
`GlobalVarSet` value a size-bounded override
(`name::INLINE_REF_EAGER_MAX_BYTES`, 64 bytes) of `string_inline_max_bytes`, so
a realistic field name promotes eager without forcing arbitrarily large literals
eager too. `wir_optimize::prune_dead_data` drops any passive data segment
speculatively registered for a literal that ends up wholly `array.new_fixed`.

The pass is gated off any `wasi:*`-namespaced module: `wir_build::register_globals`
asserts a `NirGlobal` never has a WASI `module_source`, so a hoisted global in a
WASI-binding helper fails loudly at build time instead of dangling silently.

Only values that survive optimization are reachable targets. A const struct that
is only field-read is scalarized away by SROA before this pass runs, so the
prime beneficiaries are a constant `List` / `Array` indexed dynamically in a
loop, and a pure call building a heap value from literals.

## Consequences

- Constant struct / array / tuple globals build once at instantiation; reads are
  a bare `global.get` with no init flag check.
- The const predicate lives in one place and codegen mirrors it.
- Short string globals are eager via a constant `array.new_fixed<u8>` repr;
  longer ones stay lazy.
- A derived scalar global (`global B = A + 10`) is promoted to an eager const,
  but its reads no longer fold at use sites: `niri`'s `GlobalEnv` keys on
  Wasm-mutability (`const_folding::build_global_env`), so an extracted global is
  `NonConst` to the interpreter.
- Cost: a marginally larger global section for constants a path may never reach,
  acceptable given no access-time overhead.

## TODO

- [ ] Fold user-immutable globals' reads from their initializers (`niri`
      `GlobalEnv` keyed on `wado_mutable`) — overlaps
      [niri Evolution WEP](./wep-2026-04-27-nir-interpreter.md) (Stage 6).
- [ ] `niri` folding `G.field` / `G[const]` on immutable aggregate globals —
      see [niri Evolution WEP](./wep-2026-04-27-nir-interpreter.md) (Stage 6).
