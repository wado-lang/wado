# Optimizer TODO — Smell Backlog

Findings from a full review of `src/optimize.rs`, `src/optimize/`, `src/wir_optimize.rs`, and `src/wir_optimize/` (2026-07-16, tree at 550f7454b — line numbers refer to that commit).

Severity legend:

- P0 — reproduced end-to-end: correct at `-O0`, wrong at `-O2` (or a compiler crash). Per CLAUDE.md these are P0 compiler bugs: minimal e2e fixture, issue, fix — before anything else. Repro sources are embedded below.
- P1 — soundness hole traced through the code but not yet reproduced end-to-end. Attempt a repro first; if it reproduces, it is P0.
- P2 — precision: the analysis is more conservative than the rewrite (or than a sibling pass) and misses optimizations, or a latent bug currently masked by another bug.
- P3 — structure: duplication, dead code, fragile matching, compiler-side performance.

## P0 — reproduced miscompiles and crashes

### match_to_switch: arms after a wildcard override it

- [ ] `optimize/match_to_switch.rs:282-293,358-372` — `analyze` records `default_arm` at a `PatKind::Wildcard` but keeps collecting literal/range specs from later arms; `build_switch` routes those values to their own arms, violating first-match-wins. Runs at every level including `-O0`, so the cross-level E2E strategy can never catch it. Fix: stop consuming arms once a wildcard is seen (they are dead), or bail when a spec's arm index exceeds `default_arm`.

Repro (prints `100` with `WADO_SKIP_PASS`, `5` without):

```wado
use { println, Stdout } from "core:cli";

#[inline(never)]
fn opaque(x: i32) -> i32 {
    return x;
}

export fn run() with Stdout {
    let x = opaque(5);
    let r = match x {
        _ => 100,
        0 => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 5,
        6 => 6,
        7 => 7,
        8 => 8,
        9 => 9,
    };
    println(`{r}`);
}
```

### ref_elim: capture interval ignores loop back-edges

- [ ] `optimize/ref_elim.rs:296-324,398-406` — `in_capture_interval` compares static pre-order statement positions, so a replacement of the referenced place that is statically after the ref's `last_use` but inside a shared loop executes before the next iteration's use; the fold re-reads the replaced place. Fix: extend both `last_use` and replacement positions to the end of their innermost enclosing loop.

Repro (prints `2` at `-O0`, `1000` at `-O2`; skipping `nir/peephole` restores `2`):

```wado
use { println, Stdout } from "core:cli";

struct Inner {
    v: i32,
}

struct Outer {
    xs: Inner,
}

#[inline(never)]
fn opaque(x: i32) -> i32 {
    return x;
}

export fn run() with Stdout {
    let n = opaque(2);
    let mut b = Outer { xs: Inner { v: 1 } };
    let r = &b.xs;
    let mut i = 0;
    let mut total = 0;
    while i < n {
        total += r.v;
        b.xs = Inner { v: 999 };
        i += 1;
    }
    println(`{total}`);
}
```

### ref_elim: write through a pre-existing `&mut` alias escapes the capture guard

- [ ] `optimize/ref_elim.rs:358-369,387-393` — `capture_walk` records the `&mut` borrow creation as the replacement event; a borrow that predates the ref binding falls outside `(binding, last_use]`, and the actual write during the interval roots at the alias local, never matched against the ref's places. Fix: for `via_borrow` replacements use the borrow's live range, not its creation point (or consult `stores_aliased_locals`/`address_taken_locals`, which ref_elim currently ignores — unlike `elide_box_local`).

Repro (prints `1` at `-O0`, `999` at `-O2`):

```wado
use { println, Stdout } from "core:cli";

struct Inner {
    v: i32,
}

struct Outer {
    xs: Inner,
}

export fn run() with Stdout {
    let mut b = Outer { xs: Inner { v: 1 } };
    let m = &mut b;
    let r = &b.xs;
    m.xs = Inner { v: 999 };
    println(`{r.v}`);
}
```

### Trap-erasing purity: `is_pure_expr` deletes defined traps (dae, drve, elide_local)

- [ ] `optimize/arena_query.rs:260-303` — `is_pure_expr` classifies `Div`/`Mod`, `Index`, `Deref`, `FieldAccess`, `VariantPayload`/`VariantTest`, and `Cast` as pure, so `dae` drops a trapping dead argument, `drve` voids a trapping return value, and `elide_local` drops a trapping unread `let`. The repo already models these as observable elsewhere (`mod_ref.rs:330-332` sets `may_trap` for Div/Mod; `licm.rs:1414-1450` excludes them; the `opt_const_fold_div_zero.wado` fixture pins trap preservation), so at least three purity oracles disagree. Fix: add a may-trap dimension to `arena_query` (`is_pure_expr` vs `is_pure_nontrapping_expr`), use the non-trapping form in the deleting passes, and consolidate `elide_box_local::LeftmostWalk` and `select_lowering::is_select_eligible` onto the same classification.

Repro (traps `integer divide by zero` at `-O0`, prints `helper ran / done` at `-O2`; `WADO_SKIP_PASS=nir/dae` restores the trap):

```wado
use { println, Stdout } from "core:cli";

#[inline(never)]
fn opaque(x: i32) -> i32 {
    return x;
}

#[inline(never)]
fn helper(unused: i32) with Stdout {
    println("helper ran");
}

export fn run() with Stdout {
    let zero = opaque(0);
    helper(100 / zero);
    println("done");
}
```

### labeled_block_fusion: temp binding deleted while still read later

- [ ] `optimize/labeled_block_fusion.rs:236-247,359-363` — `check_fusion_preconditions_*` counts uses of `temp_local` only inside the consumer's arms; uses after the consumer pair are never checked, yet `perform_fusion` deletes the `let temp = LB` statement. Fix: validate against the whole-function use set (`Engine::local_reads` already exists) and bail unless every read is accounted for by the consumer statement.

Repro (runs at `-O0`, traps `null reference` at `-O2`):

```wado
use { println, Stdout } from "core:cli";

fn f(i: i32) -> Option<i32> {
    if i == 1 {
        return Option::Some(10);
    }
    return null;
}

export fn run() with Stdout {
    let mut total = 0;
    for let i of 1..<3 {
        let x = f(i);
        if let Some(v) = x {
            total += v;
        }
        if let Some(w) = x {
            total += w;
        }
    }
    println("total={total}");
    assert total == 20;
}
```

### labeled_block_fusion: break checker skips `Expr`/`LetDestructure` statements

- [ ] `optimize/labeled_block_fusion.rs:585-651` — `check_lb_breaks_in_stmt`'s `_ => true` arm does not descend `StmtKind::Expr`/`LetDestructure`, so a `break L: <dynamic value>` hidden under a match/switch statement escapes validation, while `transform_lb_stmt` (966-1021) still rewrites it and routes any non-`VariantConstruct` break value to the None/else case. The sibling walkers handle these statement kinds — copy-paste divergence. Fix: cover the missing statement kinds (better: one shared exit visitor, see P3).

Repro (asserts `got == 13` at `-O0`, yields `got: 2000` at `-O2`):

```wado
use { println, Stdout } from "core:cli";

variant Sig {
    A(i32),
    B,
}

#[inline(never)]
fn h(n: i32) -> Option<i32> {
    if n > 5 {
        return Option::Some(n);
    }
    return null;
}

fn g(s: Sig) -> Option<i32> {
    match s {
        Sig::A(n) => {
            let r = h(n);
            return r;
        },
        Sig::B => {},
    };
    return Option::Some(10);
}

export fn run() with Stdout {
    let mut got = 0;
    for let i of 6..<8 {
        let x = g(Sig::A(i));
        if let Some(v) = x {
            got += v;
        } else {
            got += 1000;
        }
    }
    println("got=");
    assert got == 13;
}
```

### labeled_block_fusion: consumer block spliced inside a loop, retargeting an unlabeled break

- [ ] `optimize/labeled_block_fusion.rs:1531-1565` — the pass-local `block_contains_loop` descends only `Block`/`LabeledBlock` expressions and drops `Break { value }`, missing loops under if-expressions/match arms; the loop-gated free-`break`/`continue` bail (250-259, 368-382, 1754-1766) is then skipped and the cloned consumer block containing an unlabeled `break` is spliced inside the callee's loop. `inline.rs:1298-1309` already has a complete `for_each_child`-based version of the same query. Fix: delete the local walker and share the exhaustive one (move it to `arena_query.rs`).

Repro (asserts `hits == 100` at `-O0`, yields `hits: 107` at `-O2`):

```wado
use { println, Stdout } from "core:cli";

#[inline]
fn f(c: bool) -> Option<i32> {
    let y = if c {
        for let j of 0..<5 {
            if j == 2 {
                return Option::Some(100);
            }
        }
        0
    } else {
        1
    };
    if y > 100 {
        return null;
    }
    return Option::Some(y + 7);
}

export fn run() with Stdout {
    let mut hits = 0;
    for let i of 0..<3 {
        let x = f(i < 5);
        if let Some(v) = x {
            hits += v;
            break;
        }
        hits += 1000;
    }
    println("x");
    assert hits == 100;
}
```

### inline: `#[inline(always)]` on a recursive function crashes the compiler

- [ ] `optimize/inline.rs:365-378` — `is_inline_eligible` returns `true` for `InlineHint::Always` before the `recursive_functions` check, so each fixed-point iteration re-inlines every surviving recursive call (~doubling per iteration): 4094 splices at `-O2`, and at `-O3` the compiler aborts with `fatal runtime error: stack overflow`. Fix: keep the recursion check ahead of the `Always` short-circuit, or reject `#[inline(always)]` on recursive functions at elaboration.

Repro (compiler stack overflow at `-O3`):

```wado
use { println, Stdout } from "core:cli";

#[inline(always)]
fn fact(n: i32) -> i32 {
    if n <= 1 {
        return 1;
    }
    return n * fact(n - 1);
}

export fn run() with Stdout {
    let mut acc = 0;
    for let i of 1..<6 {
        acc += fact(i);
    }
    println("x");
    assert acc == 153;
}
```

## P1 — soundness holes traced by review (repro first, then fix)

### Shared NIR facilities

- [ ] `optimize/mod_ref.rs:203-221,297-326,652-669` — a call contributes only `calls = true`: no read/write channel bits, no `may_trap`. `may_clobber` special-cases calls only on the writer side, so when the call sits in the moved expression, `can_move_past` approves moving it past global/heap writes and past other calls. Live through `elide_box_local.rs:378` (inlined `&expr` params yield exactly the targeted `let p = Box { value: <call> }` shape). Fix: treat `other.calls` as reading (and `self.calls` as writing) globals/heap/memory, and set `may_trap` for call nodes.
- [ ] `optimize/copy_prop.rs:296-313,337-386,636-640` (+ `optimize/value_copy/mutation.rs:92-97`) — scope-stability records a `&mut y` borrow as a point event at the borrow's statement index; later writes through the alias root at the alias local (`place_root_local` does not see through refs), so `let r = &mut y; let x = y; r.f = 5; use(x)` passes `source_scope_stable` and the fast path bypasses the `address_taken`/`has_field_mutation` gates. This is exactly the shape `inline` produces for `&mut self` receivers. Fix: a `&mut` borrow of the source permanently ends scope-stability from the borrow index onward (or attribute through-ref writes to the pointee).
- [ ] `optimize/value_copy/mutation.rs:92-97` vs `optimize/mod_ref.rs:472-480` vs `wir_build/translate.rs:2352-2355` — three facilities disagree on assign-target taxonomy: mutation.rs's doc claims `*r = v` is covered but the match arms are only `FieldAccess | Index | VariantPayload` (a `Deref` target yields no witness); mod_ref sends `VariantPayload` targets through the fallback recording them as heap reads; the WIR builder silently `Drop`s both shapes. Fix: align all three, and make the WIR builder panic instead of silently dropping.
- [ ] `optimize/dce.rs:2061-2101` — `expr_has_side_effects` treats non-`never` calls as effect-free (only `type_id == NEVER` subexpressions are preserved), contradicting its caller's contract in `remove_dead_global_sets_block`: a dead global initialized by `G = build()` where `build` writes a live global/prints/asserts is deleted wholesale. Fix: treat calls and trapping ops as effects, or reuse `arena_query::is_pure_expr`/`ModRef` instead of a fourth ad-hoc purity predicate.

### NIR passes

- [ ] `optimize/clone_forward.rs:165-207` — `stmt_disturbs_place` misses three mutation channels in the binding→use interval: mutating method receivers (`xs.push(0)` has a bare-`Local` receiver, no `MutRef` node), writes through a reference created before the interval, and callee-internal global writes. Fix: reject intervals containing mutating-receiver calls (reuse `alias::method_mutates_receiver`), any call when the root is global-rooted or aliased, and widen root matching through reference locals.
- [ ] `optimize/value_copy_demote.rs:866-871` — `ElementClean::visit_call_arg` treats a bare-`Local` by-value argument as receiving an independent deep copy, but last-use lowering makes it a move; a moved callee mutating elements corrupts the demoted share source. Fix: require callee element-immutability (or a proof the site is a copy/share, not a move) for by-value bare-local args.
- [ ] `optimize/value_copy_demote.rs:480-497` — `demote_candidate` treats `storage_root == None` as "fresh rvalue, uniquely owned", but `arena_query.rs:118-121` explicitly warns `None` does not imply freshness (`container.index_value(i)` returns `None` yet aliases the container). Fix: gate the `None` case on an actual freshness proof.
- [ ] `optimize/container_sroa.rs:1137-1231` vs `1336-1361,1403-1506,1686-1805` — the escape analysis whitelists `ElementWriter`/`IndexWriter` calls at any expression position, but the rewriter expands them only at statement level; a `v.push(...)` in a match-arm tail survives referencing the deleted binding. Fix: make the whitelist position-sensitive (or expand writers in `rewrite_expr`), and panic instead of silently skipping when a call on a decomposed local fails to expand.
- [ ] `optimize/sroa_param.rs:247-266,296-351` — converting a `&S` read to a call-time value snapshot is guarded only by an identically-typed sibling `&mut` param; a write reaching the same wrapper object through a differently-typed path (`f(&s.m, &mut s)`) or a mutable global is invisible. Fix: require every call-site arg to be provably fresh/unaliased, or reject any `&mut` sibling whose pointee transitively contains the wrapper type and any callee global write before the last param read.
- [ ] `optimize/field_scalarize.rs:2010-2024,2249-2259,2331-2362` — mid-expression sync statements are hoisted to before the whole statement while the state machine models them at the intra-expression transition point: `let x = self.advance() + self.pos;` re-reads `pos` before `advance()` mutates it. The `Return` arm (1902-1960) and match-guard wrapping (2790-2846) are point fixes for the same hazard; `Let`/`Expr`/nested-operand positions are unfixed. Fix: emit expression-interior sync via the same wrap-in-block mechanism (or A-normalize call-bearing statements first).
- [ ] `optimize/field_scalarize.rs:2698-2722,1978-1994,2830-2838` — the `{ScalarOnly, FieldOnly}` join resolves to `ScalarOnly` without inserting convergence sync at labeled-break joins (`walk_labeled_block`) and match-guard accumulation, though the nested-loop comment states the heuristic is sound only with per-arm convergence sync. Fix: record break/guard sites and insert per-path convergence before them when the join diverges.
- [ ] `optimize/field_scalarize.rs:2548-2552` — `And`/`Or` RHS is walked as unconditionally executed; a scalar write in the RHS entered at `FieldOnly` exits `ScalarOnly`, stale on the short-circuit path. Fix: walk the RHS on a cloned state and join.
- [ ] `optimize/field_scalarize.rs:2249-2260,3118-3146,1502-1521` — `&`/`&mut expr.field` of a scalarized field is silently redirected to the scalar local (`f(&mut self.pos)` → `f(&mut __hfs_pos)`) while `extract_gc_local_index` gives the call no field effects, so callee updates can be lost entirely. Fix: disqualify candidates whose field is referenced in the loop, or model the ref-rewrite as a scalar-write transition.
- [ ] `optimize/field_scalarize.rs:220-282,311-335` — callee field-usage analysis is a whitelist defaulting to "param untouched": `let r = param; r.y += 1` is invisible, so callers skip write-back/re-read sync around the call. The caller-side scan treats the identical shape conservatively (824-840). Fix: any unrecognized value-position use of a tracked param marks it conservative.
- [ ] `optimize/store_load_forward.rs:145-205` — a stored constant is forwarded into a `&`/`&mut` referent position, destroying the place: `obj.f = 5; g(&mut obj.f)` → `g(&mut 5)`, losing the callee's write-back. `extract.rs::is_place_read` guards exactly this in the sibling pass (and `extract.rs::is_ref_place` is dead code). Fix: skip reads in ref/deref-place positions (share `is_place_read`).
- [ ] `optimize/scalar_forward.rs:179-218` — `writes_overlap` is purely syntactic (Assign targets + `MutRef` nodes, root-index equality), missing mutating calls through bare ref-typed locals and writes through previously-captured aliases; the pass consults neither `mod_ref::may_clobber` nor the `address_taken`/`stores_aliased` sets its sibling `store_load_forward` uses. Fix: when the forwarded value reads a field place, reject use statements containing calls (or gate via `ModRef::of_stmt`), and exclude address-taken/aliased roots.
- [ ] `optimize/tmpl_hoist.rs:394-411,504-539` — the escape analysis marks a local escaping only at direct positions (call arg, struct field, break value); values flowing out through `if`/`match`/block result tails are not marked, so a hoisted, buffer-aliasing template string can be pushed via `out.push(if c { s } else { other })` and every element aliases one reused buffer. Fix: treat block/if/match result values like break values in the escape walk; also check `escaping_locals` against the inner buffer local (590-592), currently never consulted.
- [ ] `optimize/licm.rs:2075-2088` — CSE run-splitting is blind to assignments inside expression-level blocks: `expr_assigns_local` recurses only into `NodeRef::Expr` children while if-expression/match-arm bodies are `NodeRef::Block`, so `let a = x + y; let b = if c { x = x - 1; x + y } else { 0 };` shares one `__cse` temp. `LetDestructure` is also hard-coded `false`. Fix: descend `NodeRef::Block` children (still skipping nested loops) and treat destructures as assignments.
- [ ] `optimize/const_object_globalization.rs:346-349` — `is_globalizable_const_operand` accepts any `Operand::Value` as a closed constant, but `promote_pure_values_early` promotes runtime-dependent pure arith over locals (`depth * 2`) into operands; a hoisted global shared across recursive activations is re-initialized per activation and outer frames observe inner values. Fix: require `is_const_value(&body.values, v)` for the `Operand::Value` case (and use the operand helper in the `StructLiteral` arm instead of `filter_map(as_expr)`).
- [ ] `optimize/condition_implication.rs:794-803,704-756,869-900` — the refutation `var + j <= var + goff < bound` assumes non-wrapping addition, but Wado add wraps: with `i = i32::MAX`, a dominating `if i + 1 < n` guard passes via wrap and the implied `assert i < n` is eliminated though it must fire. The same reasoning underlies `structural_loop_guard`, `recognize_early_exit`, `ShortCircuitEliminator`, and `eliminate_le_checks_in_node`. Fix: restrict offset-carrying refutations to shapes where wrap is impossible.
- [ ] `optimize/condition_implication.rs:1170-1222,1248-1274` — `index_upper_bound` accepts clamp arms (`if var > K { …; K } else { …; var }`) with arbitrary statements, checking only the tails; `eliminate_condition` then replaces the whole condition subtree with `false`, discarding effectful statements in the clamp blocks. Fix: require both clamp branches pure before eliminating.

### WIR passes

- [ ] `wir_optimize/const_forward.rs:347-349` — `Block`/`Seq` bodies are treated as unconditionally executed with the shared outer `FieldKnowledge`: a `LocalSet(x, StructNew{...})` after a conditional `BrIf` exit records knowledge that survives the block, folding stale field constants on the exited path. The `If` case already confines positive facts to cloned branch states. Fix: after any instruction that can exit the block, stop recording positive facts (or process the remainder in a cloned state merged by invalidation).
- [ ] `wir_optimize/const_forward.rs:262-311` — the nested-reassignment gate (`single_assigned_locals`) protects only `local_const`; the `fields` map has no such gate and `update_knowledge_from_instr` matches only top-level statement shapes, so a nested `LocalTee/LocalSet` reassignment leaves stale `(x, field)` entries. Fix: walk each statement's full subtree for defs, or gate `fields` on `single_assigned_locals` too.
- [ ] `wir_optimize/const_forward.rs:606-629` vs `320-336` — `invalidate_locals_modified_in_body` (branch/loop merge) is a weaker copy of the straight-line invalidator: it misses calls entirely (a `&mut` callee mutation inside an `if` arm invalidates nothing) and both miss `MultiValueLocalBind` defs. Currently masked by the func-id lookup bug (see P2). Fix: share one invalidation function and count `MultiValueLocalBind` as a def everywhere.
- [ ] `wir_optimize/elide_struct.rs:147-184,206-254,279-357` — `is_pure_for_elision` classifies `LocalGet`/`GlobalGet`/memory loads as relocatable and there is no dominance or intervening-write analysis: `x = S{v}; v = v + 1; … x.f` reads the incremented `v`; a use on a path before the def becomes a value instead of a trap. `peephole.rs:177-261` already implements the needed dominance/invariance walk for the same problem. Fix: reject `GlobalGet`/memory loads, require operand invariance plus def-dominates-use (or restrict to the adjacency discipline of `elide_adjacent_box_locals`).
- [ ] `wir_optimize/elide_struct.rs:147-184,293-297,330-343` — `is_pure_for_elision` ignores `may_trap` (`I32DivS`, non-saturating truncs, `RefCast`, `RefAsNonNull`): the multi-field pass deletes unread trapping initializers entirely, and single-use substitution can relocate a trap into a never-executed `If` arm — contradicting `wir_optimize/elide_local.rs:71-83`, which keeps `may_trap` subtrees alive. Fix: require `!may_trap(inner)` for dropped or relocated initializers.
- [ ] `wir_optimize/sroa_variant_return.rs:241-303,420-454,550-579` — `elide_return_only_temps` (which runs on every non-pinned function) relocates trapping expressions past control flow: `reads_only_local_state`'s `_` fallback admits `I32DivS`/`RefCast`/truncs and `find_paired_return` allows intervening `If { Return }`/`BrIf`/`Return`, so `t = a / b; if c { return OTHER; } return t;` loses the trap on the `c` path. `util::may_trap` exists and is unused here. Fix: require `!may_trap(value)` or reject intervening control-flow exits/observable stores.
- [ ] `wir_optimize/sroa_variant_return.rs:150,155-186,2401-2422` — v128 payloads pass the blacklist eligibility (`!matches!(AbstractRef | Unit)`) but `default_value_for_type`'s `_ => I32Const(0)` pads a v128 slot with an i32 (invalid Wasm), and `wir_types_equal`'s `_ => false` mis-classifies `(V128, V128)`. The silent-zero fallback also violates the "panic instead of dummy fallbacks" rule. Fix: whitelist eligibility, add V128 arms, panic on unknown types.
- [ ] `wir_optimize/sroa_variant_return.rs:1552-1614,2708-2749,2751-2857` — the call-site validator accepts strictly more than the rewriter rewrites: `check_uses_are_variant_access` blesses `LocalGet(temp)` under any `RefTest`/`RefCast` without checking the cast's type against the candidate's variant family or that it feeds a `StructGet`; `collect_refcast_aliases` Nops an alias definition with no multi-def or use validation. A missed access then reads a deleted local. Fix: validate type-ids against the case set, require `RefCast` results consumed by `StructGet`, and verify alias single-def + variant-access-only uses.
- [ ] `wir_optimize/sroa_variant_return.rs:1661-1700,2906-2994` — the block-unwrap `Br` guard checks depth 0 per level only; a `BrIf { depth: 1 }` in a nested block's prefix targeting the outer block escapes both checks, and the rewriter re-implements the unwrap with no `Br` guard at all, hoisting the prefix out of both removed blocks. Fix: check the prefix for `Br` at every stripped depth and share one unwrap implementation between validator and rewriter.
- [ ] `wir_optimize/sroa_variant_return.rs:1932-1954` vs `1665-1669` — `unwrap_to_inner_call` doesn't trim the trailing `Unreachable` that `extract_block_result_call` trims, so nested candidate calls in break-with-value call-site args are never invalidated; after signature widening the surviving call is an arity mismatch. Fix: apply the same trim (ideally merge the four unwrapper copies).
- [ ] `wir_optimize/sroa_variant_return.rs:2151-2158` — `clear_result_types_on_divergent` clears a `Block`'s result whenever any body statement diverges, but `Br(0)` exits can carry the block's value (`wir.rs::always_diverges` deliberately never treats `Block` as divergent); the canonical break-with-value shape gets its result stranded. Hard to reach today. Fix: clear only when no reachable `Br` targets the block with a value.
- [ ] `wir_optimize/sroa_variant_return.rs:3229-3245,3702-3730` — `rewrite_unwrap_to_guard` drops a `LocalSet(t, <slot copy>)` without checking `t` for other uses; holds today only via the unverified fresh-temps invariant. Fix: reject when `t` has reads outside the matched pair.
- [ ] `wir_optimize/peephole.rs:255-260` — the dominance walk's generic arm uses `for_each_child` order for `Select`, whose Wasm evaluation order (operands before condition) differs; a candidate copy defined in a `Select` condition and read in an arm walks def-before-use but executes use-before-def. The file documents this exact hazard for tee fusion (1226-1244) but not here. Latent today. Fix: handle `Select` explicitly (arms before condition, or disqualify candidates defined inside one).
- [ ] `wir_optimize/const_global.rs:110-121,126-134` — multi-assignment consensus is last-write-wins with no equality check (divergent copies silently keep the last), and `nop_promoted_assignments` drops the whole `Seq` including its `LocalSet` bindings without checking those locals are dead. Fix: require structural equality for a second const assignment (else disqualify), and verify dropped Seq locals are unread.
- [ ] `wir_optimize/dedupe_const_globals.rs:69,86-88` — structural identity via `format!("{:?}", …)` of type and init: distinct NaN bit patterns print identically and merge (observable via `to_bits`), and any Debug change silently regroups. Fix: purpose-built structural hash/equality over the const-expressible `WirInstr` subset.
- [ ] `optimize/array_literal.rs:199-206,224-258,305-312` — with a single target, `temp_binding` accepts impure `let` values, and the abort guards fire only when the temp is read; a consumed-but-never-read impure binding would have its side effect dropped by the window `drain`. Not reachable with today's lowering, but unenforced. Fix: bail (or keep the statement) when a consumed impure binding has zero substituted uses.
- [ ] `optimize/tmpl_hoist.rs:1602-1747,1413-1421` — the rename/transform walkers cover far fewer node kinds than the escape walker in the same file (`Match`, `Switch`, literals, `VariantConstruct/Tag/Test/Payload`, `GlobalVarSet`, `CmRawCall` are `Walk::None`; `LetDestructure` skipped); any missed `__r` mention after the init-`let` is replaced reads an uninitialized local. Fix: rewrite the rename on `for_each_child` and assert post-transform that no mention of the old local survives.

## P2 — precision and latent issues

### NIR

- [ ] `optimize/select_lowering.rs:118-130` — `arm_select_value` requires `StmtKind::Expr(Operand::Expr(_))`, but born-as-operands puts scalar literals in `Operand::Value`, so `if c { 1 } else { 2 }` / `if c { x } else { 0 }` never lower to `select` (verified via `wado dump -O2`). Fix: accept `Operand::Value` scalar-constant tails.
- [ ] `optimize/dae.rs:191-199` (+ `optimize/drve.rs:118-124`) — all trait methods are pinned although monomorphized call sites carry resolved `func_id`s; `sroa_param` already relaxed this (docs/optimizer.md notes the contrast) and closure functors are separately special-cased. Fix: adopt `sroa_param`'s concrete-impl eligibility rule in both passes.
- [ ] `optimize/labeled_block_fusion.rs:654-663` — `check_lb_breaks_in_operand` uses `is_some_and`, so any promoted `Operand::Value` (which cannot contain a break) vetoes fusion; the threading twin `validate_exits_in_operand` (1927-1937) correctly uses `is_none_or`. Fix: flip the polarity.
- [ ] `optimize/peephole.rs:92` + `optimize.rs:632,671` + `optimize/gate.rs` — pre- and post-inline peephole invocations share one `GatedPass::Peephole` watermark though they run different rule sets: a function quiescent pre-inline and not re-dirtied never gets `RefElimRule`/`ElideBoxLocalRule`/`LabeledBlockFusionRule`/`array_literal` applied. Fix: separate watermark columns (`PeepholePre`/`PeepholePost`).
- [ ] `optimize/copy_prop.rs:306-313` vs `409-427` — the `Local`-source stability check lacks the backward-read guard the `RefProjection` path has (`use_at <= k` rejection). Fix: apply the same read-position guard.
- [ ] `optimize/dce.rs:191,223-358` — `extend_reachable_for_optimizer_passes` also runs during the final DCE where no rewrite can fire, keeping `String::push` and transitive callees alive as pure output bloat. Fix: gate the virtual-edge extension on the pre-loop invocation.
- [ ] `optimize/licm.rs:886-898,1367-1383` — an in-loop `let r = &y` both marks `r` fully-modified and records the alias, so `is_field_hoistable(y, f)` always fails; Case 2 (ref-look-through hoisting, ~120 lines) is dead code — and would be unsound if reachable (no `r`-reassignment check). Fix: distinguish alias re-binding from pointee writes, fix the missing reassignment check, and add an in-loop-reborrow fixture.
- [ ] `optimize/licm.rs:263-340` + `optimize/tmpl_hoist.rs:149-197` — both block walkers classify only `Loop`/`If`/`LabeledBlock`(/`Let`), so loops nested under `Match`/`Switch` arms (post-`match_to_switch`!) or expression blocks get no LICM, no cond-impl, no template hoist; `loop_version_bce::collect_loops` finds all of them via `for_each_child`. Fix: drive both recursions off `for_each_child`.
- [ ] `optimize/condition_implication.rs:921-945` vs `979-1034` — loop bodies never get the `process_block` treatment: early-exit guard facts are not collected inside loops and `apply_dominating_if` never fires on nested `if`s there, while identical shapes outside loops are handled. Fix: route loop bodies through `process_block` with back-edge invalidation.
- [ ] `optimize/condition_implication.rs:1036-1043` — `block_always_exits` omits `Continue`, so the common in-loop skip guard `if i >= n { continue }` contributes no fact. Fix: add `StmtKind::Continue` (and consider `If` with both arms exiting).
- [ ] `optimize/const_branch_prune.rs:121-132` vs `177,216` — the `{ expr } → expr` rule matches only `Operand::Expr`; a single-statement block wrapping a promoted `Operand::Value` stays, while the two labeled-block rules in the same function handle the operand case via `redirect_expr`. Fix: add the `Operand::Value` arm.
- [ ] `optimize/const_folding.rs:286-327,721-749` — `update_env_from_stmt` defensively invalidates on `Let` rebinding but returns early for `LetDestructure` (index reuse is real: `const_object_globalization.rs:219-224`); `const_seq_len_a`'s reverse scan doesn't stop at the nearest binding of the index. Fix: invalidate destructure-bound locals; stop the scan at the first matching `let`.
- [ ] `optimize/field_scalarize.rs:2000-2009` — the `Continue` arm forces every candidate to `Both` instead of the loop's per-candidate entry state, re-emitting the per-iteration write-back the deferral machinery exists to eliminate in every `continue`-heavy loop. Fix: sync to `entry_states_for` like the body-end does.
- [ ] `optimize/scalar_forward.rs:81,119-138` — uses nested in a sub-block of the adjacent statement are rejected even for pure non-trapping scalars, and `Div`/`Mod` are banned even with a constant non-zero divisor. Fix: allow sinking into sub-blocks for trap-free values; admit const-non-zero divisors.
- [ ] `optimize/match_to_switch.rs:309-333,347-372` — no cost model on arm cloning: range expansion clones the arm body per value (up to `SWITCH_MAX_RANGE = 1024`) and default-hole filling clones `arms[0]` per hole; `covered` double-counts overlapping specs, skewing the density gate. Fix: add a clones × arm-size budget (or share repeated arms via one labeled block) and de-duplicate covered values.
- [ ] `optimize/inline.rs:98-114` — `count_stmt` costs a `Return` value but costs a `break L: <expr>` value 0, undercounting candidates. Fix: cost break values.

### WIR

- [ ] `wir_optimize/const_forward.rs:87` — callee lookup indexes `module.functions` with the absolute `WirFuncId` (which carries `defined_func_base`), so the lookup always misses for defined functions and the entire stores-aware alias refinement (78-130) never fires; an import call can fetch an unrelated function's `stores` (the unsound direction, unreachable today). This masking also hides the invalidation gaps above. Fix: subtract `module.defined_func_base` (with an import-range check).
- [ ] `wir_optimize/const_forward.rs:117-123` vs `273-299` — a direct `LocalSet(x, LocalGet y)` marks both locals aliased (making `copy_field_knowledge` dead), while the identical `Seq`/`Block`-tail copy records knowledge and no alias — one direction pure precision loss, the other sound only via undocumented upstream invariants. Fix: one uniform policy (knowledge copy + invalidation for all three shapes).
- [ ] `wir_optimize/branch_hint.rs:102-135` — `arm_always_traps` is opaque to `Block`/`Loop`: an arm whose trap sits inside a non-escaping labeled block gets no cold hint. Fix: recurse into block bodies tracking label depth.
- [ ] `wir_optimize/peephole.rs:352-369` + `wir_optimize.rs:160-166` — no fixpoint inside `run_peephole`: `simplify_redundant_ref_ops` can fold a `RefTest` condition to a constant after `eliminate_const_if` already ran, and nothing re-runs it, shipping both branches. Fix: loop the sub-pass sequence to a cheap fixpoint.
- [ ] `wir_optimize/peephole.rs:1144-1149,714` — `fuse_local_tee` and `fold_branchless_increment` are blind to `Nop`s minted by phases 1-2 (cleanup runs at phase 7), so `set x; Nop; use x` never fuses; `elide_struct::find_adjacent_box_use` already skips Nops. Fix: skip Nops in these matchers or sweep before phase 5.
- [ ] `wir_optimize/peephole.rs:497-554` — `try_fold_comparison` folds only the ten i32 comparisons (i64 missing, though `try_negate_eqz_comparison` handles both widths); `I32Eqz/I64Eqz` of constants are never folded. Fix: extract a shared fold helper, add i64 and eqz-of-const.
- [ ] `wir_optimize/elide_local.rs:44-48,106-125` — the read set includes the dead store's own RHS (self-referencing dead stores like `x = x + 1` never elide), and recursion never descends value-position bodies (write-only locals inside if-expression arms are unreachable), contradicting the comment at line 110. Fix: exclude the candidate's own RHS from the read count and recurse into value positions.
- [ ] `optimize/sroa.rs:848-951` — `soft_expr` never peels `$value_copy$T` wrappers (unlike `container_sroa`'s `strip_one_value_copy`), so a candidate consumed by value (`g(s)`, non-move `return s`) hard-escapes though reconstructing inside the copy is sound. Fix: share the peel helper and treat the wrapped bare-local use as reconstructible.
- [ ] `optimize/container_sroa.rs:638-655` — `collect_candidates` iterates only the root block's statements; candidates declared in nested blocks are never considered while the escape analysis and rewriter walk the whole body (sroa.rs recurses). Fix: collect over `reachable_blocks`.
- [ ] `optimize/container_sroa.rs:597-633` — `required_methods_available` contradicts its own doc, requiring `Query` monomorphs for every element type instead of field 0 only; `find_sig_key_for_kind` is an O(catalog) scan per (candidate × field × kind). Fix: require `Query` only for field 0 and index the catalog by `(TypeId, ListMethodKind)`.
- [ ] `wir_optimize/sroa_variant_return.rs:678-716` — the pessimistic fix-point seed can never accept mutually tail-recursive candidates (each blocked on the other at round 1). Fix: optimistic assume-then-refute iteration.
- [ ] `wir_optimize/sroa_variant_return.rs:1374-1375,1451,1855` — candidate calls inside `If` conditions are blanket-invalidated globally though `recurse_rewrite_call_sites` routes `Seq`-shaped conditions through the full rewriter — the validator is more conservative than the rewriter, inverting the file's own doctrine. Fix: give the condition path the same wrapper-aware validation.

## P3 — structure, duplication, fragility

### Consolidate shared predicates and walkers

- [ ] One may-trap / purity oracle: `arena_query::is_pure_expr`, `mod_ref` `may_trap`, `elide_box_local::LeftmostWalk`, `select_lowering::is_select_eligible`, and `dce::expr_has_side_effects` each encode their own effect/trap taxonomy and already disagree (root cause of the P0 trap-deletion bug). Centralize per-`ExprKind`/`NirBinaryOp` classification in `arena_query`/`mod_ref`.
- [ ] One "locals possibly mutated in subtree" query: `const_folding::record_loop_write`, `condition_implication::node_modifies`, `condition_implication::build_copy_bindings`, and `copy_prop`'s two witness dispatches all differ in arm sets and rely on the undocumented invariant that mutating receivers/args always carry an explicit `MutRef` node. Consolidate in `arena_query`/`mod_ref` and document/assert the invariant once.
- [ ] `optimize/copy_prop.rs:329-386` vs `490-529` — `collect_mutated_locals` and `analyze_expr` duplicate the witness dispatch with different `CalleeArg` verdicts (`unwrap_or(is_mut)` vs `unwrap_or_else(may_mutate_through_arg)`) and different root resolution (`place_root_local` vs `storage_root`) — the asymmetry feeding the copy_prop P1. Extract one shared witness→mutated-root routine.
- [ ] `optimize/labeled_block_fusion.rs:395-508,551-701,1839-1994` (+ the two transform walkers and `subst_variant_payload_*`) — three near-identical break-exit walkers plus two rewriters with subtly different coverage; the divergences are the P0/P2 fusion bugs. Factor one label-aware exit visitor parameterized by a per-exit callback.
- [ ] `optimize/inline.rs:130-244,482-694,1129-1218` — four hand-rolled full `ExprKind` child enumerations (`count_expr`, `collect_callees_*`, `inline_expr_children`); `collect_callees_*` is ~200 lines whose only logic is recording `func_id`s. Rewrite on `for_each_child`/`NirRefVisitor`, leaving only the cross-arena `splice_*` bespoke.
- [ ] `optimize/multi_value_return.rs:456-734` + `optimize/dae.rs:278-292` + `optimize/drve.rs:216-290` — triplicated call-site-scan walkers that diverge (multi_value_return skips global initializer bodies; the others scan them). Extract a shared arena walker parameterized by a call-shape callback; align initializer coverage.
- [ ] `optimize/field_scalarize.rs:2183-2207,2280-2322,2055-2089/2727-2760,842-1052,1138-1333` — `walk_operand`/`walk_expr_operand` and `field_assign_to_candidate`/`field_read_to_candidate` are byte-identical; the if-walker pair differs only in the else-slot patch; three hand-rolled descent machines re-encode ref/call-arg classification (where the P1 gaps crept in). Collapse duplicates and rewrite the scans on shared visitors + `storage_root`.
- [ ] `optimize/sroa.rs:527-605,647-694,697-952,1016-1107` — four copy-paste traversals (`escape_*`, `field_access_node`, `soft_*`, `rewrite_*`) with the `Assign`-target special case maintained in triplicate; `soft_expr` threads 7 params through two `too_many_arguments` shims. Consolidate into one parameterized walker.
- [ ] `optimize/licm.rs:496-633` — `expr_child_nodes`/`stmt_child_nodes` re-implement `for_each_child` with a `Vec` allocation per node inside a ×10 fixpoint; `arith_structural_key` (1499-1544) builds `format!` string keys per subtree per round; the hoistable-unary set is copied in three places (1464, 2096, 2309); the doc block at 2022-2025 is attached to the wrong function. Use `for_each_child`, a hashable key type, and one predicate.
- [ ] `optimize/condition_implication.rs:762-867,947-1034` — `eliminate_checks_in_node`/`eliminate_le_checks_in_node` and `process_stmt`/`process_stmt_nested_loops` are near-identical pairs; the three eliminators run as full-subtree walks per top-level statement (each node visited O(depth) times). Factor the candidate walk over a refutation closure; run eliminators once from the root.
- [ ] `wir_optimize/sroa_variant_return.rs:1637-1700,1932-1954,2893-2994` — four hand-rolled Block/Seq result-unwrappers with divergent `Unreachable`/`Br` handling (the direct cause of two P1s). Extract one shared unwrapper parameterized over inspect/take.
- [ ] `wir_optimize/sroa_variant_return.rs:241-303` vs `wir_optimize/util.rs:123-273` — `reads_only_local_state` is a third hand-maintained effect-classification match that already lost the trap dimension. Rebuild from the util predicates.
- [ ] `wir_optimize/dce.rs:379-429` vs `651-700` — `collect_instr_type_refs` and `remap_type_ids_in_instr` are parallel 50-line matches that must stay in sync (a missed variant means a stale type index after compaction); the mangle→index resolution exists twice with a base-offset difference (dce.rs:167-174 vs util.rs:49-59). Derive both walks from one `for_each_type_id_slot` helper.
- [ ] `wir_optimize/peephole.rs:372-426` — `optimize_nested` hand-enumerates composite variants (missing `GlobalSet` values, `ArraySet/Get` operands, `Select`, `BrIf` conditions, `MultiValueLocalBind`) and re-invokes the whole-tree folds at every nesting level (O(depth) revisits). Drive body discovery with `WirMutVisitor` and run whole-tree folds once per function.

### String-typed and fragile identity

- [ ] `optimize/inline.rs:273-317,402-457,1220-1236` — the call graph and candidate map are keyed on `"{module_path}/{name}"` strings (with a 30-line doc about a mangling divergence the scheme must sidestep) although every call site carries a resolved `FuncId`. Key on `FuncId`; delete `function_inline_key` and the name-fallback heuristic.
- [ ] `optimize/dce.rs:967-1047,1049-1076` — `record_call`/`record_method_call` parse mangled names inline (`find("::")`, `find('^')`) and duplicate the base-name reconstruction; name-format knowledge belongs in `name.rs`. Move it there.
- [ ] `optimize/condition_implication.rs:54-67` — panic callees resolved by `f.name.contains("panic")`; a user `fn panic_free_parse` is classified as diverging. Mark the rt panic/unreachable builtins via `name.rs` or a lowering-set flag.
- [ ] `optimize/tmpl_hoist.rs:83-91` — `f.name.contains("array_new")` / `contains("ref.as_non_null")` / `== "String::with_capacity"` can capture unrelated user functions and feed the Formatter `buf` rewrite. Resolve by builtin identity or exact predicates in `name.rs`.
- [ ] `optimize/alias.rs:1078-1108` — `type_creates_alias` hardcodes `name == "Box" || name == "List"` while `CallImmutability::new` resolves the same via `compiler_items()`; a rename silently drops alias edges (the unsound direction). Resolve via `CompilerItem`.
- [ ] `optimize/dae.rs:150-165` + `optimize/array_literal.rs:57-95` — closure inspect impls matched by literal `"Inspect"`/`"InspectAlt"` strings; `builtin::array_new` found by name while the same pass uses `CompilerItem::ListPush` for push. Add `CompilerItem` markers.
- [ ] `optimize/licm.rs:342-345,407` — prior-round hoist handles recognized via `local_name.starts_with("_licm_")`, with the replacement-name invariant maintained by two `format!`s agreeing. Track hoist-created locals in a side set / flag.
- [ ] `optimize/value_copy_demote.rs:1259-1280,398-409,515-524,168` — `is_self_derived_op`/`is_self_derived_operand` are byte-identical duplicates; the `Assign`-form site path is unreachable (the arm's own rejection makes eligibility impossible); `format!("{}$shallow", …)` mints a mangled name inline. Dedupe, resolve the dead form, move `$shallow` into `name.rs`.
- [ ] `wir_optimize/nullable_ref.rs:454-510` — variant rewrites key on `field_name == "discriminant"` / `starts_with("payload_")` and one exact operand orientation, mirroring what `wir_build/pattern_match.rs` emits today; drift produces an invalid-Wasm ICE. Centralize the naming/shape constructors shared with the builder.
- [ ] `wir_optimize/sroa_variant_return.rs:154,1311-1333,1626,1970-1972,2876,2889,401-410` — `wir_types_equal` duplicates a derived `PartialEq` under a false justification; stale comments reference a deleted `WirInstr::ValueCopy`; `apply_sroa`'s doc describes a different function; a manual field-by-field clone instead of `derive(Clone)`; magic arity limits 4/8 undocumented. Clean up.

### Dead code, magic numbers, wasted work

- [ ] `optimize/extract.rs:18,27-46,274-555,583-596` — file-wide `#![allow(dead_code)]` hides that `ExtractLiteralRule` is wired into no production session and `is_ref_place` is unused; `freeze_pure_arith` is a ~280-line god function mixing three strategies. Drop the blanket allow, delete/cfg-gate unused items, split the function.
- [ ] `optimize/dae.rs:237-249` + `optimize/drve.rs:172-174` — `collect_pinned` always returns an empty set; the pinned-set plumbing survives in both passes. Delete it, keep the rationale as a doc note.
- [ ] `optimize/dae.rs:169-205` vs `optimize/sroa_param.rs:697-727` — `is_eligible` is a near-verbatim copy diverging only in trait-method policy. Extract a shared pinning helper with an explicit policy flag; also delete `SroaInfo.struct_type_id` (`#[allow(dead_code)]`) and consider porting `sroa_param` onto the engine edit API.
- [ ] `optimize/inline.rs:445-480` — recursion detection runs a full DFS per function (O(V·(V+E)) per invocation, every fixed-point iteration). Compute SCCs once with iterative Tarjan.
- [ ] `optimize/const_folding.rs:58-95,219-260,335-372` — `CalleeMap`, `GlobalEnv`, and `GlobalFieldEnv` (a whole-program walk with per-node `Vec` allocations) are rebuilt on every fixed-point iteration before gating starts. Cache across iterations; invalidate on global/signature changes.
- [ ] `optimize/copy_prop.rs:248-282,812-863` — `analyze_block` recomputes `mut_indices`/`first_read` by full-subtree walks at every nesting level, and `propagate_at_root` re-runs the whole-function analysis every round. Collect indices in one top-down walk.
- [ ] `optimize/scalar_forward.rs:63-98` + `store_load_forward.rs:209-217` + `extract.rs:599-607` — `is_assign_target` re-implemented three times beside a private engine copy (`nir_engine.rs:739-744`); `scalar_forward::apply_block` restarts from the block head after each single fold (quadratic on long inliner-generated blocks). Expose the engine helper; continue the scan past a fold.
- [ ] `optimize.rs:324-330,758-764,783-789` — the post-loop cleanup fixpoints are unbounded `while` loops (an oscillating rewrite pair hangs compilation); the main loop is capped. Add the same defensive cap with a debug diagnostic.
- [ ] `optimize/dce.rs:1660-1663` — primitive types seeded as `for i in 0..18 { types.insert(TypeId(i)) }`; adding a primitive silently desynchronizes. Replace with a `TypeTable` constant/iterator.
- [ ] `optimize/condition_implication.rs:185-196,359-367` — two unrelated `for _ in 0..8` chain caps. Name one shared `MAX_BIND_CHAIN` const.
- [ ] `optimize/field_scalarize.rs:612-617` — the pre-load builds the receiver `Local` with an out-of-range fallback typed as the field type; `field_access_expr` uses `c.local_type_id` unconditionally. Use `c.local_type_id` and delete the fallback.
- [ ] `optimize/tmpl_hoist.rs:1404-1411` — the buffer reset hardcodes `field_index: 1` for `used` though `extract_tmpl_candidate` has the real index in hand. Thread it through `TmplCandidate`.
- [ ] `optimize/loop_version_bce.rs:487-524,836-852` — `constify_check_temp` overwrites the first `let` of the temp it finds without verifying the initializer is the eliminated comparison, while `alloc_local_set` itself mints second `let`s for existing locals; `analyze_loop` versions only the first check's bound. Verify the matched let structurally; iterate candidate bounds.
- [ ] `optimize/mod_ref.rs:385-388,562-573` — the expression-position `LabeledBlock` arm doesn't push its label onto `open_labels` (a captured `break` is misclassified `NonLocal`, needlessly rejecting moves); the statement arm's comment promises `Conditional` that is never set; `Loop`'s `let _ = outer_control` is dead. Fix and reconcile.
- [ ] `optimize/alias.rs:852-953` — `collect_ref_arg_escapes`/`collect_mut_escaped_node` cover `Call`/`MethodCall` but not `IndirectCall`/`CmRawCall`; masked only by the value-graph builder bumping all heap state on those calls, and other consumers read `mut_escaped` directly. Handle the calls or document the invariant at both sites.
- [ ] `optimize/value_copy/mutation.rs:1-12` — module doc claims four consumers; only `copy_prop` uses it. Correct the list.
- [ ] `optimize/store_load_forward.rs:142-144` — `forward_at_root`'s doc claims a second caller (a "combined cse+forward session") that no longer exists. Fix the comment and visibility.
- [ ] `wir_optimize/dce.rs:64-105` — `collect_func_refs_recursive` clones every non-`Call` instruction subtree per statement to reuse `for_each_boxed_child_mut`, then delegates to a near-identical `_mut` twin; runs on the GC module too despite the "mem-module only" comment. Rewrite on `for_each_child`, delete the twin.
- [ ] `wir_optimize/nullable_ref.rs:147-246` — `update_type_definitions` is ~100 lines of `needs_update` re-matching reducible to a direct mutable walk calling `substitute_type`. Simplify.
- [ ] `wir_optimize/array.rs:18,78,226,251,185-194` — `ARRAY_NEW_DATA_THRESHOLD = 128` vs docs/optimizer.md's "≥16" (doc drift); `i32::try_from(...).unwrap_or(0)` silently clamps oversized lengths/indices (forbidden dummy fallback — contrast `util.rs:56`'s `expect`); the splitter skips global inits while the promoter visits them. Align doc, use `expect`, align coverage.
- [ ] `wir_optimize/init_guard.rs:57-66` vs `93-100` — guard blocks are counted in global initializers (`in_other_context = false`) but never nop'd there; unreachable today, a landmine. Pass `true` for the init scan or nop inits too.
- [ ] `wir_optimize/branch_hint.rs:240-259` — `br_only_arm` tolerates `ColdPath` markers but `else_is_empty` does not, blocking the `br_if` selection asymmetrically. Accept `ColdPath` in both (or document why not).
- [ ] `wir_optimize.rs:147-167,208-209` — half the WIR passes (`forward_struct_field_constants`, `promote_constant_arrays_to_data`, `split_large_array_literals`, the `run_peephole` loop, `flatten_seq_assignments`, `elide_multi_field_struct_locals`, `remove_trivial_init_globals`, `cleanup`) bypass `wir_pass`, so `WADO_LIST_PASSES`/`WADO_SKIP_PASS`/`WADO_DUMP_PASS_*` cannot see them. Wrap every pass.
- [ ] `wir_optimize/sroa_variant_return.rs:54,3415,324/633/1245/3175,2871,2884,1489-1505,3302-3356` — `collect_pinned_func_ids` computed twice per run; `wir_build::DEFINED_FUNC_BASE` hardcoded where siblings use `module.defined_func_base`; per-instruction `IndexSet` rebuilds and per-site whole-body rescans inside the fix-point (O(n²)). Compute the pinned set once, use the module field, hoist the candidate-id set, precompute per-function def/use indexes.
