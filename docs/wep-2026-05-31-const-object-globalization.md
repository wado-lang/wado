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
`$initialize_module` runtime assignment, NIR optimization collapses builder
sequences, and by WIR the assignment is a `GlobalSet(G, value)` with `value`
fully lowered. The pass:

- Considers user-immutable globals (`g.mutable && !g.wado_mutable`), which are
  Wasm-mutable only because their init was extracted. `global mut` is excluded.
- Resolves each assignment through `is_const_expressible`, seeing through the
  builder-temp `Seq` (`$b = struct.new …; $b`) an array literal leaves. When
  every assignment to a global is constant, it moves the value into the global's
  eager `init`, marks it immutable, and drops the `GlobalSet`s.
- Recurses into nested instructions: an inlined `$initialize_module` puts its
  `GlobalSet` inside an `$inline___initialize_modules` guard block, duplicated
  per entry export, which a top-level-only scan would leave lazy.

`dce` / `cleanup` reclaim the emptied init body and the
`$modules_initialized` guard in the same phase. Promotion leaves the nullable
slot as `register_globals` set it. A non-null const init is a valid subtype of a
nullable slot.

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

Three shapes are matched, collected in a single exhaustive walk:

- A `let` binding of a qualifying value.
- A qualifying value referenced via `&` directly at an expression position with
  no enclosing `let` — the shape a call whose parameter is a reference takes
  (`digest::sha256(&"wado")`). It is rewritten in place: the `Unary::Ref`'s
  inner expression becomes `{ GlobalVarSet(G, …); GlobalVarGet(G) }`.
- A qualifying value passed to a call _by value_ — a constant header value
  handed straight to `Fields::append`. Rewritten in place like the `&` shape,
  wrapping the argument node itself, and gated additionally on the callee (see
  below).

The walk skips into a qualifying `let`'s own value and into a hoisted argument,
because hoisting both would nest one global's `GlobalVarSet` inside another's
initializer — a shape the single-assignment classifier cannot see through.

#### Gate: closed constant expression

`is_globalizable_const` requires a side-effect-free constant with no free
locals: literals, nested `Struct` / `Tuple` / `Array` / `Enum` / `Variant`
constructors, `PackedArray`, and the builder-temp block an array literal leaves.

A pure call on such constants qualifies too — it is deterministic and
side-effect free, so it is a closed constant expression in the same sense.
Purity comes from `optimize::mod_ref::FnEffect`, a per-callee summary resolved
as a least fixpoint over the call graph, tracking globals, linear memory and
component-model I/O. It deliberately excludes the GC heap: a callee that mutates
objects it allocated itself stays deterministic to its caller, and retention is
what would let a reference escape. Without that exclusion no `String`-building
function would qualify.

Reads of other globals are excluded — a non-const value cannot promote.

#### Gate: allocating initializer

A `let` whose value reduces to one local read through `&`, `*` and casts names
storage that already exists. Its global would alias whatever holds that storage,
and that binding is a candidate of its own, so the literal ends up in two
globals with one pointing at the other. `references_one_binding` declines it,
leaving the binding that does allocate to hoist. A `&String` argument reaching
an `S: AsStrSlice` parameter has exactly this shape once the blanket
`impl AsStrSlice for &T` inlines.

#### Gate: read-only

`is_readonly` requires every use to be a borrowing or reading position. It is
modelled on `value_copy_demote`'s element-immutability walk but stricter:
because the whole object is shared, even a spine mutation (`push`) corrupts it.
Any `&mut self` method, any `&mut` of a projection, and any assignment to the
binding or a projection disqualifies it.

A bare whole-value read in a consuming position (return, block tail, `let y =
xs`, an aggregate element, a by-value call argument) is also rejected: the
value-copy machinery may have elided the copy treating the binding as a movable
local, which globalizing would break. By-`&` borrows, field / index reads and
`&self` methods are admitted.

#### Gate: callee parameter, for a by-value argument

A by-value argument is handed over uncopied when the value is fresh — a literal
always is — so the callee receives the object itself, and globalizing makes that
object shared. `callee_param_readonly` therefore runs the same read-only walk
over the callee's own body, anchored at the parameter's local index. A parameter
the callee writes, stores, or returns fails it, and so does a callee with no
body (an import: nothing here can prove what it does with the value) or a
`&` / `&mut` parameter.

Read-only is not sufficient on its own. A by-value parameter is the callee's own
copy, so returning a projection of it (`return s.data`) is legitimate — the
return-convention fixpoint even calls the result _owned_, which is what lets the
caller skip a defensive copy of it. Hoisting the argument invalidates that
premise: the "owned" value handed back is the shared global's storage, and the
first mutation corrupts the constant. `param_storage_escapes` therefore also
rejects a callee that returns, stores, or passes on the parameter's storage,
following `let` aliases (`let r = s.data;`) since they name the same storage.

The pair is what keeps this shape sound on its own terms rather than by luck: a
mutating callee used to get a caller-side defensive copy that blocked the const
gate first, but that copy was itself removable — nothing else stands between a
shared global and a callee that writes it.

#### Gate: whole-program sharing, where the parameter gate refuses

The two gates above read one callee's body. A parameter the callee stashes away,
into a field of a struct it builds or into a further callee, fails them however
read-only the program as a whole is. `shared_escape` answers that case directly:
the constant is safe to share when nothing anywhere writes through it.

Taint names the shared object itself and never a container holding it, so a
projection of a tainted value is tainted while a struct built around one is not.
Storing a tainted value away raises an obligation on the slot it lands in — a
field name, a callee parameter, a callee result — and that slot's own reads are
then tainted in turn, program-wide. A write of anything tainted refuses the
query, and so does every shape the walk does not model. A field is keyed by its
name rather than by its receiver's type: a name is one key however the receiver
is spelled, so no newtype or reference wrapper hides a read from the scan.

A bodyless callee has no body for the walk to reach, so its parameter is
answered from what the declaration stated: `core:builtin` leaves the argument
where the caller put it when it takes the position by `&` rather than `&mut`,
and no `#[retain(p)]` clause names it. `#[retain(elements_of = p)]` keeps what
`p` holds rather than `p` itself, so it leaves the argument object alone. The
result is a way out of the call too, and no clause follows it: a return type
that can hold a reference refuses the argument unless `#[result(owned)]` states
that what comes back is freshly allocated. Only
`core:builtin` answers this way — `#[retain]` is already what the value-copy
plan trusts there, so a missing clause is a bug rather than a silence to read as
consent, which is what it would be on a CM import or a `.wasm` asset export.

Because "no write reaches this object" is a safety property, a query that
re-enters itself reads `true`: a cycle carrying no write of its own really does
hold. A verdict resting on that assumption is not cached, since a later
refutation of the cycle would leave it stale.

#### Gate: legality of the operand

Some builtins lower their argument to a Wasm immediate rather than to a value on
the stack, and codegen reads its literal out. `builtin::v128_const` is the first:
its bit pattern becomes the `v128.const` immediate. Replacing such an argument
with a global read is not a bad trade but a broken lowering.

The declaration says so, with `#[immediate(p)]` beside `#[retain]` and
`#[result]`, and the pass reads the declaration rather than matching on a name.
A builtin that gains an immediate operand later is covered the day it is
declared. Nothing hoists an argument at such a position, nor a `let` that
delivers one.

#### Gate: profitability

Hoisting costs a global, a guard branch, and an object that stays live for the
whole program. What it buys depends on what would otherwise become of the
constant, which is two different trades:

- **Retained** — a callee stores the constant into the heap, which is the case
  the sharing gate above admits. Each evaluation would add one more object to
  the live set, and the live set is what a collection walks. Hoisting collapses
  all of them into one, so it pays whatever the constant's shape.
- **Transient** — nothing keeps it past the use, so its allocation dies before
  the next collection and never costs a trace. Only the work of building it is
  saved. That is worth a global for a value owning heap storage — one that
  transitively owns a GC array, as `String` and `List` do, where the constructor
  fills every element — and not for an aggregate of scalars, which is a couple
  of field stores.

So the storage test decides the transient case alone. It does not carry over to
the retained one, where a `struct` of scalars is worth a global however cheap it
is to rebuild. `multi_value_return` is no argument against hoisting one either:
it hands such a value back in Wasm multi-values in return position, which says
nothing about an argument.

#### Lazy-init guard

An initializer the classifier cannot promote to an eager `init` leaves its
inline assignment standing, where, unguarded, it would re-run on every
activation. Three shapes are non-promotable and get the guard, decided by one
predicate (`needs_lazy_guard`) over the hoisted value — including any sibling
`let`s moved into it:

- a call, never Wasm-const-expressible;
- a `PackedArray` past its eager bound (`name::packed_array_is_eager`, the
  same choice `translate_packed_array` makes — `string_inline_max_bytes` for
  a `let`-shape global, `INLINE_REF_EAGER_MAX_BYTES` on top for an in-place
  one) whose repr is `array.new_data`, not a constant instruction;
- an `ArrayLiteral` of scalar constants at or past the `array.new_data`
  promotion threshold, which `promote_constant_arrays_to_data` rewrites out
  of const-expressibility before the classifier runs.

The guard is `if builtin::is_uninitialized(G)`, which reads the global's slot
at a nullable type and tests the `null` placeholder — the slot itself records
whether initialization has happened, so no companion flag global is needed.

The guard also pins the semantics: initialization happens at the first execution
of the expression it replaced, so a callee that traps or diverges still does so,
at the same point. Moving the work to module init would drag both to
instantiation time.

An initializer within the eager bounds keeps the unguarded shape, since the
classifier deletes its assignment outright.

#### Representation and scope

A global created from either in-place case — the inline `&` or the by-value
argument — is marked `NirGlobal::prefer_fixed_string_repr`, a field rather than
a name-prefix guess,
so it cannot misidentify a user-declared global sharing the pass's
`$const_obj_*` naming convention. WIR build gives only such a global's
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
- An extracted global's value is readable to `niri` from the assignment that
  fills the slot, not from the placeholder in it, so a derived scalar global
  (`global B = A + 10`) and a hoisted constant aggregate alike fold at their use
  sites — field and element reads down to scalars, then branch pruning, then DCE
  of the global nobody reads. This is the cross-function constant propagation
  intra-function SROA cannot reach.
- Cost: a marginally larger global section for constants a path may never reach,
  acceptable given no access-time overhead.

## Known gaps

- Two of `shared_escape`'s conservative answers have no fixture: a promoted
  operand whose taint the walk cannot name, and a parameter position the
  callee's body declares nothing for. Both refuse, so the gap is coverage and
  not correctness, and neither is steerable from Wado source today.

  A fixture reaches this analysis at all only when the callee survives
  inlining, since an inlined one leaves no parameter to ask about. Making the
  callee bulky is what does it — the three `shared_escape_stashed_*` fixtures
  all do, and each was verified against the trace rather than assumed.

- A value carried out of an `ExprKind::Switch` has no fixture either. The
  program that would show it needs the switch arms to differ, because CSE folds
  identical ones back to the field read the taint already follows.
- Only `core:builtin` answers the bodyless-callee question. A Component Model
  import and a `.wasm` asset export always refuse, however read-only they are,
  because `#[retain]` is not complete on them the way it is on `core:builtin`.
  Closing it means deciding what an absent clause means on those two, which is a
  language question rather than a pass one.
- `#[immediate(p)]` is read by `const_object_globalization` alone. Any later
  pass that would substitute an argument has to consult it too, and nothing
  makes it. The declarations are complete as of the SIMD lane operands and
  `v128_const`, which are the only positions codegen reads a literal from.
- The hoist is priced per constant, never per program. A module whose every
  constant is retained hoists all of them, and nothing bounds what that adds to
  the module-lifetime live set — which the WasmGC cost model says is the tax
  paid at every collection.
