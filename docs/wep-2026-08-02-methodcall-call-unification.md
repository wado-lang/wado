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

- [ ] `nir_arena.rs` — drop the variant, add `has_receiver` to `Call`; fix
      `clone_expr_kind`, `replace_operand_to`, `for_each_child`; delete
      `for_each_operand` and `map_operands` (no callers)
- [ ] `lower/translate.rs` — emit `Call { has_receiver: true, args: [receiver, …args] }`
      from `TirExprKind::MethodCall`, routing the receiver through
      `convert_call_arg_at(_, callee, 0, _)`; drop `call_mut_roots`'s offset
- [ ] `nir_unparse.rs` — render a `has_receiver` call as `recv.method(rest)`
- [ ] `optimize/*` (≈90 sites)
- [ ] `niri/*`, `nir_value_graph/builder.rs`, `nir_engine.rs`
- [ ] `wir_build/translate.rs`

### Stage B — TIR (66 sites + 57 constructor sites)

- [ ] `tir.rs` — drop the variant and the `MethodCallInvariant` /
      `TirExprKind::method_call` pair; fix the stale `Call` doc comment
      (`tir.rs:4035`) that still claims `method_info: Some(_)` means static
- [ ] `tir_visitor.rs`, `unparse.rs`
- [ ] `elaborator/` — `build_tir_method_call` becomes a `Call` builder that
      prepends the receiver, keeping its 21 `reify.rs` callers' shape;
      `self_in_args` becomes the `has_receiver` value rather than a separate
      annotation read
- [ ] `synthesis/` — `resource_rewrite`'s instance/static split reads
      `has_receiver`; `effect_dispatch`'s `build_resource_fallback_call` collapses
      its two branches into one
- [ ] `monomorphize/`, `lower/plan/*`
- [ ] `lower/translate.rs` becomes a pass-through

### Stage C — cleanup

- [ ] `effect_check.rs` — `is_method = func_ref.method_info.is_some() && !self_in_args`
      becomes a direct read
- [ ] Update the WEPs describing the two-node model:
      [NIR](./wep-2026-05-11-nir.md),
      [CM Binding Synthesis](./wep-2026-02-15-cm-binding-synthesis.md),
      [Effect Handler](./wep-2026-04-11-effect-handler.md),
      [Worklist Rewrite Engine](./wep-2026-06-05-worklist-rewrite-engine.md)
- [ ] Update `docs/compiler.md` and `docs/optimizer.md`

## Consequences

The sites below change behavior rather than just shape. Each needs its own
red/green step; several are latent-bug fixes and should land with a fixture.

### Merged arms that currently skip the receiver

After the merge the receiver joins `args` and these loops start seeing it:

| Site                                     | Effect                                                           |
| ---------------------------------------- | ---------------------------------------------------------------- |
| `nir_arena.rs:712` (`map_expr_operands`) | LICM gains receiver hoisting / promotion — new optimization      |
| `optimize/alias.rs:891`                  | Ref-escape collection covers receivers — more conservative alias |
| `optimize/field_scalarize.rs:2467`       | `Slot::Arg(i)` indices shift by one for methods                  |
| `optimize/arena_query.rs:621`            | review                                                           |
| `optimize/clone_forward.rs:164`          | review                                                           |
| `lower/plan/value_copy/analyze.rs:102`   | review                                                           |
| `lower/translate/pattern.rs:2346`        | review                                                           |
| `monomorphize/func_inst.rs:4023`         | review                                                           |

### Argument / parameter index shifts

Every pass that zips `args` against the callee's parameters drops its
method-specific offset: `dae` (whose `dead[0]` is already the receiver — it
converges), `param_spec`, `sroa_param`, `multi_value_return`, `field_scalarize`,
`container_sroa`.

### The receiver gains a value-copy decision

Today the receiver bypasses `convert_call_arg_at`, so it is never wrapped in a
copy and carries no `is_mut`. Routing it through the uniform path must not
introduce copies: `&self` / `&mut self` receivers are references
(`should_wrap_value_copy` declines), and `is_mut` for slot 0 is
`self_kind == MutRef` — the value the UFCS path already computes. Verify with
`mise run benchmark-all` and `mise run report-wasm-size` before and after.

### Unit erasure

`wir_build` erases unit-typed arguments. The receiver is never unit (existing
comment at `wir_build/translate.rs:2401`), so uniform erasure is a no-op — but the
claim now has to hold for `args[0]` rather than a distinct slot.

### Dump and `Inspect` output

Preserved by `has_receiver` rendering, with one accepted change: a UFCS call
`Trait::method(recv, …)` now dumps as `recv.method(…)`, since `has_receiver`
records semantics rather than spelling.

### Lost guardrail

`MethodCallInvariant` channelled every `MethodCall` construction through one
checkpoint asserting that arguments were typechecked against the callee's declared
parameters. `Call` has never had such a witness, so unification drops it. This is a
deliberate trade; the surviving protection is that the elaborator still builds call
arguments only from a resolved signature.
