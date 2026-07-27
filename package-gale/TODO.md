# Gale TODO

Open work towards full ANTLR4 compatibility and the performance budget it implies. Read this together with:

- [`antlr4-compatibility.md`](./antlr4-compatibility.md) — the compatibility contract, prediction / codegen design, soundness invariants, descriptor pipeline, and triage workflow.
- [`AGENTS.md`](./AGENTS.md) — dev-cycle essentials and the prediction failed approaches.
- [`perf.md`](./perf.md) — runtime performance: benchmark state, live profile, what would move the needle, and measured perf dead-ends.

This file lists what is **not yet done** at a behavioral level; find the code via search, not line pointers. Closed work belongs in commit history.

## Diagnostics & introspection ([#1246](https://github.com/wado-lang/wado/issues/1246))

Grammar-authoring DX follow-ups:

- **Optional-scan-guard-fallback warning.** Lowering warns on an overlap tournament today; the obvious next warning fires when an `e?` resolves to a scan-guarded optional (live case: a rule in `example/Wado.g4`). Deferred because it needs the enclosing rule name available at the warn site; add the diagnostic kind, the warn, and a fixture together.
- **Structured diagnostic-to-rule identity.** A diagnostic's owning rule is carried today as the free-form human label the warn was raised with, and tooling re-associates it by substring-matching the quoted rule name — so group-scoped warnings (no quoted rule name) never inline under their owning rule. Carry a structured owner (rule name / index), set it at the warn site, and compare by equality, keeping the label display-only. The same change lets the overlap-dispatch builder be told explicitly whether it is on the scan pass instead of recovering that from a label suffix.

## LL prediction — remaining gaps

### Iter-body K-prefix for `Repeat` inner rule references

The K-prefix follow-mask path closes the multi-token tail-greedy gap at the outer alternative position, but a rule reference inside a `Repeat` body still falls back to the 1-token mask path. The fixed-point "next iteration | exit-to-caller" computation that would let it gate is straightforward but not yet plumbed. Few real grammars need it; revisit when a descriptor surfaces a regression.

### Multi-alt rule-reference expansion in the caller-side mask analysis

The K-prefix caller-side mask analysis halts at a multi-alternative rule reference because a per-depth union of the alternatives' prefixes would over-yield by matching cross-alternative sequences no real alternative admits. A per-alternative sequence representation could extend the walk safely — useful when a caller's continuation passes through a multi-alternative rule like `expr : literal | name`.

## Stage B′ — JVM-oracle integration

The Stage B′ pipeline covers `FullContextParsing`, `LeftRecursion`, `ParserErrors`, `ParserExec`, `SemPredEvalParser`, and `Sets`, with the remaining prediction divergences pinned as oracle-todo. The infrastructure (design in [`antlr4-compatibility.md`](./antlr4-compatibility.md)) is in place; Java is needed only at extract time, not in CI.

Remaining:

- **Extend coverage to the remaining parser categories** (`ParseTrees`, `Listeners`) and re-triage the oracle-skip / oracle-todo entries after each re-extract. Several oracle-skip entries were recorded because StringTemplate directives sat outside action bodies where the stripper cannot reach; extract-time action-template expansion (see `antlr4-compatibility.md`) now turns those into plain Java type slots, so re-triage them at the next JDK-equipped re-extract — some should graduate from skip.

## Composite (slave-grammar) descriptors

Every `CompositeLexers` / `CompositeParsers` descriptor short-circuits on the presence of imported slave grammars. Independent blockers:

- **Importer multi-input plumbing.** A grammar import (`import S;`) must resolve against the sibling slave-grammar files. Kiln already supports multi-input; lift the short-circuit once resolution lands.
- **Host-side output (Stage C).** Every composite descriptor's expected output is a host-side artefact — action prints, token dumps, or empty — so none survive the Stage B output normalizer. Re-evaluate once Stage C lands.

## Stage C — action / predicate execution

Design in [`action.md`](./action.md). Remaining:

- Lexer actions under a `Repeat`. The action replay places each action at the cursor it was written at, covering mid-element and nested-group placement, but a `Repeat` matches an unknown number of times and the non-greedy / lookahead-aware emitters restructure the sequence around it. An alt carrying one keeps the flat emit: top-level actions run at the end of the match, anything nested inside warns.
- The ATN-class lexer path.
- The rest of the lexer `$`-attribute surface — `$type` and member methods reading match position / text. The char-position half is covered: java2wado resolves `getCharPositionInLine()` and `_tokenStartCharPositionInLine`, but only in a Java body; the identity translator still has no `$`-form for either.
- `@lexer::members` for a `language = Java` grammar. A Java member method takes `&mut self`, but a lexer predicate runs inside `try_<rule>(lx: &Lexer, ...)` — the tournament must not mutate through a losing candidate. Java lexer bodies therefore see no members, and a reference is reported. Wiring them needs a split between members a predicate may read and members only an action may touch.
- Two same-named rule labels bound to _different_ rules in different alternatives (`x=a | x=b`). Per-alternative resolution disambiguates token-vs-rule, which is all the binding records, so a `.field` read still resolves against the first-declared rule's value channel. `$<label>.text` is unaffected — it reads the call's own span.
- The SuperClass effect interface for the real-world grammars below. Landed for **predicate-only** lexer bases, including `language = Java`: RustLexer tokenizes and parses end to end through a hand-written `impl RustLexerBase`. See `action.md` ("SuperClass — an effect interface"). Remaining before TypeScript / ANTLRv4 run: action ops (`{this.m();}` — the winner-replay path), the parser side (parser-rule superClass predicates like `{this.NextGT()}?`, currently discarded), and lifecycle hooks (`nextToken` for last-token tracking). Action-op bases stay carved out (byte-identical) until the replay path lands.
- Make the ANTLR descriptor output corpus codegen-and-compare (parse-only today), unblocking the output acceptance.
- Extend the Stage C output-compare beyond `Sets` and `SemPredEvalParser`, the categories it runs for today. The remaining ones are a mechanical extractor re-run plus triage of whatever lands in `[stage_c_todo]` / `[stage_c_skip]`.
- Parser actions on the paths that still warn: a non-transparent group's alternatives (the transparent path inlines its actions with its elements), an LR suffix, and a multi-alt prequel. Each surfaces `UnsupportedAction`, so a grammar that needs one is never silently wrong.
- The recognizer accessors ANTLR exposes to an action that Gale does not model: `getExpectedTokens()` and `getVocabulary()` (live case: the `ParserErrors/LL1ErrorInfo` descriptor prints the expected set), and `PredictionMode` / `dumpDFA`, which describe ANTLR's simulator rather than the grammar.
- java2wado numeric promotion: an `i32` token member (`$X.int` / `.type` / `.line` / `.pos` / `.index`) mixed with a wider value-channel field (`returns [long v]` / `[float]` / `[double]`) mismatches Wado's strict widths, since Wado has no implicit widening. Loud compile error, not silent; no corpus grammar hits it. A proper fix threads Java's promotion rules through the translator.

Gale still silently discards action / predicate contents for the real-world grammars whose constructs the parser subset does not yet cover (`ANTLRv4Lexer`, `RustLexer`, `RustParser`, `TypeScriptLexer`, `TypeScriptParser`): they load cleanly, but the generated recognizer behaves as if every predicate were `true` and every action a no-op. That is wrong for:

- Rust's `>>` / `>>=` token splitting in generics (`{this.NextGT()}?`) and float-literal disambiguation (`{this.FloatLiteralPossible()}?`); without them Gale mis-parses nested generics. (Raw-string `#`-count matching is _not_ a Stage C case — it is a recursive fragment, an ATN-class lexer concern.)
- TypeScript's regex-vs-division disambiguation and other context-sensitive lexer and parser rules.

All of these call `this.<method>()` against a hand-written `superClass` base that lives outside the `.g4` — executing them needs the SuperClass mechanism, not just action translation.

Stage C is a hard prerequisite for treating Gale as a drop-in ANTLR4 replacement, for any lexer-level optimization (a fast tokenizer is meaningless if it tokenizes incorrectly), and for `superClass` / `tokenVocab`. It also unblocks composite-descriptor output comparison and parser descriptors whose output is purely action-print stdout.

## gale-highlight — theme vocabulary

`gale-highlight` provides a grammar-agnostic `Theme` (capture-class → CSS color), `stylesheet`, and `default_theme`. The _color → class_ half is covered; the _rule → class_ half is not: the set of capture classes is grammar-defined (the `.scm` highlight query), so a theme author keys colors by hand-typed class names with nothing to validate against, and a class the grammar emits but the theme omits is silently unstyled.

- **Expose a grammar's capture vocabulary from the generator.** Gale already parses the `.scm` at generation time; emit the capture names it uses as a generated artefact (e.g. a `pub const CAPTURES: List<String>`, or a class → default-color map) alongside the parser. A grammar package could then build or validate a `Theme` against the real class set, turning a mistyped or dropped class into a signal instead of silent unstyled output.

## Performance

Runtime performance — the benchmark state, the live profile, the directions that would move the needle, and measured dead-ends (e.g. data-driven scan) — lives in [`perf.md`](./perf.md).

## Code-health bugs

Add a failing test before fixing.

### Soundness and compatibility divergence

The highest-risk bugs: a static-prediction edge or a parse/scan asymmetry that can mis-parse valid input. Several need their own focused PR with full-corpus validation rather than a quick patch (the prediction design notes the static path always has edges).

Entries state the symptom, how to reproduce it, and anything already measured — not a diagnosis or a proposed fix. A diagnosis written here reads as an instruction later, and two have been wrong: one would have broken compatibility if implemented as written, the other described a difference that did not exist.

- [ ] Valid input is rejected — the emitted dispatch has no else-fallback: `a : X+ Y | X Z` fails on `X X Y`. The opaque-rule expansion path separately drops at-end alternatives its template keeps.
- [ ] `a NOT BETWEEN 1 AND 2 AND b` brackets as `(a BETWEEN 1 AND 2) AND b` — a wrong tree with no diagnostic, where ANTLR4 routes the mid-alternative operand through `expr[0]`. Fixtures `lr_mid_operand.g4` and `lr_between.g4`, both `#[TODO]`, plus the `sqlite` oracle case. Measured: routing it through `expr[0]` makes the rule ATN-class and the simulator then runs on every operator of a hot expression rule, parse-side and once per tournament re-scan — [`perf.md`](./perf.md) has the numbers.
- [ ] Two operator alternatives opening on the same token (`expr NOT? IN …` and `expr NOT? BETWEEN …` on `NOT`) are decided by alternative order rather than lookahead: `atn_lr_loop_decision` takes the first precedence-allowed enter edge whose suffix admits the token. No committed grammar is ATN-class at its LR loop, so nothing hits it today — but SQLite acquires the shape the moment the item above lands (`SELECT a NOT BETWEEN 1 AND 2 AND b` was decided as `IN` on the shared `NOT`), so the two must land together.
- [ ] `column_def`'s `type_name?` is not entered on `a UNSIGNED BIG INT` (rejected outright) or on `a CONSTRAINT c NOT NULL DEFAULT 1` (`CONSTRAINT c` lands under `column_constraint` where ANTLR4 reads it as the type name) — two `sqlite` oracle cases, `#[TODO]`. `_parse_type_name` is called with `&EMPTY_FOLLOW`, so its `name+?` probe reads "the caller cannot continue" everywhere and always takes one name; the entry probe then measures viability against that one name.

  Measured, in two steps. Asking the probe whether the token appears at any depth of the mask (rather than `follow_yields`'s lockstep read) and giving the `e?` body a mask of its own resolves both cases and leaves `a VARCHAR(10), b DECIMAL(10, 2)` broken — `column_def` inside `(',' column_def)*` gets a mask with no `,` in it. Also handing a repeat body's tail the enclosing continuation repairs that, and breaks `htmlContent` nested tags, CSS `selector`'s child combinator and `FullContextParsing/ExprAmbiguity_2`: the widened mask is what every `follow_yields` gate reads, which is the over-broad guard `AGENTS.md` records. So the probe cannot borrow the yield mask — it needs its own channel.
- [ ] A lexer alternation that is _not_ in tail position takes the first matching arm, not the one that lets the whole rule match: `I : ('a' | 'ab') 'bc'` rejects `abc`. Tail-position alternations are maximal-munch and unaffected.
- [ ] A label on a transparent group (`x=(ID)`) silently drops the binding.
- [ ] `\P{...}` (negated Unicode property) is parsed as literal chars — only lowercase `\p` is detected — and an unknown `\p{...}` property expands to an empty set with no diagnostic.
- [ ] A wildcard alternative gets an empty-token branch in a direct dispatch at the lowered-IR level, and `w=.` escapes the wildcard machinery entirely (the check does not unwrap labels). The surface-IR paths apply the wildcard soundness invariant; these do not.
- [ ] Overlapping-but-unequal first-char ranges in the lexer dispatch shadow later rules: a char in the intersection only tries the first range group, and the wildcard fallback is unreachable for it.
- [ ] A surrogate code point in a char range (legal in ANTLR4 for matching UTF-16 code units) collapses to a single replacement char — range endpoints are Unicode scalars.

### Pipeline and tooling correctness

- [ ] A rule-argument action whose host type contains `[]` (`r[int[] arr]`) ends early and its remainder leaks into the grammar text: the action stripper ends a `[...]` at the first unescaped `]`, which is right for the char sets the corpus does exercise. No corpus grammar hits this.

### Deleted-terminal rendering

`to_string_tree` matches ANTLR4's `Trees.toStringTree` everywhere except a deleted terminal: Gale prints `<skip z>` where ANTLR4 prints the bare token (`<missing X>` already matches). The marker is a deliberate extension — it is what makes Gale's own error-recovery fixtures able to tell recovery from a clean parse — so `ParseTrees/ExtraToken` sits in `[stage_b_skip]` rather than being forced to pass. Decide once: either the corpus gets an ANTLR-identical rendering mode, or the marker becomes opt-in and the recovery fixtures read the `Skip` rows structurally.

### Diagnostics and minor

- [ ] The error-fallback path puts internal constant names in user-facing "expected" lists while the normal expect path uses the token vocabulary — two error paths, two vocabularies.
- [ ] Error-token text is a message, so diagnostics read `unexpected token "unterminated string"`.
- [ ] A parse error's `expected` set is populated everywhere but rendered by nothing (the Display impl omits it).
- [ ] An empty lookahead signature is guarded on the scan side but not the parse side, where the lookahead condition would emit syntactically broken code; either the guard is dead or the parse side is missing it.
- [ ] A list-label leaf path double-bumps the inner name counter, and the group case lacks the collision rebind the leaf case has — both in the label-dedup bug class a fixture already exists for. The non-greedy transparent first iteration also dedups outer-scope bindings against a fresh counter table.

### Unchecked-argument quality nits (non-crash)

- [ ] Malformed lexer command _arguments_ are unchecked (the paren panics are fixed): `pushMode(42)` interns a mode literally named `42`, and `-> ;` yields the odd "unknown lexer command ;".
