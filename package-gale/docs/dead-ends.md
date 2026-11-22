# Gale Dead Ends

Approaches that were tried and failed. Read the section for an area before
retrying anything in it. Performance dead ends live in
[`perf.md`](./perf.md#tried-and-didnt-pan-out).

## Prediction

The static path always has edges, a decidability limit. The complete answer is
the runtime ATN simulator (see
[`antlr4-compatibility.md`](./antlr4-compatibility.md)).

- RuleRef expansion via a return stack (2026-03): expanding multi-token RuleRefs during SLL prediction to cut backtracking. Tokens from inside an expanded sub-rule can't be used at the decision point without an ATN-grade depth mapping, and dedup-by-alt merges alts that share a sub-rule. Left as zero-overhead scaffolding.
- LL(\*) static variant emit (2026-05), three over-broad attempts at per-(rule, follow-mask) variants. Static analysis can't distinguish "tail-greedy that should yield to the caller" from "one that legitimately re-enters" — each over-broad guard silently broke a real grammar (`htmlContent`, CSS `selector`). Superseded by the runtime FOLLOW gate; pair any LL repair with a rejection-case fixture, not just a hit-case one.
- A caller-FOLLOW tier on the scan tournament (2026-09): among the alternatives that scan, prefer the longest whose end the caller's continuation can start at, and fall back to plain longest when none clears it. It was aimed at `if any_error { None }`, where `any_error { None }` is the longer expression and only the block the `if` still owes separates them. That is Rust's condition-excludes-struct-literal rule, which neither this grammar nor ANTLR4 encodes. The tier gets that class right and is wrong in general: an alternative's scan end is not the rule's end, so on an LR atom it reads a position the precedence loop has not finished with. `driver_cst_sqlite_oracle_test` catches it on `SELECT CASE WHEN a THEN CASE WHEN b THEN 1 END ELSE 2 END`, where it hands the dangling `ELSE` to the inner `CASE` and the jar gives it to the outer. The answer is full-context simulation rather than a tie-break: ANTLR4 simulates the continuation through the whole rule, which is the ATN.
- A continuation scan on the tournament (2026-10): scan each contested alternative followed by what can follow the rule, and take the lowest alternative that still scans. What follows was a union over every call site, since a scan has no call stack, and that union admits continuations the actual caller does not. On TypeScript it read `Map<keyof E, V>` as a comparison. It also cannot evaluate a predicate inside a called rule, so `export interface` became an `expressionStatement`. The simulator walks the parse's own stack and answers both.
- A post-iteration check on the loop FOLLOW gate (2026-10): a greedy `*` yields where the caller's continuation matches only if it no longer matches after one scanned iteration. It keeps `impl Fn() -> a::R + Send`'s inner `:: R`, as the jar does, but both readings stop at a callee's first token, so a match after the iteration says as little as the one before it: Rust closures `|_| String::new(),` took `| String::new()` as an or-pattern because `, label` also matched. Viability lies past the depth the mask holds, which is the simulator's job.

## Semantic Predicates

A gate member is parser state, and the parser's state is not part of what a
rollback restores.

- A scope stack in a gate member (2026-09), for Rust's "no struct literal in
  a condition". The rule is about the innermost enclosing scope, which a member
  alone cannot name: a flag cleared on the way into a bracket never comes back,
  and a counter cannot tell a function body from a head inside one, because a
  block steps it too. One bit per scope in a single int says it exactly — a head
  pushes `* 2 + 1`, a bracket `* 2`, each pops `/ 2`, and the ban is
  `noStruct % 2`. It parses every hand-written probe and is far worse on real
  files. Error recovery and the repeat-exit probe roll back the position but not
  the parser's members, so a push whose pop never runs shifts the stack for the
  rest of the file. Restoring members across recovery and speculation is the
  precondition for any stack-shaped gate. A flag survives that rollback because
  it is idempotent, and the rule-level scope (`locals` + `@init` / `@after`)
  gives it the nesting a bracket would otherwise take away.

## Lexer

- Choosing statically between the arms of a greedy lexer loop (2026-09), for `STR : '"' (~["] | ESC)* '"'`. Every static policy loses a case: first-match ends the token at an escaped quote, maximal munch takes the longest arm and strands the suffix in `('a' | 'ab')* 'b'`, and scoring the arms against the suffix needs a forward scan that repeats the same choice. The class is regular, so the lexer ATN runs the arms together in one pass and decides it (`scan_undecidable_arms`). Pair any static retry with `lexer_loop_arm_longest.g4`, which holds both the hit case and the rejection case.
