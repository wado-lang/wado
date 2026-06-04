# Gale Development Guide

Immutable knowledge and standing rules for developing Gale. Volatile state (current pass/fail numbers, test inventory) lives in CI and the source tree; open work is tracked in [`TODO.md`](./TODO.md); the antlr4 compatibility contract, descriptor-test pipeline, and regeneration commands live in [`antlr4-compatibility.md`](./antlr4-compatibility.md).

## First: initialize the ANTLR4 submodule

Do this before anything else — the descriptor corpus, `.g4` semantics, and test regeneration all depend on it:

```sh
git submodule update --init --recommend-shallow vendor/antlr4
```

Files headed `// Do not edit by hand` are generated; never hand-edit them. To change one, edit its source (e.g. `status.toml`) and regenerate via `scripts/extract-antlr4-descriptors.sh` — which requires this submodule.

## Overview

Gale is a Wado-native parser generator. See [README.md](./README.md) and [WEP: Gale](../docs/wep-2026-03-02-gale.md) for design context.

## Compatibility Principle

Gale aims for full compatibility with the ANTLR4 `.g4` grammar syntax. The g4 parser must accept any well-formed grammar that the upstream `antlr4` tool accepts. Treat this as a hard contract: if you find a real-world `.g4` file that ANTLR4 accepts but Gale rejects, that is a bug in Gale.

The single intentional exception is action bodies, whose contents are skipped:

- `{ ... }` action blocks (rule-level, element-level, named actions like `@header`/`@members`/`@parser::name`)
- `{ ... }?` semantic predicates
- `catch [ ... ] { ... }` and `finally { ... }` exception handlers
- `@init { ... }` / `@after { ... }` rule prequel actions

The parser must still recognize these constructs (so files containing them parse without error) and preserve their presence and position in the surrounding IR — only the host-language code inside the braces is discarded. Everything else is first-class.

When fixing or extending the g4 frontend:

- Drive every change with a unit test in `src/g4/{lexer,parser}_test.wado` (TDD: failing test first, then implementation).
- If an existing test encodes a wrong expectation that diverges from ANTLR4, fix the test — the spec wins. Use the published `antlr-4.13.2-complete.jar` as a black-box oracle to confirm the expectation (see "License hygiene" below); do not read ANTLR4's implementation source to figure out what the right answer is.

## ANTLR4 Reference

The upstream ANTLR4 source is vendored as a shallow git submodule at `vendor/antlr4/` for two reasons: (1) the `runtime-testsuite/` descriptor corpus drives Gale's Stage A / Stage B regression tests, and (2) the JVM tool can be built locally as a black-box oracle for behavior verification. The vendored tree is **not** intended as a reading reference for implementation details — see "License hygiene" below.

### License hygiene — what you may read from `vendor/antlr4/`

Gale ships under its own license. ANTLR4 is BSD-3, but copying or paraphrasing its implementation creates a derivative-work risk for Gale. Therefore, while developing Gale:

- **DO NOT read** ANTLR4 implementation source: `vendor/antlr4/tool/**/*.{java,g}` and `vendor/antlr4/runtime/**/*.java` (and the same content under any other path). This includes `ParserATNSimulator.java`, `ATNConfig.java`, `LL1Analyzer.java`, and the bootstrap `.g` grammars under `tool/`. Algorithmic ideas inferred from reading the source belong to ANTLR4, not Gale.
- **OK to read**: `.g4` files anywhere under `vendor/antlr4/` (test-descriptor grammars, sample grammars). A `.g4` is data in the language Gale targets, not ANTLR4 implementation — reading one teaches you the user-facing grammar language, not how ANTLR4 internally implements parsing.
- **OK to read**: `vendor/antlr4/runtime-testsuite/**/*.txt` (test descriptors — observed input/output).
- **OK to run**: the published `antlr-4.13.2-complete.jar` from antlr.org on grammars + inputs to observe its behavior as a black box. This is clean-room oracle measurement and does not contaminate Gale.
- **OK to read**: `vendor/antlr4/doc/*.md`. These are prose describing the grammar / lexer / parser-rule semantics — effectively a third-party language spec, not implementation code. Refer to them when you need the canonical meaning of a `.g4` construct, but do not copy the text into this repo verbatim.

Initialize the submodule (first time only):

```sh
git submodule update --init --recommend-shallow vendor/antlr4
```

To bump the pinned revision later:

```sh
git -C vendor/antlr4 fetch --depth 1 origin dev
git -C vendor/antlr4 checkout FETCH_HEAD
git add vendor/antlr4
```

### Syncing the antlr4 test corpus

The upstream `runtime-testsuite/` ships ~345 descriptors that double as Gale's Stage A / Stage B regression suite. Re-extract them whenever you bump `vendor/antlr4` or edit triage state:

```sh
package-gale/scripts/extract-antlr4-descriptors.sh
```

See [`antlr4-compatibility.md`](./antlr4-compatibility.md) for the pipeline mechanics, triage workflow, and how to interpret results.

### Curated doc index

These are the upstream pages that matter most when working on the g4 parser or the lexer/parser code generator. Read them in roughly this order when ramping up. Per the License-hygiene rule above, prose `doc/*.md` pages are spec-like and OK to consult; the implementation source under `tool/` and `runtime/` remains off-limits.

| File                                                                                                | Why it matters for Gale                                                                                |
| --------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| [`vendor/antlr4/doc/grammars.md`](../vendor/antlr4/doc/grammars.md)                                 | Top-level grammar structure: combined vs. `lexer grammar` / `parser grammar`, `tokens {}`, `import`.   |
| [`vendor/antlr4/doc/lexer-rules.md`](../vendor/antlr4/doc/lexer-rules.md)                           | Lexer rule semantics: fragments, modes, channels, lexer commands (`skip`, `more`, `pushMode`, `type`). |
| [`vendor/antlr4/doc/parser-rules.md`](../vendor/antlr4/doc/parser-rules.md)                         | Parser rule semantics: alternatives, EBNF operators, labels, rule arguments and return values.         |
| [`vendor/antlr4/doc/left-recursion.md`](../vendor/antlr4/doc/left-recursion.md)                     | How ANTLR4 rewrites direct left recursion. Essential context for any parser-generator design choice.   |
| [`vendor/antlr4/doc/wildcard.md`](../vendor/antlr4/doc/wildcard.md)                                 | Semantics of `.` and non-greedy operators — easy to get wrong in code generation.                      |
| [`vendor/antlr4/doc/options.md`](../vendor/antlr4/doc/options.md)                                   | Grammar / rule / element options the g4 parser must accept (e.g. `caseInsensitive`, `assoc`).          |
| [`vendor/antlr4/doc/lexicon.md`](../vendor/antlr4/doc/lexicon.md)                                   | Lexical structure of `.g4` source itself: identifiers, literals, comments, escapes.                    |
| [`vendor/antlr4/doc/actions.md`](../vendor/antlr4/doc/actions.md)                                   | Action / attribute syntax. Gale skips these, but the parser must recognize and warn on them.           |
| [`vendor/antlr4/doc/predicates.md`](../vendor/antlr4/doc/predicates.md)                             | Semantic predicate syntax. Same story as actions: must be recognized and skipped.                      |
| [`vendor/antlr4/doc/target-agnostic-grammars.md`](../vendor/antlr4/doc/target-agnostic-grammars.md) | Best practices for writing host-language-free grammars — exactly the subset Gale targets.              |

For everything else, browse `vendor/antlr4/doc/` directly.

## Debugging Grammars with `gale dump`

`gale dump` lowers the grammar to GIR and prints a readable, per-rule
report of the actual prediction decisions — rule shape (Simple /
MultiAlt: Direct / MultiAlt: Tournament / LeftRecursive), per-alt first
sets, the per-overlap-group `PredictionNode` tree (Consume / Dispatch /
Leaf / Ambiguous, mirroring `build_prediction` in `parser_gen.wado`),
repeat strategies, follow-variants, and inlined prediction warnings —
followed by a summary of every warning. It reflects what the emitter
sees, not the raw surface IR. ATN-class decisions where static
prediction cannot disambiguate surface as `Ambiguous([alt N, alt M])`
under the relevant rule's `prediction:` section. There are no options;
multiple files are merged the same as `gale gen`.

(note: each `wado` command is actually `cargo run --bin wado`)

```sh
wado run package-gale -- dump path/to/Grammar.g4
```

## Tracing a parse with the `trace` option

`gale dump` is static (it shows the prediction decisions the emitter
*would* make). To see what the generated parser *actually does* on a
specific input — where the recursive descent bails, and which
alternative each multi-alt decision committed to — turn on the `trace`
option. It logs an indented event stream to stderr (via `log_stderr`):

- **`enter` / `ok` / `FAIL <rule>`** — one frame per parser-rule call.
  The innermost `FAIL` pinpoints the real culprit instead of the shallow
  "expected `<closer>`" error the caller surfaces.
- **`scan <rule> alt#N: -> @end` / `fail`** — per-alternative scan length
  the longest-match tournament weighed at a decision point.
- **`pick <rule> alt#N`** — the alternative the tournament / direct
  lookahead dispatch committed to (`no alt matched` when none scanned).
- **`try <rule> alt#N: ok` / `rewind`** — outcome of a speculative
  save-and-rewind attempt (the hybrid / fallback dispatch paths).

The instrumentation is strictly opt-in: with `trace` off (the default)
the generated parser is byte-for-byte unchanged. Enable it in a driver /
debug harness through the Kiln generator options:

```wado
use g from "./grammars/Grammar.g4"
    with { generator: { module: "../src/generator.wado",
                        options: { highlight: false, trace: true } } };
```

or from the CLI (`gale gen --trace Grammar.g4`). Example output for
`X X Y` against `a : X a Y? | X` (a shared-prefix tournament):

```
enter r @0 'X'
  enter a @0 'X'
      scan a alt#0: -> @2    <- 'X a Y?' scans furthest, so it wins
      scan a alt#1: -> @1
      pick a alt#0
    enter a @1 'X'
        scan a alt#0: fail
        scan a alt#1: -> @2
        pick a alt#1
    ok a
  ok a
  enter b @2 'Y'
  ok b
ok r
```

The `alt#N` indices match the per-rule alternative numbering shown by
`gale dump`, so a wrong `pick` cross-references straight back to the
prediction report. Group-internal `(a | b)` dispatch nested inside a
single alternative is not separately traced; promote the group to its
own rule if you need that granularity.

## Running Tests

```sh
# Run all Wado tests for this package
wado test package-gale/**/*.wado

# Run a specific file
wado test package-gale/src/codegen_test.wado
```

## E2E Test Architecture

Gale has three test layers, all driven by `.g4` files in `tests/grammars/` plus the upstream descriptor corpus.

### Layer 1: g4 parse tests (`src/g4/integration_test.wado`)

Verify that the g4 parser can parse real-world `.g4` files into `Grammar` IR without errors. Each test uses `#include_str` to load the `.g4` file and calls `parse()`.

```wado
test "parse JSON.g4" {
    let input = #include_str("../../tests/grammars/JSON.g4");
    let g = parse(input).unwrap();
    assert g.name == "JSON";
    assert g.parser_rules.len() == 5;
}
```

### Layer 2: driver tests (`tests/driver_*_test.wado`)

Driver tests verify generated parsers by parsing real input and checking the CST structure. Each test invokes the generator at compile time via `use ... with { generator: ... }` (Kiln inline invocation), then parses real input and uses `to_string_tree()` for ANTLR4-style S-expression output and `normalize_tree()` to write readable multi-line expected values:

```wado
use json from "./grammars/JSON.g4"
    with {
        generator: {
            module: "../src/generator.wado",
            options: { highlight: false },
        },
    };
use { normalize_tree } from "./grammars/JSON.g4";

fn assert_tree(input: &String, expected: &String) {
    let root = json::parse(input).unwrap();
    let tree = json::to_tree(&root);
    let actual = tree.to_string_tree();
    let norm = normalize_tree(expected);
    assert actual == norm, `\ninput:    {*input}\nexpected: {norm}\nactual:   {actual}`;
}

test "tree: nested object with array" {
    assert_tree(&"{\"a\":[1,true,null]}", &"
        (json
          (value
            (obj
              { (pair \"a\"
                  : (value
                      (arr [ (value 1) , (value true) , (value null) ])))
              })))
    ");
}
```

- `to_string_tree()` outputs `(ruleName child1 child2 ...)` with tokens as their text. EOF is omitted.
- `normalize_tree()` collapses whitespace (preserving quoted strings) so multi-line indented expected values compare correctly with compact single-line output.
- Both functions are defined in `runtime.wado` and available in all generated parsers.

#### Adding a new e2e test grammar

1. Add the `.g4` file to `tests/grammars/` (include `// Source:` and `// License:` headers).
2. Add a parse test in `src/g4/integration_test.wado`.
3. Add a driver test under `tests/` that imports the grammar via `use ... with { generator: { module: "../src/generator.wado", options: { ... } } }`. The compiler runs Gale on the `.g4` at build time and resolves the `use` against the freshly generated parser.

### Layer 3: ANTLR4 descriptor compatibility (`tests/antlr4-compat/`)

The upstream `runtime-testsuite/` is extracted into per-category Wado tests as a long-lived parse / parse-tree regression suite. Tracked in [`antlr4-compatibility.md`](./antlr4-compatibility.md) — read that for the stages, the descriptor pipeline, the regeneration commands, and the triage workflow.

## Inlined Runtime

`runtime.wado` is included verbatim into every generated file via `#include_str` in `codegen.wado`. It must remain self-contained (no imports from other source files). See [WEP: Compile-Time File Inclusion](../docs/wep-2026-03-02-include-str.md).

## Generated Parser Rules

- **No backtracking.** Disambiguate alternatives with static k-token
  lookahead prediction; if it cannot resolve within depth 5, file an issue
  rather than adding backtracking. An alt whose suffix is unscannable at a
  tournament site is a codegen-time `panic!`.
- **Multi-alt dispatch is a scan-side longest-match tournament.**
  `gen_scan_multi_alt` partitions atom alts by their depth-0 first token.
  Within a partition, `emit_scan_partition_body` commits on success for a
  lone alt, and for two or more tries every candidate from the same start
  and keeps the greatest successful end. Do not switch this to
  first-success-wins: it is unsound when alts share a prefix and tie on
  static length (`'mut'? IDENT` vs `path '(' ... ')'` on `N(n)`).

## Generated Lexer Rules

- **No backtracking in lexer codegen either.** ANTLR4's lexer obtains
  its longest match via NFA→DFA simulation
  (`vendor/antlr4/doc/lexer-rules.md`, `vendor/antlr4/doc/wildcard.md`).
  Gale replicates the same single-pass forward DFA with explicit
  accept-state tracking — never a try-fail-retry loop over remembered
  positions. When a greedy `Plus` / `Star` inner can also match the
  suffix's first character (`'a' ~('b')+ 'c'` is the canonical case from
  `Sets/UnicodeNegated*`), `gen_lexer_repeat_lookahead_aware` peeks the
  suffix at every iteration and records the latest position where it
  could legally start in `accept_<id>`. Once the inner stops matching,
  the cursor rewinds once to that position and the suffix is consumed
  normally. Regression fixture: `tests/grammars/lexer_greedy_suffix.g4`.
- **Trigger window is narrow on purpose.** The lookahead-aware emitter
  only fires when both the `Plus` / `Star` inner and every suffix
  element are single-char-consuming (Literal of one char, CharRange,
  CharClass, AnyChar, Not over any of those, or a Group of
  single-element alts). For shapes outside that window — a RuleRef
  inner / suffix, a nested Repeat, a multi-element-alt Group — the
  emitter falls through to the unchanged greedy loop. That is sound
  for every grammar in the corpus today because the surrounding inner
  cannot consume the suffix's first character in those cases. If a
  future grammar exercises the complex shape AND the inner does compete
  with the suffix, generalise the peek emitters rather than widening
  the trigger blindly.

## LL Prediction

Gale's parser-side prediction is a static FOLLOW-based repair on top
of SLL: when a `RuleRef R` call site's caller-local FOLLOW set
intersects `R`'s tail-greedy first set, Gale emits a
`scan_R__follow_<id>` / `parse_R__follow_<id>` variant whose body
suppresses the colliding tail-greedy iterations. Variants are emitted
through the same `gen_parse_fn_named` / `gen_scan_function_named` paths
the regular rules go through, so multi-alt and left-recursive rules
get variant emit "for free" under the unified naming scheme
`__follow_<id>{,_atom,_lr_N,_bt_N}`.

Two complementary mask shapes:

- **1-token mask** (`List<String>`) — the canonical intersection
  `tail_greedy_first(rule) ∩ caller_follow`. The variant body's iter
  dispatch subtracts the mask from `body_first_set`.
- **K-prefix mask** (`List<List<String>>`) — `mask[d]` is the set of
  caller tokens at input depth `d`. At iter entry, the variant body
  checks `peek_at(d) ∈ mask[d]` for every depth; on full match it
  yields to the caller. Admits multi-token-inner `Repeat`s (`(X Y)+`,
  `(X Y)?`, …) that the 1-token analysis rejects under soundness
  invariant 1 below. Registered only when the 1-token mask is empty
  (so the K-prefix path doesn't duplicate variants the 1-token path
  already covers).

Implementation references:

- `package-gale/src/follow_env.wado` — pure analysis. `FollowEnv`
  carries the per-rule `tail_greedy` snapshot and the call-graph
  fixed-point `rule_follow`. No codegen state; consumed read-only.
- `package-gale/src/gen_context.wado` —
  `tail_greedy_first_of_rule` / `tail_greedy_k_prefix_of_rule`,
  `element_is_first_exact`, `deep_suffix_is_first_exact`,
  `deep_position_first_sets_from`,
  `compute_call_site_follow_and_mask` /
  `compute_call_site_k_prefix_mask`,
  `compute_k_prefix_position_mask`,
  `intern_follow_variant`, `FollowVariantEntry`.
- `package-gale/src/parser_gen.wado` — `emit_follow_variant` (single
  dispatcher behind the fixed-point variant emit loop),
  `gen_parse_fn_named` / `gen_scan_function_named` (mask-aware
  body emitters), the LR helpers (`gen_lr_*` / `gen_scan_lr_*`)
  parameterised by `fn_name`, `ll_match_length`,
  `k_prefix_match_expr` / `k_prefix_match_expr_scan`,
  `emit_k_prefix_yield_gate`.

### Soundness invariants

Four invariants must be respected by any future LL-related change.
Violating them broke real grammars in the past, and the corresponding
guards are present as inline conservatism with explicit comments at
each site.

1. **Single-token tail-greedy inner.** Only `Repeat`s whose inner
   consumes exactly one token per iteration contribute to a rule's
   1-token `tail_greedy_first` set. Multi-token-inner Repeats can
   re-enter on the same first token at a deeper position (HTMLParser's
   `htmlContent` is the canonical example: `((htmlElement | CDATA |
   htmlComment) htmlChardata?)*` re-enters on TAG_OPEN), so
   suppressing them by a 1-token follow mask would break legitimate
   parses. The K-prefix path admits these Repeats because its
   per-depth gate distinguishes the closing-tag prefix
   `[TAG_OPEN, '/', TagName]` from the iter prefix
   `[TAG_OPEN, TagName, …]` structurally.

2. **First-exact deep-nullable suffix.** When a `RuleRef` site's
   suffix is deep-nullable, its first set may be unioned into the
   site's caller-follow only if every walked element is
   `element_is_first_exact` (single-token derived). Multi-element
   alts (CSS3's `(combinator simpleSelectorSequence ws)*` Star
   group) over-count: `combinator`'s first includes Space, but
   suppressing Space at the preceding `ws` strands the runtime on a
   lone Space.

3. **Variant emit reproduces the callee body faithfully.** All
   variant emit paths route through `gen_parse_fn_named` /
   `gen_scan_function_named` with `fn_name` set to
   `parse_<rule>__follow_<id>` (and analogous helper-name
   templates). No shrunken-duplicate body emitters; whatever shape
   the regular path can emit, the variant path emits as well.

4. **Wildcard alts collapse the overlap group and sort last.**
   `first_of_element(Wildcard)` returns `[]` because we cannot
   enumerate every token kind, but a wildcard alt effectively
   overlaps with every other alt that consumes at least one token.
   `compute_overlap_groups_with_wildcard` (in `alt_grouping.wado`)
   therefore merges wildcard alts and every non-empty-FIRST alt into
   a single overlap group, the parse-side dispatch suppresses the
   group's outer kind-check gate (so the inner scan-based
   first-success-wins is reached for any non-EOF lookahead), and the
   scan-side iteration order puts wildcard alts last (driven by
   `ScanGroupElem.wildcard_alt_indices`). Without this triple, the
   parse-side commits to the more specific alt on lookahead match
   even when its deeper structure cannot succeed — the
   `ParserExec/Wildcard` descriptor `(assign | .)+ EOF` over
   `x=10; abc;` is the canonical regression. Regression fixture:
   `tests/grammars/ll_wildcard_alt.g4`.

Beyond what static FOLLOW + K-prefix can decide, runtime ATN
simulation is the only complete answer (ANTLR4 implements one in
its runtime; do not read it — see "License hygiene" above).
Remaining gaps are tracked in [`TODO.md`](./TODO.md).

## Failed Approaches (Do Not Repeat)

### RuleRef Expansion via Return Stack (2026-03)

**Goal:** Expand multi-token RuleRefs during SLL prediction to reduce backtracking.

**What was tried:** Added `return_stack` to `SllConfig` to track continuation points when entering a referenced rule. `sll_expand_rule_ref` pushed return frames and advanced inside sub-rules. `try_expand_opaque` called expansion when `build_sll_node` would otherwise produce `Ambiguous` (then named `Backtrack`).

**Why it failed (3 distinct bugs):**

1. **Consume node corruption:** `build_sll_node` emits `Consume(element, child)` when all configs share a common terminal. For expanded configs inside a sub-rule, this emits `p.expect(K_FROM)` at the _decision point_, consuming a token that belongs to the referenced rule (e.g., `delete_stmt`). Fix attempted: `strip_all_consume` — but this loses disambiguation information.

2. **Depth-mixed Dispatch:** Expanded configs produce Dispatch branches for tokens _inside_ sub-rules (e.g., `K_RECURSIVE` from `with_clause`). When multiple alternatives share the same prefix rule (`with_clause`), these dispatches are meaningless — every alternative sees the same tokens. The generated parser enters wrong branches and fails or times out.

3. **Dedup false resolution:** `sll_dedup_by_alt` keeps one config per `alt_index`. When two alternatives expand to configs with identical FIRST sets (e.g., `join_clause` and `table_or_subquery` both start with `table_or_subquery`), dedup merges them into a single alt. The prediction then emits a `Leaf` for the wrong alternative, silently dropping the other.

**What remains:** The `return_stack` field on `SllConfig`, `push_return`, `pop_return`, and return-stack-aware `sll_config_first` / `sll_advance` are committed as zero-overhead infrastructure. They don't affect generated output.

**Lessons:**

- Tokens from inside expanded sub-rules cannot be used for prediction at the decision point level.
- To use expansion correctly, the prediction must map expanded tokens back to the decision point's lookahead depth (essentially an ATN simulator).
- `sll_dedup_by_alt` is too aggressive for expanded configs — alternatives sharing sub-rules get merged.

### LL(\*) variant emit — three over-broad attempts (2026-05)

**Goal:** Static-analysis-based one-level LL(\*) repair via per-(rule, follow-mask) `__follow_<id>` variants. Regression suite at `tests/grammars/ll_*.g4`.

**Three attempts that broke real grammars and had to be narrowed back:**

1. **Swapping `alt_sort_priority` 2 ↔ 3 globally** (so multi-element-RuleRef alts beat single-RuleRef alts everywhere). This made `(a b | a) EOF` pick `a b` correctly but broke `LeftRecursion/PrefixAndOtherAlt_*` — `expr : literal | op expr | expr op expr` would try `op expr` before `literal` for input `-1`, committing to a non-LL alt. Fix: keep priority unchanged at the rule level; introduce `sort_group_by_mandatory_count_desc` and use it ONLY at group-level dispatch sites (`gen_consume_group*`, `gen_general_group_store*`, `gen_group_prediction_code_skip` Ambiguous).

2. **Adopting any tail-position Repeat into `tail_greedy_first`** regardless of the inner element shape. This treated HTMLParser's `htmlContent` rule (`htmlChardata? ((htmlElement | CDATA | htmlComment) htmlChardata?)*`) as having tail-greedy = first set of the inner Group, including `TAG_OPEN`. The inner Group's `htmlElement` alt **legitimately** re-enters on `TAG_OPEN`, but the variant's mask suppressed all TAG_OPEN-led iterations, breaking nested-tag parses. Fix: restrict `tail_greedy_first_of_element`'s `Repeat` arm to `Repeat`s whose inner is `element_is_single_token` (soundness invariant 1 above). The K-prefix mask path admits these Repeats safely via per-depth gating.

3. **Registering variants for any `RuleRef` call site with a non-empty caller-side follow** — including suffix-nullable positions where the local follow propagates the outer rule's follow. This fired on CSS3's `selector : simpleSelectorSequence ws (combinator simpleSelectorSequence ws)*`, where `ws`'s follow at position 1 is `first(combinator) = {Plus, Greater, Tilde, Space}`. The variant suppressed `ws` from consuming Space, leaving Space for the (often-empty) combinator loop and breaking `* { … }` selectors. Fix: `compute_call_site_follow` drops the suffix's first set when the tail at `i + 1` is deep-nullable but NOT first-exact, returning only the variant's outer follow (soundness invariant 2 above).

**Lessons:**

- Static analysis can't distinguish "tail-greedy that should yield to caller" from "tail-greedy that legitimately re-enters." The conservative side is silent failure (variant doesn't fire); the unsound side is broken parses.
- Each LL repair must be paired with a regression fixture covering the rejection case, not just the hit case — otherwise the next contributor relaxes the guard and quietly breaks `htmlContent` / `selector` again.
- A single global rule cannot decide "should this token be consumed here or by my caller?" — that's why a runtime simulator (closure / DFA cache, as used in ANTLR4) is the complete answer (out of scope to inspect; see "License hygiene" above). A static repair will always have edges; pick the edge that matches today's grammar set and add a fixture so it stays the edge.
