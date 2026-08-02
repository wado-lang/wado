# WEP: Unifying `MethodCall` into `Call`

## Context

Three IRs carry a call node. Two of them carry it twice.

| IR  | Free call                     | Instance method                          | Static method            |
| --- | ----------------------------- | ---------------------------------------- | ------------------------ |
| AST | `Expr::Call { callee, args }` | `Expr::MethodCall { receiver, … }`       | `Expr::StaticMethodCall` |
| TIR | `Call { func, args }`         | `MethodCall { receiver, func, args }`    | `Call { func, args }`    |
| NIR | `Call { func_id, args }`      | `MethodCall { receiver, func_id, args }` | `Call { func_id, args }` |

TIR and NIR already fold static methods into `Call` — the only thing `method_info`
adds there is name formatting. Instance methods are the outlier: the receiver sits
in its own `Box<TirExpr>` / `Operand` slot outside `args`, so every consumer that
walks a call handles two shapes, and every consumer that maps arguments to
parameters applies a `+1` offset for one of them.

The receiver is a parameter. `wir_build` says so directly:

```rust
// wado-compiler/src/wir_build/translate.rs
ExprKind::MethodCall { func_id, receiver, args, .. } => {
    // Receiver first (self/&self/&mut self is never unit, so it
    // always stays a call argument); args[i] maps to params[i+1].
    let ordered: Vec<Operand> = std::iter::once(*receiver)
        .chain(args.iter().map(|a| a.expr))
        .collect();
```

Both node kinds emit one `WirInstr::Call`. The split buys nothing at the backend.

### The merged shape is already load-bearing

Four places in the compiler already model a method call as "receiver at `args[0]`":

- UFCS (`Trait::method(recv, …)`,
  [Overload Resolution](./wep-2026-07-31-overload-resolution.md)).
  `resolve_trait_qualified_call` builds `param_is_mut`, `param_defaults`, and
  `param_types` with a leading `self` entry and records `self_in_args: true`
  (`elaborator/method_call.rs:1315`). A UFCS call on an instance method therefore
  already reaches TIR as a `Call` whose `args[0]` is the receiver — the merged
  shape exists in the IR today.
- CM binding. `rewrite_cm_instance_method` takes a `MethodCall`, prepends the
  receiver to `args`, and emits a `Call` / `CmRawCall`
  (`synthesis/cm_binding/resource_rewrite.rs:1779`).
- DAE. When a method's `self` is dead, `apply_dae` rewrites
  `MethodCall(recv, m, args)` into `Call(m, args)` at every call site
  (`optimize/dae.rs:42`), tracking receiver deadness as `dead[0]`.
- `last_use`. `returns_receiver_alias` is consulted through two arms that differ
  only in how they reach the receiver — `place_path(receiver)` versus
  `place_path(&args.first()?.expr)` (`lower/plan/value_copy/last_use.rs:476`).

Representability is not in question. What remains is the mechanical cost of the
duplication, and the defects it has already produced.

### Cost today

`MethodCall` is named at 181 sites across the compiler:

| Layer                                                           | Sites | Of which merged `Call { … } \| MethodCall { … }` arms |
| --------------------------------------------------------------- | ----- | ----------------------------------------------------- |
| NIR (`nir_arena`, `optimize/*`, `niri/*`, `wir_build`)          | 115   | 19                                                    |
| TIR (`tir`, `elaborator`, `synthesis`, `monomorphize`, `lower`) | 66    | 12                                                    |

Plus 57 call sites of the `MethodCall` constructor pair
(`TirExprKind::method_call` / `Elaborator::build_tir_method_call`), whose only
purpose is to gate a zero-sized `MethodCallInvariant` witness that `Call` does not
have.

### Defects the duplication has produced

Two arena traversals bind `args` from both variants and so never visit a
`MethodCall`'s receiver:

```rust
// wado-compiler/src/nir_arena.rs:712  (map_expr_operands)
// wado-compiler/src/nir_arena.rs:799  (for_each_operand)
ExprKind::Call { args, .. } | ExprKind::MethodCall { args, .. } => {
    for a in args { a.expr = f(a.expr); }
}
```

`map_expr_operands` is LICM's operand-snapshot/rewrite pair
(`optimize/licm.rs:2610`, `:2700`). Snapshot and rewrite share the same blind
traversal, so this is not wrong code — it is a missed optimization: an invariant
receiver is never hoisted, and promotion never lifts a receiver to
`Operand::Value`. `for_each_operand` and `map_operands` have no callers at all in
the workspace — dead code encoding the same gap.

A third instance of the class: `optimize/alias.rs:891` walks call arguments to
record reference escapes and skips the receiver, so a `&`-borrowed receiver is
invisible to the escape collector.

The merge closes all three by construction: with the receiver in `args`, a
traversal cannot omit it without omitting every argument.

## Decision

Delete `TirExprKind::MethodCall` and `nir_arena::ExprKind::MethodCall`. The
surviving node is:

```rust
Call {
    func: FunctionRef,          // NIR: func_id: FuncId
    type_args: Vec<TypeId>,
    /// When `has_receiver`, `args[0]` is the method receiver and `args[i]`
    /// maps to the callee's `params[i]` with no offset.
    args: Vec<CallArg>,         // NIR: Vec<ArenaCallArg>
    has_receiver: bool,
}
```

Arguments line up with the callee's declared parameters at every index, for every
call shape. The `+1` offsets in `lower/translate.rs`
(`call_mut_roots(func, Some(receiver), args, 1)`,
`convert_call_arg_at(a, …, i + 1, …)`) disappear, and `call_mut_roots` loses its
`receiver` and `param_offset` parameters entirely.

### Why `has_receiver` stays as data

Whether a callee takes `self` is a property of the callee, so the purist option is
to derive it — `params[0].name == "self"` — rather than store it. Three consumers
make that expensive:

- `unparse.rs` renders TIR back to Wado source and holds only a `FunctionRef`,
  which has no parameter list. This is not merely a debug surface:
  `unparse_tir_closure_source` (`lower/plan/closure.rs:756`) produces the string a
  closure's `Inspect` prints. Without the flag, `|x| x.len()` would inspect as
  `|x| String::len(&x)` — auto-`&` included, since the rule that strips it is keyed
  on the receiver slot.
- `synthesis/cm_binding/resource_rewrite.rs` picks the instance path (cast
  `args[0]` to an `i32` handle) or the static path off the node kind. That is a
  correctness path and needs a definite answer.
- `wir_build` must stay correct for unresolved / bodyless callees, where no
  parameter list is reachable.

`has_receiver` already exists conceptually: it is `self_in_args` from
`elaborator::sem::types::StaticMethodDispatch`, promoted from an elaborator-side
annotation onto the node. Its meaning is semantic, not syntactic — `args[0]` is the
receiver — so a UFCS call sets it too.

The bool is not the variant in disguise. The variant forced a second _shape_ (a
receiver outside `args`, and every index shifted); the bool leaves one shape and
answers one question, at the ~10 sites that ask it. The other ~170 stop asking.
Whether it can later be derived and dropped is a follow-up worth revisiting once
those ten are down to two.

### Non-goals

`ast::Expr::MethodCall` stays. The AST records surface syntax for the formatter,
the LSP, and `assert` diagnostics; `recv.m(a)` and `f(a)` are genuinely different
spellings there. The elaborator's `method_call.rs` / `call.rs` split is about
_resolution_ (method lookup versus free-function lookup), not node shape, and is
untouched by this WEP.

## Implementation

NIR first. It is downstream, its consumers are mostly mechanical rewriters, and
merging it leaves `lower/translate.rs` as the single adaptation point — it prepends
the receiver to `args`, exactly as `rewrite_cm_instance_method` already does. Each
stage is independently green.

Method: delete the variant, then let `rustc` enumerate the work.

### Stage A — NIR (115 sites)

- [x] `nir_arena.rs` — drop the variant, add `has_receiver` to `Call`; fix
      `clone_expr_kind`, `replace_operand_to`, `for_each_child`; delete
      `for_each_operand` and `map_operands` (no callers)
- [x] `lower/translate.rs` — emit `Call { has_receiver: true, args: [receiver, …args] }`
      from `TirExprKind::MethodCall`, routing the receiver through
      `convert_call_arg_at(_, callee, 0, _)`; drop `call_mut_roots`'s offset
- [x] `nir_unparse.rs` — render a `has_receiver` call as `recv.method(rest)`
- [x] `optimize/*` (≈90 sites)
- [x] `niri/*`, `nir_value_graph/builder.rs`, `nir_engine.rs`
- [x] `wir_build/translate.rs`

Two accessors carry the merged shape where a pass genuinely reasons about a
receiver: `ExprKind::as_method_call()` returns `(receiver, func_id, rest)` for a
`has_receiver` call, and `ExprKind::method_call(func_id, receiver,
receiver_is_mut, args)` builds one. Both are views over the single argument
list, not a second storage shape.

Collapsed along the way: `niri::CallSite`'s `receiver: Option<Operand>` field
and its `arity()` / `operands()` offset arithmetic; `inline`'s
`try_inline_method_call_expr` (merged into `try_inline_call_expr`) and the
`Call::{Free,Method,Other}` dispatch in `inline_calls_in_expr`;
`field_scalarize`'s `Slot::Receiver`; `dae`'s and `sroa_param`'s
receiver-collapse branches, both now a `has_receiver = false` assignment.

### Stage B — TIR (66 sites + 57 constructor sites)

- [x] `tir.rs` — drop the variant and the `MethodCallInvariant` witness;
      `TirExprKind::method_call` keeps its signature and now builds a `Call`, so
      all 57 constructor sites needed no edit. `as_method_call` reads the shape
      back, mirroring the NIR accessor. The stale `Call` doc comment claiming
      `method_info: Some(_)` means static is gone.
- [x] `tir_visitor.rs`, `unparse.rs` — the renderer branches on `has_receiver`
      so a closure's `Inspect` text stays source-shaped
- [x] `elaborator/` — `build_tir_method_call` prepends the receiver
- [x] `synthesis/` — `rewrite_cm_instance_method` and `rewrite_cm_static_method`
      become one `rewrite_cm_call`: they only ever differed by whether `args[0]`
      is cast to the i32 resource handle
- [x] `monomorphize/`, `lower/plan/*`
- [x] `lower/translate.rs` — one `convert_call`, no `MethodCall` arm

`Call::func` is boxed. With `MethodCall` gone, `Call` is the only `FunctionRef`
holder and unboxed it dominates `TirExprKind` by ~290 bytes, tripping
`large_enum_variant` — the same treatment
[`ConditionElement::Let`](../wado-compiler/src/ast.rs) already gives its
`Pattern`.

Two arms in closure planning collapsed outright: both already derived their
argument slice from `self_param_offset`, which is exactly the
receiver-at-`args[0]` shape, so the method arm was the same code with the offset
hardcoded.

### Stage C — cleanup

- [x] Update the WEPs describing the two-node model:
      [NIR](./wep-2026-05-11-nir.md),
      [CM Binding Synthesis](./wep-2026-02-15-cm-binding-synthesis.md),
      [Effect Handler](./wep-2026-04-11-effect-handler.md),
      [Worklist Rewrite Engine](./wep-2026-06-05-worklist-rewrite-engine.md),
      [Closure Internals](./wep-2026-01-25-closure-implementation-internals.md),
      [NIR Array Literal](./wep-2026-05-31-nir-array-literal.md).
      `docs/compiler.md` and `docs/optimizer.md` never named the node.
- [ ] The elaborator sets `has_receiver` for a trait-qualified (UFCS) call, so
      the flag means "args[0] is the receiver" rather than "came from dot
      syntax". This changes the dump spelling and what `alias` / `niri` treat as
      a receiver, so it wants its own red/green step.
- [ ] The elaborator sets the receiver's `is_mut` from `self_kind`, and
      `lower`'s `call_args_in_param_order` drops the `mut_ref_params` override.
- [ ] `effect_check.rs` — `is_method = func_ref.method_info.is_some() && !self_in_args`
      becomes a direct read once the flag above is semantic.

## Consequences

The sites below change behavior rather than just shape. Each needs its own
red/green step; several are latent-bug fixes and should land with a fixture.

### Merged arms that skipped the receiver

After the merge the receiver joins `args`, so these loops see it. Reviewed one
by one; three broadened deliberately, the rest kept their receiver-specific
handling by branching on `has_receiver`.

| Site                                                                                                                                | Outcome                                                                                    |
| ----------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| `nir_arena::map_expr_operands`                                                                                                      | LICM gains receiver hoisting / promotion — new optimization                                |
| `optimize/const_object_globalization.rs`                                                                                            | A by-value receiver is now a hoisting candidate — new optimization                         |
| `optimize/alias.rs` (`collect_mut_escaped_node`)                                                                                    | `is_mut` loop over-approximates `args[0]`; conservative direction                          |
| `optimize/alias.rs` (`collect_ref_arg_escapes`)                                                                                     | Skips `args[0]`: the receiver is judged by the callee's `self` mode                        |
| `optimize/value_copy/mutation.rs`                                                                                                   | `args[0]` still reports as `Witness::Receiver`, not `CalleeArg`                            |
| `niri/trackability.rs`                                                                                                              | A receiver is disqualified, never a passing read                                           |
| `optimize/tmpl_hoist.rs`                                                                                                            | A receiver is scanned but not `mark_chain`'d                                               |
| `optimize/clone_forward.rs`                                                                                                         | Keeps `method_mutates_receiver` on `args[0]`                                               |
| `optimize/field_scalarize.rs`                                                                                                       | `Slot::Receiver` gone; `accumulate_call_sync`'s `_operand` wrappers were exact equivalents |
| `optimize/dce.rs`                                                                                                                   | `record_method_call` vs `record_call` keyed on `as_method_call`                            |
| `optimize/sroa.rs`, `arena_query.rs`, `lower/plan/value_copy/analyze.rs`, `lower/translate/pattern.rs`, `monomorphize/func_inst.rs` | Pure traversals; merging is exact                                                          |

`const_object_globalization` is worth spelling out because it moved a golden
fixture. `value_arg_candidates` gates on
`callee_param_readonly(func_id, first_param + pos)`, and with the receiver at
`args[0]` and `first_param` now uniformly 0, that asks the right question of
parameter 0. `compute_param_readonly` rejects any `Ref` / `MutRef` parameter
outright, so only a _by-value_ `self` that is read-only and non-escaping
qualifies — the pass's existing soundness contract, one position wider. The
effect is that a boxed literal receiver stops being re-allocated per call.
`__const_obj_N` is numbered in hoist order, so `const_global_dedup.wado` needed
its read-site index re-pinned; its other patterns are index-free now.

### Argument / parameter index shifts

Every pass that zips `args` against the callee's parameters drops its
method-specific offset: `dae` (whose `dead[0]` is already the receiver — it
converges), `param_spec`, `sroa_param`, `multi_value_return`, `field_scalarize`,
`container_sroa`.

### The receiver must not gain a value-copy decision

This checkpoint fired, and it was a miscompile. Routing the receiver through
`convert_call_arg_at` along with every other argument wraps a by-value argument
in `$value_copy$T`; the receiver is a _place_, so the copy discarded the mutation
the call exists to perform.

The premise — that a receiver is always reference-typed, so `needs_value_copy`
declines it — was wrong. A template string accumulates into a synthesized local
of bare `String` type, so every `push_str` appended to a fresh copy and only the
last part survived: `a${n}b${n}c` rendered as `33`, and an assert's diagnostic
collapsed to its final interpolation.

`lower::translate::convert_receiver_arg` gives `args[0]` the pre-merge treatment
(plain `convert_operand`), which also keeps a specialized fn-param receiver from
being re-wrapped as a canonical closure — the method was resolved against the
receiver's own type, not `fn(...)`. Pinned by
`tests/fixtures/method_receiver_no_value_copy.wado`.

The receiver's `is_mut` is still the callee's `self` mode. TIR leaves it `false`
and `lower` fills it in from `mut_ref_params`, which is where its only consumer
lives; having the elaborator set it from `self_kind` is a Stage C follow-up.

### Unit erasure

`wir_build` erases unit-typed arguments. The receiver is never unit (existing
comment at `wir_build/translate.rs:2401`), so uniform erasure is a no-op — but the
claim now has to hold for `args[0]` rather than a distinct slot.

### Dump and `Inspect` output

Preserved by `has_receiver` rendering. The anticipated UFCS change did not
materialize: `TirExprKind::method_call` is the only writer of
`has_receiver: true`, and a trait-qualified call is built as an ordinary `Call`,
so `Trait::method(recv, …)` still dumps as itself. Setting `has_receiver` there
too — making the flag fully semantic rather than "came from dot syntax" — is a
Stage C follow-up, and it would change both the dump spelling and what
`alias`/`niri` treat as a receiver.

### Lost guardrail

`MethodCallInvariant` channelled every `MethodCall` construction through one
checkpoint asserting that arguments were typechecked against the callee's declared
parameters. `Call` has never had such a witness, so unification drops it. This is a
deliberate trade; the surviving protection is that the elaborator still builds call
arguments only from a resolved signature, and `TirExprKind::method_call` remains
the single constructor every method call flows through.
