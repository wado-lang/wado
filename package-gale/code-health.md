# Gale Code Health

Findings from a full-source audit (2026-06-10) of `package-gale/` (~40k lines: `src/`, `src/g4/`, `src/runtime/`, `scripts/`, `tests/`). This file tracks accumulated tech debt: bugs, duplicated logic, naming drift, and quality decay. Feature/compatibility work stays in [`TODO.md`](./TODO.md); this file is only about code health.

How to read:

- This file lists only what is **not yet done**. Closed items are removed; the fix lives in commit history.
- "✓ verified" means the finding was re-confirmed by reading the cited code during the audit; others were identified with quoted code but deserve a failing test first.
- Line numbers are as of the audit commit and will drift.

The debt clusters into four recurring themes:

1. Twin-path divergence: parse-side/scan-side, storable/transparent, tokens/channels, compile-time/runtime pairs are maintained by copy-paste, and fixes land on one side only. This is the dominant bug source.
2. Remnants of retired mechanisms (`__follow_<id>` variants, `_bt_` backtracking, "Phase 2.x" migration): dead parameters, comments that are now false, and one production-dead module.
3. Cross-layer naming drift: three or four words for one concept, and a few names that say X while the code does Y.
4. No error-handling policy: panic / Result / silent fallback / exit-0 coexist in one pipeline.

## Bugs

### Soundness and compatibility divergence

These are the highest-risk remaining bugs: a static-prediction edge or a parse/scan asymmetry that can mis-parse valid input. Several need their own focused PR with full-corpus validation rather than a quick patch (the CLAUDE.md "LL Prediction" notes the static path "will always have edges").

- [ ] SLL prediction under-approximates and emits incomplete Dispatch trees (valid input rejected, since codegen emits Dispatch with no else-fallback):
  - `sll_advance` collapses `+`/`*` repeats to "consumes exactly one token" (`a : X+ Y | X Z` mispredicts on `X X Y`). `src/prediction.wado:522-539`
  - `try_expand_opaque` lacks the at-end-config handling its template `build_sll_node` has, dropping at-end alts from the returned Dispatch. `src/prediction.wado:753-901` vs `:712-745`
- [ ] Scan/parse EOF asymmetry: parse-side `expect`/`match_set` match `TK_EOF` without advancing (matchedEOF), scan side emits `pos += 1` unconditionally — scan over-counts by 1 whenever EOF is the last matched element, which can flip a tournament tie. `src/parser_gen.wado:1311-1314`, `:1388`, `:626` vs `:312-316`, `:413-416`
- [ ] Tournament/scan-gate call sites never forward the runtime `follow` argument (helpers run with `&EMPTY_FOLLOW`) while the corresponding parse calls forward it — violating the documented scan/parse lockstep invariant on FOLLOW-gated grammars. `src/parser_gen.wado:5856`, `:5905`, `:5913`, `:5447`, `:5455`
- [ ] SimpleCst group scan lowering threads `outer_follow` while the parse-op path threads `empty_follow`, and the two comments contradict each other about which is sound. Decide once, fix the other side, pin with a fixture. `src/lower.wado:2783-2796` vs `:2820-2841`
- [ ] A label on a Transparent group (`x=(ID)`) silently drops the binding: `rebind_group_shape`'s Transparent arm returns unchanged and the promised caller recursion does not exist; the inner field was also deduped against a throwaway scope. `src/lower.wado:3962-3972`, `:3638-3683`
- [ ] `\P{...}` (negated Unicode property) is parsed as literal chars `P { L }` (only lowercase `p` is detected); unknown `\p{...}` properties expand to an empty set silently. At minimum warn. Full handling needs Unicode complement ranges (Gale's `\p` support is already a hand-rolled approximation). `src/g4/parser.wado:1517`, `:1546-1616`
- [ ] GIR-level multi-alt dispatch has no wildcard-alt awareness (soundness invariant 4 is only applied on parser_gen's surface-IR paths); a wildcard alt gets an empty-token branch in a `Direct` dispatch. Also `alt_is_wildcard_led` does not unwrap labels, so `w=.` escapes the wildcard machinery entirely. `src/lower.wado:1450-1494`, `src/alt_grouping.wado:31-41`
- [ ] Overlapping-but-unequal first-char ranges in the lexer dispatch shadow later rules: groups are keyed by exact guard string and emitted as `if/else if`, so a char in the intersection only tries the first range group; the wildcard fallback containing all range calls is unreachable for it. `src/lexer_gen.wado:1629-1666`, `:1702-1728`

### Pipeline and tooling correctness

- [ ] WASI helpers drop completion futures unchecked: `write_file_string` returns `Ok(())` on partial/failed writes; `read_file_to_string` cannot distinguish mid-stream errors from EOF (silently truncated grammars could be committed). `scripts/extract_antlr4_descriptors.wado:450-469`, `:419-448` — BLOCKED on a wado-compiler bug: consuming a `Future<Result<(), ErrorCode>>` (`done.read()`) ICEs the CLI-world compile (unit-payload result lifting at the CM boundary; "WIR pipeline generated invalid core Wasm module — values remaining on stack at end of block" in the consuming fn). Same family as the `Exit::exit(Result<(),()>)` ICE. The intended checks are in place as TODO(compiler) comments at both helpers and in `src/main.wado::read_all`; re-apply once the compiler is fixed.
- [ ] Emitters write `#[TODO]` before `// triage:` but every committed generated file has the reverse (post-`wado format`) order — raw regeneration output is not format-stable. Emit in the post-format order. `scripts/extract_antlr4_descriptors.wado:676-678` and 4 sibling emitters
- [ ] `parse_cli_args` silently ignores unknown `--` flags (a typo'd `--finalize-stage-b-oracl` runs a full extract). `scripts/extract_antlr4_descriptors.wado:1034-1038`
- [ ] Any stderr output from TestRig is classified as a parse error (exit 2), so benign runtime warnings drop the entry from Stage B′; the oracle jar is fetched with no checksum. `scripts/antlr4-oracle.sh:188-199`, `:129-140`
- [ ] `action_strip`'s `[...]` now ends at the first unescaped `]` (correct for char sets, the corpus case). This loses the depth tracking that handled a rule-argument / `catch` action whose host type contains `[]` (`r[int[] arr]`, `catch [T[] xs]`): such an action ends early and its remainder leaks into the grammar text. No corpus grammar exercises this (all nested-`[` cases are char sets), but a context-aware stripper (distinguish set vs arg-action by position) would handle both. `src/g4/action_strip.wado:38-61`

### Diagnostics and minor

- [ ] `gen_error_fallback` puts internal constant names (`TK_IDENT`) in user-facing "expected" lists while the `expect` path uses `token_kind_name` — two error paths, two vocabularies. `src/parser_gen.wado:6290-6313`
- [ ] Error-token text is a message, so diagnostics read `unexpected token "unterminated string"`. `src/g4/lexer.wado:110`, `src/g4/parser.wado:1107`
- [ ] `ParseError.expected` is populated everywhere but rendered by nothing (the Display impl omits it). `src/runtime/lex.wado:166`, `:207-214`
- [ ] Empty lookahead `sig` is guarded on the scan side but not the parse side, where `gen_lookahead_condition` would emit syntactically broken code (`if` / `&& ()`); either the guard is dead or the parse side is missing it. `src/parser_gen.wado:1679-1681` vs `:3178`, `:3240`
- [ ] Diagnostic-to-rule association is by substring on a free-form label; `Diagnostic.rule` carries labels like `"SimpleCst group"` with no rule name, so the `(rule, message)` dedup can collapse diagnostics from different rules. Already tracked in `TODO.md` ("Structured diagnostic-to-rule identity") — kept here for completeness. `src/gir.wado:106-109`, `src/dump.wado:505-517`
- [ ] List-label leaf path double-bumps the inner name counter (lower bakes one bump, codegen applies two), and the Group arm lacks the collision rebind the leaf arm has — both in the dedup bug class `codegen_label_collision_test.wado` exists for. Also the non-greedy transparent first iteration dedups outer-scope bindings against a fresh counter table. `src/parser_gen.wado:3502-3530`, `:3480-3493`, `:3835-3838`
- [ ] `gen_bt_scan_op_elements` treats the last scanned-prefix element as "trailing self-ref" even when the prefix is partial, short-circuiting the LR climb and under-scanning the tournament prefix. `src/parser_gen.wado:1253-1258`
- [ ] Static `gen_scan_lr_suffix_dispatch` lacks the no-progress guard its ATN twin has; combined with the `continue` that skips emitting `scan_X_lr_N` for 1-element LR alts, a future `valid_lr_alts` change could reference an undefined function. `src/parser_gen.wado:825-885`, `:743` vs `:819-821`

### Unchecked-argument quality nits (non-crash)

- [ ] Malformed lexer command *arguments* are still unchecked (the paren panics are fixed): `pushMode(42)` interns a mode literally named `42`, `-> ;` yields the odd "unknown lexer command ;". Validate the argument is an identifier. `src/g4/parser.wado:1232-1290`

## Logic duplication

The biggest single lever: most twin-path bugs above exist because the second copy missed a fix.

- [ ] Parse-side/scan-side twin emitters in `parser_gen.wado` (~15 pairs, est. 800-1000 unifiable lines): `gen_lr_overlap_dispatch`/`gen_scan_lr_overlap_dispatch` (117/112 lines, ~90 shared), the LR suffix dispatch pairs, the consume/general group store pairs (where the missing panic crept in), three lookahead-condition builders, the save-and-rewind blocks, the 12-line RuleCall-dispatch emit block (×4).
- [ ] Group-classifier chains duplicated op-side/scan-side in `lower.wado` (×4 pairs: `lower_group_op`/`lower_scan_group` etc.) — the SimpleCst follow contradiction lives here. Plus four hand-rolled deep walkers for self-ref stamping and five places that enumerate every `RepeatOp` field by hand (one claims to be the single point of change; it is not).
- [ ] Verbatim cross-module copies: `compute_overlap_groups` (`lower.wado:1357-1397` ≡ `alt_grouping.wado:108-148`), `alt_sort_priority` (`lower.wado:1602-1623` with bare magic numbers vs `alt_grouping.wado:200-219` with the exported constants), `dedup_name`/counter helpers (`lower.wado:4035-4062` ≡ `gen_util.wado:603-632`; the "import churn" justification is stale — lower already imports ~20 symbols from gen_util).
- [ ] Set-helper zoo: `first_contains` ≡ `first_set_contains` (both used in `lower.wado`), `sets_overlap` ≡ `first_sets_overlap`, `subtract_first` ≡ `subtract_sets`, `extend_dedup`/`dedup_append_arr`/`union_kind_arrays`. Three functions would cover all nine.
- [ ] Two escape resolvers (`read_string_escape` vs `resolve_char` — both fixed in place but still separate, should be unified), four balanced-delimiter scanners with conflicting failure conventions (-1 vs end-of-input), four comment-skipping implementations, duplicated postfix/alternatives parsing (`parse_postfix`/`parse_lexer_postfix`, `parse_alternatives`/`parse_lexer_alternatives`), duplicated `{ ID (, ID)* }` block scanners (tokens/channels — the hang fix landed in one, now both).
- [ ] `best_*` state-reset strings rendered in four places. `src/lexer_gen.wado:1352-1423`, `:1846-1900` (token numbering is now unified via `token_slot_order`).
- [ ] `generator.wado` still copies the open → read → parse → merge → synthesize → check pipeline that `main.wado` now folds into `load_and_merge`. `src/generator.wado:55-88`
- [ ] `try_expand_opaque` re-implements `build_sll_node`'s dispatch construction (where the at-end handling got lost); `dump.wado::render_prediction` hand-mirrors `gen_multi_alt_body_bt`'s grouping+sort+`build_prediction(…, 5, …)` pipeline.
- [ ] Test helpers copy-pasted per file: `assert_tree` ×33, `assert_parses` ×8 (+2 near-clones), `unparse_xml` ×3, hand-rolled `str_contains` ×5 (stdlib `String::contains` exists and is used elsewhere). Create a shared test-support module.
- [ ] Extractor emitter boilerplate ×6 (~80% identical bodies); `category_to_snake` duplicates `to_snake_case` with different acronym behavior (a latent naming divergence). `scripts/extract_antlr4_descriptors.wado:541-557`, `:659-998`, `:1841-1857`
- [ ] Name-dedup machinery re-implemented in `visitor_gen.wado` (`get_name_count`/`increment_name_count` + inline `dedup_name`) instead of importing `gen_util`. `src/visitor_gen.wado:236-240`, `:392-410`
- [ ] Architectural duplication: `codegen.wado` reads pre-classified GIR shapes, but `visitor_gen.wado` re-derives the same field names from the surface IR with its own group-counter logic — numbering drift produces non-compiling field names. `src/codegen.wado:347-369` vs `src/visitor_gen.wado`

## Naming inconsistency

Decide each convention once, record it here, then migrate mechanically.

- [ ] `alt_index` (116 uses) vs `alt_idx` (92) — pick one. Index soup generally: `idx`/`index`/`i`/`ei`/`ai`/`ri`/`gi`/`li`/`real_idx`.
- [ ] `gen_` (128 fns) vs `emit_` (27) with no discernible rule, in the same files at the same altitude. Define the rule (or collapse to `gen_`).
- [ ] One pipeline stage axis, three vocabularies: surface `Element`, parse-side GIR `Op`, scan-side GIR `ScanElement` with `...Elem` suffixes (`ScanRuleCallElem` vs `RuleRefElement`). Also `ruleref` (326) vs `rulecall` (174) spellings.
- [ ] `ctx` means both `GenContext` (everywhere compile-time) and a `CtxArena` DAG node id (`runtime/atn.wado` `AtnConfig.ctx`). Rename the runtime one (e.g. `pctx`).
- [ ] `kind` is overloaded across token-kind constant strings, state kinds, transition kinds, `RepeatKind`, `FieldKind`, etc.; the string-typed token kind is a constant name and should be named like one. `pos` means token index (runtime) and element index with a `-1` opaque sentinel (`SllConfig`) — name the sentinel.
- [ ] `ci` is the case-insensitivity flag project-wide and also a loop variable in `lexer_gen.wado:1262` and `:1642` — rename the loop vars.
- [ ] Names that lie: `take_diagnostics` copies without draining (`src/gen_context.wado:289-291`); `compute_first_chars` builds a full `TryCall`; `skip_rule_prequel` returns harvested options; `rep_recovery_call` calls nothing; `contains_upper` is plain substring search; `main_test.wado` tests the g4 parser, not `main.wado`; `iter_follow_unused` is used.
- [ ] GIR doc comments that contradict the implementation (fix the docs): `LrAlt.own_prec` says the last LR alt has the highest precedence (implementation and pinning test say the first); `MultiAltRule.scan_dispatch` says it "frequently differs from `dispatch`" (always a byte-identical clone, and neither named sort is used); `rule_alt_body` doc says it returns null for LR alts (it falls back to `lr.suffix`). `src/gir.wado:268-271`, `:208-213`, `:1020-1023`
- [ ] Retired vocabulary still load-bearing: `_bt_` infix on helper names (backtracking is gone; the alt's twin helpers are `{fn}_bt_{i}` vs `{fn}_scan_{i}`); `k_prefix_match` vs `k_match_scan` for the same runtime gate, with comments still describing the retired K-prefix mask mechanism. `src/parser_gen.wado:457`, `:1443-1452`, `:3938-3941`
- [ ] `follow_sep` is a module function and a local variable shadowing it (`src/parser_gen.wado:109` vs `:1319`, `:1393`, `:4149`); `atn_needs` vs `needs_atn` (`:1874`).
- [ ] Diagnostic owner label parameter named `ctx_name`/`ctx_label`/`rule`/`label` across sites. "preamble" vs "prequel" for the same g4 concept; `LexerActions` for what diagnostics call lexer commands; `Alternative` vs `LexerAlt`.
- [ ] File naming: `scripts/` mixes snake (`extract_antlr4_descriptors.wado`) and kebab (`strip-grammar.wado`, `*.sh`); `tests/` root breaks the `driver_*_test.wado` convention twice (`sqlite_case_when_test.wado`, `sqlite_regression_test.wado`).
- [ ] `tests/generated/` dir/module/grammar three-way mismatches (19 found): `ll_basic/llbasic.wado` (grammar `LLBasic` + acronym-collapsing `to_snake_case`), `overlap_tournament/` drops the `ll_` prefix its source has, `trace/` is named after the option not the grammar, `antlr4/antlrv4.wado/ANTLRv4Lexer.g4`, `error_recovery/err_rec.wado`, three dirs all containing `json.wado` while five sqlite tests share one dir. Fix by aligning declared grammar names with fixture file names and regenerating; `ll_basic.wado:2` also stamps `sources = ["LLBasic.g4"]`, a file that does not exist.
- [ ] Two CamelCase→snake conventions coexist: `to_snake_case` (acronym-collapsing, `src/ident.wado`) vs `category_to_snake` (script-local).

## Code quality

### Dead code

- [ ] `src/follow_env.wado` is production-dead: its only importer is its own test, while `CLAUDE.md` presents it as live LL infrastructure. Wire it in or mark it as parked scaffolding explicitly.
- [ ] Dead weight shipped inside every ATN-bearing generated parser: `ATN_MAX_STACK`/`ATN_MAX_CONFIGS` (unused, with comments claiming they guard the closure — `ATN_CLOSURE_GUARD`/`ATN_ARENA_CAP` actually do), `AtnSim.tr_src` (decoded and stored, read only by a test), duplicated-then-unused `escape_html`. `src/runtime/atn.wado:75-80`, `:101`, `src/runtime/highlight.wado:125-139`
- [ ] Parameters surviving from the retired follow-variant pass: `emit_visibility` (threaded through 3 fns, always `true`), `allow_lr_split` (always `true`). `src/parser_gen.wado:1954-2059`, `:524-571`
- [ ] `wadopoet.wado` API kept alive only by its test: `FlagsSpec`, `TraitSpec`, `add_mut_param`, `ParamSpec::new_mut`, `GlobalSpec::set_mut`. `src/wadopoet.wado:296-325`, `:480-529`
- [ ] Misc: `sll_closure` is an identity function advertised as part of the simulator (`src/prediction.wado:253-257`); unused `display` local (`src/parser_gen.wado:3599`); unreachable `return` after panic (`:4481-4482`, `src/runtime/cst.wado:129-130`); dead `name == "EOF"` disjunct (`src/g4/parser.wado:1027`); `let _ = name_span;` (`:804`); dead imports (`Span`, `make_token` in `g4/parser.wado`; `TreeMap` in `lower.wado:81`; several in `dump.wado:21-52`); dead locals `outer_follow` (`src/lower.wado:1174`, `:2266`) and unused `outer_follow` params (`:2559`, `:2901`); unreachable highlight-classifier arms (`src/highlight_gen.wado:131-165`); `compute_first_chars`' dead `use_modes` branch (`src/lexer_gen.wado:1391-1402`); `load_triage_map` wrapper with no callers (`scripts/extract_antlr4_descriptors.wado:403-408`); leftover empty loop + `has_b` in `tests/driver_json_test.wado:133-136`; stray `__DATA__` section in `tests/driver_sqlite_create_table_test.wado:40-41`; unused imports in `driver_sexpression_ast_test.wado` and `driver_typescript_test.wado`.
- [ ] `is_wado_reserved` contains non-keywords (`move` — a Rust leftover — plus `scope`, `panic`, `handler`, `unreachable`) and misses contextual keywords it should arguably escape; the over/under-inclusion policy is undocumented. `src/ident.wado:10-44`
- [ ] Single-variant enum `ScanRepeatStrategy { Plain }` carried since the non-greedy scan strategy was abandoned. `src/gir.wado:925-927`

### Stale and contradictory comments

- [ ] "Phase" fossils in `lower.wado`: the header still says codegen does not call `lower` yet (it does, `src/codegen.wado:87`); "Phase 2.3a only handles disjoint first-sets" sits above Tournament handling; "remaining shapes panic with TODO Phase 2.x markers" — no such panics remain; "Phase 2.6 variant masks" references the retired mechanism. `src/lower.wado:40-42`, `:524-527`, `:576-578`, `:2264-2265`, `:2330-2332`, `:2998-3002`
- [ ] `gen_scan_multi_alt`'s comment claims first-success-wins is correctness-equivalent to the tournament — directly contradicted by `emit_scan_partition_body`'s own doc citing the #1245 fix. `src/parser_gen.wado:1114-1120` vs `:1175-1182`
- [ ] Five driver tests document the retired `__follow_<id>` variant mechanism as current behavior (`tests/driver_ll_basic_test.wado:20-21`, `driver_ll_multi_alt_overlap_test.wado:27-30`, `driver_rust_test.wado:110-113`, `driver_ll_multi_token_tail_test.wado:19-27`, `driver_ll_k_prefix_cascade_test.wado:19-23`).
- [ ] Doc blocks fused onto the wrong function (one comment documenting two functions): `src/parser_gen.wado:1782-1802`, `:2624-2630`, `:3018-3027`; a 30-line doc block attached to `stage_b_oracle_eligible` actually describes `normalize_output_for_stage_b` (`scripts/extract_antlr4_descriptors.wado:1635-1667`); mangled doc on `repeat_outer_base_name` (`src/lower.wado:3560-3568`); duplicated/stacked doc on `lower` itself (`:192-227`).
- [ ] Citations of forbidden ANTLR4 implementation sources (`tool/.../ANTLRParser.g`) in comments — either stale or a license-hygiene breach; resolve and replace with `doc/*.md` citations. `src/g4/parser.wado:120`, `:236-238`, `src/g4/parser_test.wado:958`, `:971`
- [ ] Smaller: `gen_context.wado:853-855` cites a nonexistent "Failed Approaches" section; packed-key width comment disagrees between test and code (`src/atn_sim_test.wado:328-329` vs `src/runtime/atn.wado:653-662`); "k=5 lookahead exhausted" label overstates the bound since Consume nodes do not count against depth (`src/dump.wado:378`, `src/prediction.wado:617-629`); obsolete "FLIPPED GREEN" narrative inside `[stage_a_todo]` in `tests/antlr4-compat/status.toml:64-69`.

### Known representation gaps

- [ ] Surrogate / astral handling in char ranges: `CharRange` endpoints are Wado `char` (Unicode scalars), so a surrogate code point (`[\uD800-\uDBFF]`, legal in ANTLR4 for matching UTF-16 code units) cannot be represented — the escape resolvers now fall back to U+FFFD instead of trapping, but a surrogate _range_ collapses to a single replacement char. Full support needs a wider char-range representation (i32 code-point endpoints). `src/g4/parser.wado` `resolve_unicode_escape`, `src/ir.wado` `CharRange`.

### Structure and policy

- [ ] Magic numbers needing one home: lookahead depth `5` (×3 + the literal "k=5" string), config-explosion guard `200` (×2), `TK_LIT_` prefix length `7`, merge-cache stride `1000000` (holds only by cross-file coincidence with `ATN_ARENA_CAP`), `>20 && >20` broad-set threshold, `min_len = 9999`, stream chunk sizes 8192/4096/64. (`MAX_SHAPE_OPTIONALS` = 8 is now named.)
- [ ] Worst oversized functions: `extract_antlr4_descriptors.wado::run` ~585 lines, `gen_tokenize_fn` ~330, `gen_parser_struct` 263, `gen_prediction_code_inner` 262 (5-deep strategy nesting), `sll_advance` ~250, `gen_parse_fn_named` 222, `parse_grammar` ~180, `lower_repeat_op` ~180, `strip_action_bodies` ~170, `build_dispatch_groups` ~148. `lower.wado` (4062 lines) mixes five concerns and would split naturally.
- [ ] Error-handling policy: define when to panic vs return Result vs warn, then fix the outliers — silent placeholder fallbacks that generate wrong code (`group_case_name` → `"Unknown"`, `element_field_info` → `["_", "Token"]`), non-exhaustive `if let` dispatchers on the scan side that silently emit nothing for a new variant while the parse side panics (`src/parser_gen.wado:1304-1381`, `:1386-1438`, `:3590-3679`).
- [ ] `FollowArg` masks are interned (and emitted as `FOLLOW_MASK_*` globals) for every element position including non-rule-calls that discard them — dead globals plus unstable mask ids. `src/lower.wado:151-178` and its 5 call sites
- [ ] Kind-set pre-interning walk misses `RepeatOp.scan_body` and `GroupOp.scan_alts`, defeating its stated id-anchoring goal for sets that only surface through the scan mirrors. `src/lower.wado:473-492`
- [ ] wadopoet is bypassed exactly where its missing features matter: no `export`/effects support forces a hand-written `export fn` (`src/highlight_gen.wado:85-116`, with mid-module `use` imports), no generics support means `visitor_gen.wado` uses zero specs; meanwhile other emitters bypass it without that excuse. Extend `FnSpec` (export, effects, generics) and migrate.
- [ ] Performance papercuts (non-blocking): O(n²) `line_col_at` rescans (`src/runtime/tools.wado:143-156`), `preceded_by_prequel_keyword` re-collecting the whole output per `{` (`src/g4/action_strip.wado:305-314`), `compute_alt_gc_starts`/`case_names_for_rule` recomputed per alt instead of per rule (`src/lower.wado:2244-2256`, `:1110-1157`), found-flag loops without `break` (`src/lexer_gen.wado:1196-1201`, `:1642-1646`, `src/g4/parser.wado:338-343`), redundant `clone_dispatch` under value semantics (`src/lower.wado:1555`, etc.), manual deep-copy loops in `prediction.wado:219-250`.

## Test coverage gaps

- [ ] `codegen_test.wado` asserts only on generated-source substrings, so dispatch-shape bugs are invisible to it; coverage relies entirely on driver/descriptor layers.
- [ ] Triage parsing covers 3 of 4 axes: no `"oracle"` arm in `lookup_for_axis` (unknown axes silently map to stage_b), and zero tests for `[stage_b_oracle_skip]`/`[stage_b_oracle_todo]` parsing. `scripts/extract_antlr4_descriptors.wado:2051-2057`
- [ ] `gen_tokenize_fn` / `build_dispatch_groups` / keyword classifier (largest lexer_gen code) have no unit tests; deep-nullable FIRST through rule refs is a `gen_context_test` blind spot.
