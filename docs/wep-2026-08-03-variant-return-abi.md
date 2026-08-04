# WEP: Variant Return Scalarization at NIR

## Context

A function returning a `variant` — overwhelmingly `Result<T, E>` and
`Option<T>` — is compiled by `wir_build` as a single GC struct result. Every
`return Ok(v)` allocates. `wir_optimize::sroa_variant_return` then rediscovers
the pattern from WIR shapes and unpicks that boxing into a flat
`[i32 discriminant, payload…]` multi-value result.

Everything about that is late. The scalarization happens after every NIR pass
has run, so no NIR pass ever sees through a `Result`-returning call: to
`const_fold`, `dce`, `sroa`, `store_load_forward`, and `inline` alike, the
result is one opaque boxed value. The point of moving the work to NIR is not to
relocate it — it is to let it compose with the rest of the optimizer, which is
where the compounding wins are.

That goal decides the shape of the change, and it rules out the obvious design.

## Decision

### A `ReturnAbi` marker cannot deliver the goal

The natural reading of issue #1742 is: add `ReturnAbi::Variant` beside
`ReturnAbi::MultiValue`, classify variant returns in
`optimize::multi_value_return`, and let `wir_build` emit the flat signature.

That is not enough, and the issue's own caveat says why.
`ReturnAbi::MultiValue` does not change a function's NIR `return_type` — the
type stays the aggregate, and only the WIR-level ABI shifts. The classifier
also runs _after_ the fixed-point loop, among the backend-required rewrites. So
a marker is by construction invisible to every NIR pass: nothing in the loop
reads `return_abi`, and nothing could usefully read it, because the NIR the
passes see is unchanged.

A marker buys real but narrow things — `wir_build` emitting the intended ABI
instead of operating on boxing it introduced itself, and WIR peephole/DCE
seeing clean code. It buys no pass interaction at all.

### The design: scalarize the return in NIR, reuse the tuple ABI

Rewrite the function, not a flag. A variant-returning function becomes a
tuple-returning function:

```
fn parse(s: String) -> Result<i32, String>
  ⇒
fn parse(s: String) -> (i32, i32, Option<String>)
//                      tag  Ok    Err
```

- `return Ok(v)` becomes `return (0, v, None)`.
- `return Err(e)` becomes `return (1, 0, Some(e))`.
- `match parse(s) { Ok(v) => A, Err(e) => B }` becomes
  `let t = parse(s); match t.0 { 0 => { let v = t.1; A } _ => { let e = t.2!; B } }`.

From there every existing NIR pass applies unchanged, because there is no
variant left in the return position — only a tuple, an integer tag, and locals.
And the Wasm-level ABI needs no new machinery: `optimize::multi_value_return`
already turns a tuple return whose call sites destructure into a flat
multi-value signature. The variant stops being a special case.

This is the return-position dual of `optimize::sroa_param`, which already
rewrites parameter signatures interprocedurally inside the loop, deliberately
placed before `nir/inline` "so the inliner sees post-SROA signatures and can
propagate the scalar through call chains" (`sroa_param.rs`). `nir/dae` and
`nir/drve` likewise change signatures mid-loop. Signature rewriting in the loop
is an established pattern here, not a new risk.

### What it unlocks

| pass                       | what it can now do                                                                                  |
| -------------------------- | --------------------------------------------------------------------------------------------------- |
| `inline`                   | inlines a callee whose returns are tuple literals; the caller's `match` on the tag meets a constant |
| `const_fold`               | folds `t.0` to a literal on paths where the tag is known, then collapses the branch                 |
| `const_branch_prune`       | deletes the arm the folded tag excluded, and everything it dominated                                |
| `sroa` / `field_scalarize` | scalarizes the returned tuple local away entirely                                                   |
| `store_load_forward`       | forwards a payload slot like any other scalar                                                       |
| `dce` / `drve`             | drops a slot no caller reads — shrinking the ABI further                                            |
| `match_to_switch`          | the tag test is a dense integer match, so it lowers to `br_table`                                   |

The `docs/optimizer.md` item directly below this one — folding a `match` whose
scrutinee is a known `VariantConstruct`, blocked because "case known, payload
opaque" is inexpressible in `const_eval::Value` — becomes reachable for the
cross-call case for free: after the rewrite the case _is_ an `i32` the constant
machinery already handles, and the payload is an ordinary opaque local.

None of these are available to a marker-only design at any effort.

## Design

### The slot typing rule

The one thing NIR cannot express directly is the padding. WIR pads an unused
result slot with `ref.null` and widens the slot with `WirType::as_nullable`;
NIR types carry no nullability modifier, and a `String`-typed slot holding null
would be a lie.

The rule that resolves it uses only types NIR already has. It keys on the
payload's _Wasm representation_, not its NIR shape, because that is what
decides whether the wrapper costs anything:

| payload type `T`                                                                                                                              | slot type   | live value | pad          |
| --------------------------------------------------------------------------------------------------------------------------------------------- | ----------- | ---------- | ------------ |
| scalar at Wasm level: primitive, `enum`, `flags`, `char`, `bool`, and `resource` / `stream` / `future` (all `WirType::I32`, `context.rs:952`) | `T`         | `v`        | zero literal |
| already null-representable: `Option<X>` for ref-represented `X`                                                                               | `T`         | `v`        | `None`       |
| any other ref: `struct`, `String`, `List<X>`, `variant`, `Option<scalar>`                                                                     | `Option<T>` | `Some(v)`  | `None`       |
| `Unit`                                                                                                                                        | (no slot)   | —          | —            |

The `Option<T>` wrapper in row 3 is free. `wir_optimize::nullable_ref` lowers a
2-case `{Unit, Payload(ref T)}` variant to a bare nullable ref, so `Option<T>`
for a non-nullable ref `T` _is_ `ref null T` at Wasm level, `Some(v)` is `v`,
and `None` is `ref.null`. The resulting signature is byte-identical to what the
WIR pass produces today — `parse_leaf` in
`tests/fixtures/opt_sroa_variant_tail_call.wado` expects
`-> [i32, i32, ref null "String"]`, which is exactly
`(i32, i32, Option<String>)` after lowering.

Row 2 is what stops the wrapper from ever costing a box. `Option<String>` is
already `ref null String`, so wrapping it again would produce
`Option<Option<String>>` — whose payload is a _nullable_ ref, which
`is_nullable_ref_eligible` rejects, so it would allocate. Using the payload
type as its own slot type instead makes the pad and a genuine `None` payload
indistinguishable, which is harmless: the tag discriminates. `Option<i32>` is
not in this row — it is an ordinary boxed variant struct, so row 3 applies and
`Option<Option<i32>>` lowers to `ref null Option<i32>`, again free.

A pad is never read — the reader tests the tag first. If a future pass gets
that wrong, an `Option` slot unwraps through `RefAsNonNull` and traps, rather
than silently producing a mistyped value. A zero-padded `resource` slot has no
such guard, which matches what the WIR pass emitted
(`default_value_for_type` → `I32Const(0)`).

`v128`, `i128` and `u128` are declined outright. The pad is what decides it:
every row above needs a zero the pass can mint as a promoted value, and
`ValueKind` has no `v128` literal — while `i128` / `u128` occupy two Wasm
results, which the slot count does not model. The WIR pass could pad all three,
because `default_value_for_type` works on `WirType` after lowering; retiring it
gives the shapes up. Nothing measured exercises them
(`tests/fixtures/opt_variant_return_v128_payload.wado` pins the decline).

### Layout: shared and per-case

The same two shapes `wir_optimize`'s `layout.rs` computes today, decided now on
NIR types:

- Shared, when every payload-bearing case has the same slot type: one payload
  slot. `Option<char>` → `(i32, char)`.
- Per-case otherwise: one slot per payload-bearing case, in case order.
  `Result<i32, String>` → `(i32, i32, Option<String>)`.

Cap: at most 8 tuple elements (`MAX_PER_CASE_RESULT_FIELDS` today), which
bounds a per-case layout at seven payload-bearing cases.

A WIR variant case has at most one payload slot — `wir_build/types.rs:380` and
`:910` map a case's single NIR payload `TypeId` to `vec![]` when it is `Unit`
and to a one-element vector otherwise, and a multi-field case is one slot
holding a tuple ref (`MTupleShape::Rectangle(f64, f64)` becomes
`payload_0: ref "tuple//[f64, f64]"`,
`tests/generated/fixtures/match_2.wir.wado:490`). So `MAX_SHARED_RESULT_FIELDS = 4`
and the `1 + max_payload_count > 4` guard in `layout.rs` can never bind, and
its `payload_{j}` loops are dead generality. The NIR layout must not carry them
forward.

### The pass: `nir/sroa_variant_return`

Modeled on `optimize::sroa_param`: gate-aware, interprocedural, placed in the
loop before `nir/inline`, and firing at most once per function (state carried
like `param_spec::ParamSpecState`) so the fixed point terminates.

#### Phase 1 — candidates

A function qualifies when its return type is a variant (or a `GenericInstance`
resolving to one), it is neither an ABI boundary nor address-taken, and its
case payloads fit the slot rule and the cap.

The exclusion set mirrors what the WIR pass derives precisely from
`collect_pinned_func_ids` (exports, element-table entries, `RefFunc` operands,
`ArrayClone` value-copy helpers):

| excluded                                      | why                                        |
| --------------------------------------------- | ------------------------------------------ |
| `is_export`, `is_cm_export`, `is_cm_binding`  | CM boundary signature is fixed             |
| `is_async`                                    | result travels via `task return`           |
| `is_dispatch_wrapper`                         | effect dispatch calls the binding directly |
| `is_closure_call()`                           | reached by `RefFunc` from a closure        |
| `FunctionKind::ValueCopy`                     | `ArrayClone` resolves it at emit time      |
| `FunctionKind::FnCanonicalDispatch`           | body supplied by `wir_build`               |
| `has_real_type_params()` / `impl_type_params` | not monomorphized                          |
| `body.is_none()`                              | extern / declaration                       |

`is_trait_method()` is not on this list, and must not be — see
[Trait methods carry the benefit](#trait-methods-carry-the-benefit).

A variant that `nullable_ref` will erase (2 cases, one `Unit`, one
ref-represented payload — i.e. `Option<T>` for ref `T`) is excluded: it already
returns a bare nullable ref, which beats any tuple.

#### Phase 2 — validation, to a fix-point

Every `Return` must produce a fresh variant value:

- `Return(VariantConstruct)` of this variant;
- `Return(Local(t))` where `t` is single-def / single-use and defined by a
  `VariantConstruct` — the shape `field_scalarize` leaves behind, and the whole
  reason `wir_optimize`'s `return_temp.rs` exists;
- `Return(Call(g))` where `g` is another candidate returning the same variant —
  the tail-call rule;
- `Return(null)`, the `Option::None` literal;
- `Block` / `If` / `Match` / `Switch` whose every leaf is one of the above;
- a diverging (`Never`-typed) value.

Every call site must destructure the result immediately:

- `Match { expr: Call(f), .. }` — the call _is_ the scrutinee. This is what `?`
  desugars to (`match f(x) { Ok(v) => v, Err(e) => { return Err(e) } }`), and
  it is the dominant shape, not an edge case.
- `let t = Call(f); …` where every use of `t` is a `Match` / `Switch`
  scrutinee, a `VariantTest`, a `VariantPayload`, or a `VariantTag`.
- `Return(Call(f))` inside another candidate.

Arm patterns must be one level deep over this variant (`Ok(x)` / `Err(e)` /
`_`). A nested pattern (`Ok(Some(x))`) rejects the candidate rather than
triggering a peel — the peel is where the WIR pass's complexity lives and it
should not be recreated. `nir/inline` and `nir/sroa` reach most of those cases
anyway once the outer level is scalarized.

Both rules couple caller and callee candidacy, so validation iterates to a
fixed point (optimistic assume-then-refute, so mutually tail-recursive groups
are accepted), exactly as `sroa_param` and `widen.rs` do.

#### Phase 3 — rewrite

Callee: `return_type` becomes the tuple type; each `Return` leaf's
`VariantConstruct` becomes a `TupleLiteral` of tag plus slots, per the layout.

Call sites: the bound temp's type becomes the tuple; `VariantTest` becomes
`FieldAccess(t, "0") == k`; `VariantPayload` becomes the slot read (unwrapped
through `VariantPayload(slot, Some)` for an `Option` slot); a `Match` over
variant patterns becomes a `Match` / `Switch` over the tag with the payload
binding prepended to each arm body. `match f(x) { … }` is first normalized into
`{ let t = f(x); match t { … } }`, so every site is the one `let`-bound shape.

A candidate reached from a global initializer is dropped: an initializer body
has no locals list to mint the binding in.

#### Every copy of a type has to move together

A rewrite that changes what a function returns has to retype four things that
each carry their own copy, and missing any one produces invalid Wasm rather
than a missed optimization:

- the function's `return_type`;
- every `Call` node targeting it — its `type_id` is what `wir_build` reads;
- the `let` binding: the `NirLocal`, the `StmtKind::Let`'s own `type_id`, and
  every `ExprKind::Local` node reading it;
- the merge points a return value passes through (`Block` / `If` / `Match` /
  `Switch`), whose `type_id` still names the variant.

The same applies to the `let t = Ok(v); … return t` shape: the initializer is
rewritten in place, so the binding's declared type has to follow it.

#### `return null` is not a node

`null` is Wado's `Option::None` literal, and lowering promotes it to a pure
value: the `Return` carries an `Operand::Value`, not an `ExprId`. A rewrite that
only ever overwrites expression nodes cannot touch it, so `rewrite_return_value`
hands back a replacement operand and each of its three write-back positions —
the `Return` statement, a block-tail `Expr` statement, a `Match` arm body —
stores it.

Which case the value denotes comes from the return type, exactly as `wir_build`
resolves a promoted `Null` (`translate.rs`), so the two agree by construction.

This is not an edge case: it is how every hand-written `Iterator::next` signals
exhaustion. Rejecting it cost both of `syntax_highlight`'s functions, the only
one `sqlite_parse` has, and two or three on each serde workload.

#### Constants must carry their type

`ValuePool::intern` is type-erased and records no type; `ValuePool::alloc_unshared`
records one. Every tag and pad this pass mints is a promoted operand the WIR
extractor will ask the type of, so all of them go through `alloc_unshared` —
`intern` compiles fine and panics in `wir_build`.

#### `Option`'s declaration has to outlive its uses

The pass mints `Option<T>` slots _after_ the early DCE run, and
`wir_build::register_mono_variants` registers an instance off the declaration.
A program that used no `Option` had the declaration dropped, so the minted slot
type had no WIR registration and codegen panicked. `remove_unreachable_types`
now keeps the `option` compiler-item declaration unconditionally; that costs
nothing, since WIR registers instances, not declarations.

Keeping a declaration means keeping its case payload types too:
`register_mono_variants` substitutes the declaration's payloads against each
instance's type args, so a surviving declaration whose payload `TypeId` was
pruned panics just the same. One set therefore gates both the payload walk and
the retain — two predicates that have to agree, and did not.

### Required changes to `optimize::multi_value_return`

The tuple classifier becomes the sole owner of the Wasm-level decision, and two
of its gates block the traffic this pass sends it.

#### Trait methods carry the benefit

Confirmed SROA candidates at `-O2` (`WADO_TRACE=sroa_variant_return`):

| program                       | functions widened | of which trait methods |
| ----------------------------- | ----------------- | ---------------------- |
| `benchmark/cbor/cbor_twitter` | 150               | 123                    |
| `benchmark/json_twitter`      | 81                | 66                     |
| `benchmark/json_canada`       | 27                | 21                     |
| `benchmark/syntax_highlight`  | 2                 | 2                      |
| `benchmark/sqlite_parse`      | 1                 | 1                      |
| `benchmark/http_routing`      | 0                 | 0                      |
| `benchmark/fts`               | 0                 | 0                      |

`Iterator::next`, `Deserializer::*`, and `FieldSchema::lookup` are 78–82 % of
every widened function on the parser workloads. `multi_value_return.rs:171`
rejects a trait method outright; keeping that gate caps the whole change at
~20 % of the available benefit. A trait method is an ordinary direct-call
target after monomorphization, and the WIR pass widening 123 of them on
`cbor_twitter` today is the existence proof that the ABI change is safe for
them. Dropping the gate also lets the tuple/struct path reach trait methods
returning tuples — a separate win, to measure on its own.

#### The arity cap

`(2..=4).contains(&result_types.len())` must widen to the layout cap (8), or
the classifier that is supposed to serve this pass rejects every per-case
variant layout. This also affects source-level tuples; measure the size effect.

#### The tail-call rule, again

The classifier required every return to build a fresh aggregate and every call
site to bind one, so `return g(x)` disqualified both functions at once: `g` had
an unbound call site, and its caller returned nothing it had built. That is the
same rule this pass applies to variants, and without it a scalarized
`return g(x)` chain lands a boxed tuple where `widen.rs` used to emit
multi-value. Accepting it couples caller and callee candidacy, so
classification became a refute-to-fix-point loop like this pass's own.

Both halves of the rule must read the _assumed_ set, not the candidate set as
it stands mid-round. Reading the latter let a caller spare its callee and then
be invalidated later in the same round: the callee took the ABI, the caller did
not, and the caller's `return g(x)` pushed N results through a one-ref
signature — 239 fixtures of invalid Wasm, at `-O0` as well as `-O2`.

## Consequences

### What retires in WIR

`widen.rs`'s entire job — rediscovering the pattern from WIR shapes, the two
fix-points, the return rewriting, the call-site rewriting — is subsumed.
`return_temp.rs` normalizes temps `field_scalarize` leaves and `wir_build`
materializes. Those temps are gone at their source rather than recognised
afterwards — see
[the return temp](#the-return-temp--resolved-in-field_scalarize) — which is
what lets this file retire.

| file                     | lines | fate                                                      |
| ------------------------ | ----- | --------------------------------------------------------- |
| `widen.rs`               | 1523  | retires                                                   |
| `return_temp.rs`         | 784   | retires                                                   |
| `wrapper.rs`             | 230   | ~215 retires; `slot_flatten` keeps `unwrap_to_inner_call` |
| `sroa_variant_return.rs` | 79    | ~40 retires (the `sroa_variant_returns` entry point)      |
| `layout.rs`              | 317   | shrinks; the layout decision moves to NIR types           |
| `access.rs`              | 467   | stays — `slot_flatten` uses its whole API                 |
| `slot_flatten.rs`        | 788   | stays — this pass declines the shape it handles           |

Retired: ≈ 2,560 lines. New: ≈ 900–1,100 for the NIR pass, by analogy with
`sroa_param.rs` (950 lines for the strictly simpler single-field parameter
case). Net ≈ −1,500 — and the line count is the least interesting number here.
This design is justified by pass interaction, not by size.

### The candidate sets converged — measured

The two analyses look at different programs: the WIR pass saw
post-`nullable_ref`, post-`propagate_trivial_copies` shapes; the NIR pass sees
NIR, and sees it _before_ inlining rather than after everything. So they did not
coincide at first. On `cbor_twitter` the NIR pass took 74 functions and left 87
to the WIR side — more than it took.

Closing those shapes one at a time took the leftovers to zero. Counted at `-O2`
with `WADO_TRACE=sroa_variant_return`, `widen` is what
`wir_optimize::sroa_variant_returns` still confirms behind the NIR pass:

| program            | widen | slot-flatten | wasm size |
| ------------------ | ----- | ------------ | --------- |
| `cbor_twitter`     | 0     | 0            | 214,015   |
| `cbor_canada`      | 0     | 0            | 90,333    |
| `cbor_catalog`     | 0     | 0            | 150,165   |
| `json_twitter`     | 0     | 0            | 170,246   |
| `json_canada`      | 0     | 0            | 75,356    |
| `json_catalog`     | 0     | 0            | 123,850   |
| `sqlite_parse`     | 0     | 0            | 584,940   |
| `syntax_highlight` | 0     | 0            | 601,425   |
| `gale_gen`         | 0     | 1            | 1,310,413 |
| `fts`              | 0     | 0            | 18,860    |
| `http_routing`     | 0     | 0            | 58,699    |

The one slot-flatten on `gale_gen` is by design — that pass stays (step 5).
`widen` is dead on every workload, which is what makes step 4 reachable.

The size cost of the NIR half was +0.80 % / +0.23 % / +0.29 % at the point the
two passes split the work, and +2.4 % / +4.4 % / +1.6 % before the tail-call
cascade rule below. The residual is the padding and `Some(_)` / `None` nodes the
NIR pass emits at return sites, which the WIR pass did not pay because it worked
in WIR directly.

No rebox fires on any workload.

Every count here is a paired run — same build, flag on and off — because a
widened-function count read once, on one program, is not evidence. An earlier
draft recorded `sqlite_parse` dropping from 67 to 1 and blamed step 2; a
merge-base build widens 1 too, and emits the same bytes.

### Throughput

`json_twitter`, four interleaved pairs on one machine (MB/s):

| run | off ser | on ser | off de | on de  |
| --- | ------- | ------ | ------ | ------ |
| 1   | 271.35  | 287.02 | 109.16 | 112.58 |
| 2   | 288.12  | 291.65 | 114.00 | 111.29 |
| 3   | 284.09  | 294.94 | 110.61 | 137.40 |
| 4   | 288.49  | 290.32 | 112.06 | 114.46 |

Serialization is faster in all four pairs, by about 2 % on the medians, and the
slowest `on` run still beats the median `off` run. Deserialization is not
distinguishable from noise: the 137.40 is an outlier, and without it the two
medians sit within 1 %.

Read that as a floor, not a verdict. A first ad-hoc pair on this same container
read +6.2 % and the next read −2.7 %, so a single run here is worthless and even
four pairs only resolve a couple of percent. The number that decides step 4 has
to come from `mise run benchmark-all` on a quiet machine.

### Termination and monotonicity

The pass changes signatures inside a loop that runs to a fixed point. It needs
no "already rewritten" state to terminate: a rewritten function returns a
tuple, and Phase 1 only admits a variant return, so it can never be a candidate
again. The candidate set strictly shrinks and the pass cannot keep the loop
alive.

### Validation is a snapshot; `inline` runs after

Validation sees the call sites that exist when it runs. `nir/inline` runs later
in the same iteration and can plant new ones — a `let mut t = <block ending in
a candidate call>` whose local still carries the variant the callee no longer
returns. The signature is already committed by then, so this is invalid Wasm,
not a missed optimization, and it is the reason the pass is staged off today
(`serde_json_synth_variant.wado`).

Placing the pass _after_ `nir/inline` does not fix it: a callee rewritten in one
iteration cannot be un-rewritten when the next iteration plants a site, whatever
the order within an iteration. Soundness has to come from the rewrite itself.

The mechanism is to rebox — wrap any call the fast path did not bind back into
the variant it used to return, `f(x)` becoming
`{ let t = f(x); match t.0 { 0 => Ok(t.1!), _ => Err(t.2!) } }`. The surrounding
context then sees exactly the type it was written against, and validation
becomes a profitability filter rather than a correctness precondition. The
rebox costs the allocation the scalarization was meant to avoid, so it is a
backstop `const_fold` and `sroa` are expected to collapse once the tag is
known — not a target.

Three rules make it work, and all three were learned by getting them wrong:

- The call-site rewrite must run over everything scalarized so far, not just
  the round's candidates. Reboxing a site inlining just planted gives up the
  optimization on exactly the sites inlining made hot; trying the fast path
  first and reboxing only the remainder does not. The rewrite then has to
  decline per binding, since a site inherited from an earlier round was never
  validated.
- The fast path, the repair, and the invariant check must agree on what counts
  as a binding fed by a call, and all three have to look through a block tail —
  by the time a later round sees a binding, `let_block_flatten` may have
  wrapped its call in a block. A narrower rule in any one of them makes the
  repair rebox the fast path's own work, which is what produced a tuple-typed
  binding holding a variant.
- Exiting a labeled block of the callee's tuple type is a handled position too,
  not just a `let` of that type. Inlining a scalarized callee into a rewritten
  call site turns its body into a labeled block typed by the tuple, and the
  callee's own tail calls then exit through `break L: f(x)`. Reboxing one hands
  a variant to a tuple-typed position — invalid Wasm from the repair itself.
  The call node's own `type_id` looks like a simpler test, but it is not one:
  the site rewrite retypes _every_ call to a rewritten callee before deciding
  which consumers it can rewrite, so that type says nothing about the consumer.

With those in place the E2E suite runs clean with the pass enabled, and emits
no invalid Wasm anywhere. The failures left are `opt_sroa_variant_*` fixtures
pinning WIR-pass local names this pass does not produce — expectation churn,
which step 3 updates.

### A payload binding that is a reference cell

An arm pattern binds one name and the rewrite re-mints it as `let v = t.k`.
When `v`'s address is taken (`Some(v) => sink.put(&v)`), `lower::plan::boxing`
promoted its slot from `T` to `Box<T>`, so a plain `let` stores a bare slot read
into a box-typed local. The re-mint boxes it instead, exactly as lowering did.
Refusing the shape instead cost 39 bindings on `gale_gen` — enough to keep four
functions boxed there — and recovering them is size-neutral.

The test has to be structural: the declared type is genuinely the `Box<T>`
struct of what the pattern names. "The declared type differs, so it was boxed"
is unsound, because local indices are pooled — a declaration matching nothing
the pattern names belongs to another binding, and boxing against it emits a ref
where an `i32` is read. A binding the test does not recognise is refused, and
validation consults the same predicate so the two decline together.

A local a `stores` parameter aliases is refused outright either way: that alias
is established outside the body, and a `let` does not re-establish it.

### A rebox is a signal, not a cost to absorb

Reboxing is sound but it reinstates the very allocation the pass removes, so a
rebox firing in steady state means the candidate should not have been taken. On
`cbor_twitter` fifty of them fired, every one a `return g(x)` in a caller that
still returned the variant: `invalidate_bad_call_sites` accepted a
tail-position call without checking that the _caller_ had a tuple to pass it
through. Refusing the callee in that case — and letting the fix-point propagate
the refusal, which is what `widen.rs`'s cascade does — took the reboxes to zero
and the size cost from +4.4 % to +0.91 %.

The rule that follows: the repair exists for sites that appear after the fact,
not for sites validation could have refused. A non-zero steady-state rebox
count is a bug in the candidate set, and worth asserting on once the fixture
churn is settled.

### `?` re-padding is free

After the rewrite, a `?` in a scalarized caller reads the callee's slots and
re-pads into the caller's own layout. Each function's returns are rewritten to
its own layout independently, so no cross-function agreement beyond "same
variant type" is needed — which is what the tail-call rule already requires.

## Staging

Each step lands and is measured before the next.

- [x] Step 1 — the pass behind a staging flag, off by default. The
      candidate-set comparison is done (see above): the two passes together
      scalarize more than the WIR pass alone on every serde workload, at
      +0.27 % to +0.92 % of wasm size and about +2 % of serialization
      throughput.
- [x] Step 2 — lift the `is_trait_method` gate and the arity cap in
      `multi_value_return`, on their own. Landed: full E2E green, wasm size
      neutral to -0.06%. `i128^Mul::mul` and its siblings now return
      `[u64, i64]` instead of allocating an `i128` struct per operation.
- [x] Step 3 — the pass is on and the flag is gone, with
      `wir_optimize::sroa_variant_returns` still running behind it. What it
      still found was the worklist, and
      [closing it](#how-the-87-function-gap-closed) took it to zero.
- [ ] Step 4 — retire `widen.rs`, `return_temp.rs`, and most of `wrapper.rs`;
      shrink `layout.rs`. Gate: widened-function counts held, size and
      throughput not regressed. If the WIR side does not shrink, keep it and
      stop here — a NIR rewrite that duplicates a WIR rediscovery is worse than
      either alone.
- [x] Step 5 — `slot_flatten` stays in WIR, and the measurement is why. It
      splits a `ref W` result slot whose `W` is itself a small variant, and it
      fires 1–2 times per benchmark — once across `gale_gen`, twice on
      `cbor_twitter`. A NIR analogue means a tree-shaped layout through
      `compute_layout`, `build_result_tuple`, the call-site reconstruction and
      the arity cap, for those one or two functions. Instead this pass
      _declines_ the shape (`slot_flatten_would_split`), which restores the
      WIR splits it had been displacing and is worth -155 / -76 / -48 bytes.

## How the 87-function gap closed

At step 3 `widen.rs` still widened 87 functions on `cbor_twitter` — more than
the 74 this pass took — and the retire table was premature. Joining the WIR
pass's applied list against this pass's decline reasons split them three ways,
and each turned out to be one shape, not dozens of problems:

| group                          | count | what it actually was                                          |
| ------------------------------ | ----- | ------------------------------------------------------------- |
| no layout                      | 30    | `Result<Option<i32>, E>` — a payload that is itself a variant |
| refuted at a call site         | 32    | a `let mut` binding the call, never assigned again            |
| never reached this pass at all | 25    | nothing — a join artifact                                     |

The 25 were not a gap. WIR function names carry their module path and NIR names
are bare, so the join dropped them; they were candidates all along.

The 32 were one rule. A `let mut t = f(x)` that nothing ever assigns is an
immutable binding, and both this pass and `multi_value_return` were refusing it
on the `mut` alone. `settled_locals` accepts it, and the two passes share the
predicate — a site only one of them accepts leaves the tuple boxed, which is
worse than the variant it replaced. The rule has to count writes _through_ a
local, not just assignments to it: `h.seen = h.inner.next()` makes `h`'s `mut`
load-bearing, and treating `h` as settled split a local the field write then had
nowhere to land in, trapping at `-O0`.

The 30 were the payload rule. `Option<i32>` as a case payload tripped
`slot_flatten_would_split`, handing them to the WIR side by design. Giving the
inner variant its own tag and slots in the outer tuple takes them here instead.
A nested layout that _reconstructs_ the inner value at the call site was
implemented and measured first, and discarded: it produced exactly the intended
signatures and cost +985 bytes to recover +113, because a site binding the whole
inner value has to rebuild what the callee just took apart. The form that pays
pushes the destructure into the call site, which is what `slot_flatten` does —
so a payload the split would still reach is left to it.

The last four functions on `gale_gen` were the reference-cell refusal above:
39 arm bindings declined for an address-taken local, none of them nested.

Nothing in that list argued the design was wrong; it argued the measurement had
to come before the deletion.

## Open questions

### The return temp — resolved in `field_scalarize`

Recorded here because the answer is not where this WEP first looked. The temp is
not this pass's to sink: `field_scalarize` hoists the field write-back _after_
the returns are already tuple literals, one `let` per branch, so the local
carries two defs and two reads and no single-def rule in this pass reaches it.
Sinking the `VariantConstruct` wrapper at this pass's own rewrite site was tried
and is inert — the shape does not exist yet at that point.

The constraint that creates the temp is "the write-back must run between the
value's evaluation and the return jump". That applies to the value's
_components_, not to the aggregate: an aggregate literal is a pure construction
over its operands, so binding the operands and rebuilding the literal after the
sync evaluates the same effects in the same order and leaves no aggregate local.
`field_scalarize::capture_for_sync` states that once for the three sites that
used to hand-roll it. Two details it has to get right:

- Only a literal constant is re-emitted after the sync without a binding. A
  promoted `ValueKind::FieldAccess` is re-emitted at its use, and the sync may
  have written the field it reads.
- Each operand holds its pooled temp until the whole literal is built, or
  `alloc_temp`'s free list hands one index to two live slots.

`Cursor::scan` goes from a boxed `ref tuple` to `-> [i32, i32, ref null Fail]`,
and `CborDeserializer::is_null` — the last boxed tuple this pass left on
`cbor_twitter` — flattens with it, for -94 bytes.

The generalisation for step 4: `return_temp.rs` covers this shape in WIR with a
scan and a relocation budget. Removing the shape at its source is cheaper than
recognising it afterwards, and it is what lets that file retire.

### A `LabeledBlock` return value arriving via `break L: v`

Rejected outright. The return-value rewrite walks the block tail; a value
reaching the return through `break L: v` is not in the tail, so the candidate is
declined rather than mis-rewritten. Sound, but it gives up functions whose
returns pass through a labeled block.

## Alternative considered: `ReturnAbi::Variant`

Add a `Variant` case to `nir::ReturnAbi` carrying nothing — a permission, since
the physical layout is a WIR representation fact `wir_build` must be free to
veto (`nullable_ref` erasure, payload-type eligibility, caps). The classifier
sets it; `wir_build` computes the layout, emits the flat signature, lowers
`VariantConstruct` at the single `StmtKind::Return` translation site
(`translate.rs:1770`), and binds call results to split locals as
`try_emit_multi_value_let` already does for tuples.

That design is sound, and cheaper: no signature rewriting inside the loop, no
new pass, and the same WIR files retire. It is the right fallback if the NIR
rewrite proves infeasible — for instance if arm-pattern rewriting turns out to
carry the complexity the WIR peel does.

It is not the design here because it cannot serve the motivation. A flag only
`wir_build` reads produces no interaction with any NIR pass, and no refinement
of it will, because the NIR the passes see is byte-for-byte what it was before.
The measurable difference between the two designs is everything in
[What it unlocks](#what-it-unlocks).

## References

- Issue #1742.
- `wado-compiler/src/optimize/sroa_param.rs` — the in-loop interprocedural
  signature rewrite this mirrors.
- `wado-compiler/src/optimize/multi_value_return.rs` — the tuple ABI classifier
  this pass feeds.
- `wado-compiler/src/wir_optimize/sroa_variant_return/` — the WIR pass this
  replaces.
- `wado-compiler/src/wir_optimize/nullable_ref.rs` — the lowering that makes
  the `Option<T>` slot free.
- [Variant Wasm GC Representation](./wep-2026-02-08-variant-representation.md)
- [NIR Rewrite Engine — Detailed Design](./wep-2026-06-05-nir-rewrite-engine-design.md)
