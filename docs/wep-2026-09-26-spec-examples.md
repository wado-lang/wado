# WEP: Spec Examples Quote Fixtures

## Context

The specification is normative: it says how the language should behave. Nothing
holds it to that. Its 353 `wado` code blocks are never compiled or run, so an
example can be wrong from the day it is written, or turn wrong when the language
moves, and no check notices.

Measured when this WEP was written, `wado check` over each block as written, and
again wrapped in a function body:

| Outcome            | Blocks | Main causes                                                           |
| ------------------ | ------ | --------------------------------------------------------------------- |
| Compiles           | 124    |                                                                       |
| Parse error        | 101    | items and statements mixed at the top level; `...` placeholders (~60) |
| Name or type error | 128    | fragments naming what they never declare; examples meant to be errors |

The e2e fixtures, by contrast, are checked on every run, and `AGENTS.md` already
names them as where the language's behaviour is stated. Two blocks out of 353
appear in any fixture, and only once indented.

### How other ecosystems hold a specification

- CommonMark extracts the examples from its specification and runs them as its
  test suite.
- The Rust Reference gives each rule an identifier, and rustc's tests name the
  rules they cover, so a tool reports the rules no test reaches.
- ECMAScript's test262 tags each test with the section it tests, and a proposal
  reaches Stage 4 only with tests and two implementations.
- WebAssembly writes its specification in a DSL that generates the prose, the
  formal rules and the proof-assistant definitions, beside a reference
  interpreter and a shared test suite.

The first makes the examples true. The second ties every rule to a test.

## Decision

A code block in the specification is a quotation of an e2e fixture.

### The rule

1. Every `` ```wado `` block in `docs/spec-*.md` names a fixture.
2. The named fixture contains the block verbatim: its lines, in order and
   contiguous, each shifted by one indentation prefix shared by all of them. A
   blank line matches a blank line.
3. Any number of blocks may name one fixture.
4. A block whose fixture does not expect a compile error contains at least one
   `assert`. This rule is provisional: the first migrated file decides whether
   it holds or is absurd, and it is dropped if absurd.

The reference is an HTML comment, the block directly before the fence in the
same container. The formatter puts a blank line between the two, and GitHub does
not render the comment, so the reader sees an ordinary code block:

````markdown
<!-- fixture: cast_fn_type_newtype.wado -->

```wado
type Meters = f64;
let double = (|x| x + x) as fn(Meters) -> Meters;
assert double(1.5 as Meters) == 3.0 as Meters;
```
````

The path is relative to `wado-compiler/tests/fixtures/` and may name any `.wado`
file there, a helper module a fixture imports included. A two-file example
quotes each file.

### Why a quotation, and not a generated test

A fixture already carries what an example needs: the declarations around it,
the expectation in `__DATA__`, and a run at every optimization level. Quoting it
needs no generator, no wrapping rule for fragments, no hidden setup lines and no
second expectation syntax. The check is a file lookup and a substring match.

The link is also checked from both sides. Editing the example without the
fixture breaks the match, and so does renaming, deleting or rewriting the
fixture. A copy with no link drifts silently in either direction.

The reference is the traceability a rule identifier would give: a rule's
example names the test that holds it.

### What follows from the rule

- A `` ```wado `` block is always real Wado. A shape sketch with `...`
  placeholders or a grammar outline is `` ```text ``.
- The indentation allowance is what lets a fragment stand in the specification:
  `let p = …; assert …;` sits inside a `test { }` in its fixture, four spaces
  in.
- A claim an example makes about a value is written as an `assert` in the block,
  not as a comment. The claim is then part of the quoted text and runs. A
  comment such as `// 3.14` is checked by nothing, which is what the assert rule
  targets.
- A compile-error example quotes a fixture declaring `compile_error` or
  `compile_error_codes`. One fixture expects one report, so each rejected
  example has a fixture of its own.
- A block quoting a fixture marked `#[TODO]` or `#![TODO]` is an example the
  compiler does not yet honour. That is a known gap, and the specification
  records no gaps, so the checker requires a WEP to name that fixture.
- A fixture is excluded from the formatter (`[format] exclude`), so its layout
  stays as the specification quotes it.

### The checker

The checker is written in Wado. It reads Markdown with Marl and lexes Wado with
the `Wado.g4` grammar, so it counts an `assert` keyword only where the lexer
finds one, not in a comment or a string. It follows
`package-gale/tools/rust_inline_paths.wado`: a script under `scripts/` drives
`wado run`, and a baseline file records the blocks that do not comply yet.

Marl's `fenced_code_blocks` gives the checker what it reads of each fenced
block: its info string, its text, its source line, and the HTML block directly
before it. Marl's document tree stays `internal`.

### Rollout

The baseline lists every block that has no reference yet, by file and heading,
and CI fails when it grows. It only shrinks, as `scripts/rust-inline-paths.json`
does.

Migrating a block is triage. Each one is exactly one of:

1. correct, and quoted from an existing or new fixture;
2. a sketch, and turned into `` ```text ``;
3. meant to be rejected, and quoted from a `compile_error` fixture;
4. wrong in the specification, and the specification is fixed;
5. right, but the compiler disagrees: a `#[TODO]` fixture and a WEP known gap.

The last two are what this WEP is for. The migration will find them.

## Roadmap

1. [x] Extend Marl with a public read of the fenced code blocks: info string,
       text, source line, and the HTML block directly preceding each. An HTML
       comment now ends at `-->`, not at the next blank line, as CommonMark
       says.
2. [ ] Write the checker, TDD against small Markdown and fixture cases: the
       reference, the indented substring match, the `assert` rule, the `#[TODO]`
       rule and the baseline.
3. [ ] Add `mise run check-spec-examples` and its CI job, and record the baseline
       of all 353 blocks.
4. [ ] Migrate one file first, `spec-types.md`, and decide the `assert` rule from
       it: keep it, or drop it and say why here.
5. [ ] Migrate the other twelve spec files, one change per file.
6. [ ] Delete the baseline once it is empty, so the rule holds with no
       exceptions.

## Known gaps

- The match proves the quoted text compiles in its fixture, not that the
  fixture exercises it. A block quoted from a function no test calls passes.
- The cheatsheet repeats the specification's examples in a shorter form, and
  nothing holds it to this rule.
