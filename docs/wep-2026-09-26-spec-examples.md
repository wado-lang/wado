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

The reference is an HTML comment holding a JSON object, the block directly
before the fence in the same container. The formatter puts a blank line between
the two, and GitHub does not render the comment, so the reader sees an ordinary
code block:

````markdown
<!-- {"fixture": "cast_fn_type_newtype.wado"} -->

```wado
type Meters = f64;
let double = (|x| x + x) as fn(Meters) -> Meters;
assert double(1.5 as Meters) == 3.0 as Meters;
```
````

The comment is data, so JSON says what it may hold, and no syntax of its own
needs a rule. A comment whose body does not open with `{` is no reference, which
leaves room for a formatter directive. One that does is a reference: `fixture`
is its only key, and a body that fails to parse or holds another key is an
error, so a misspelled key is caught rather than read as no reference.

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

The checker is `package-gale-highlight-wado/tools/spec_examples.wado`, beside
the `Wado.g4` grammar it lexes with, so it counts an `assert` keyword only where
the lexer finds one, not in a comment or a string. It reads the Markdown with
Marl. `scripts/check-spec-examples.sh` drives it with `wado run`, as
`scripts/check-rust-paths.sh` drives `package-gale/tools/rust_inline_paths.wado`.

A block that names no fixture is counted against the baseline. A block with a
reference is migrated, so any rule it breaks fails the check outright. The
`#[TODO]` rule reads the fixture as a whole: one marked test anywhere in it
asks for a WEP.

Marl's `fenced_code_blocks` gives the checker what it reads of each fenced
block: its info string, its text, its source line, and the HTML block directly
before it. Marl's document tree stays `internal`.

### Rollout

The baseline, `scripts/spec-examples.json`, counts the blocks of each file that
have no reference yet, and CI fails when a count grows. It only shrinks, as
`scripts/rust-inline-paths.json` does.

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
2. [x] Write the checker, TDD against small Markdown and fixture cases: the
       reference, the indented substring match, the `assert` rule, the `#[TODO]`
       rule and the baseline.
3. [x] Add `mise run check-spec-examples` and its CI job, and record the baseline
       of all 353 blocks.
4. [x] Migrate one file first, `spec-types.md`, and decide the `assert` rule from
       it: keep it, or drop it and say why here. Kept. Its 42 blocks became 55
       quotations and one `` ```text ``, since a rejected example is now a
       block of its own; 8 quote a `compile_error` fixture. In most of the rest
       the assert replaced a comment that stated a value. A block made
       only of declarations quotes the `test` that uses them, which closes the
       first known gap for that block. The assert was artificial in three: a
       type-level claim of inference, a field visibility, and a `#[cm]` name.
       The migration found one block wrong in the specification: a float
       vector's comparison mask is the integer vector of the same width, not
       the float one.
5. [ ] Migrate the other twelve spec files, one change per file.
6. [ ] Delete the baseline once it is empty, so the rule holds with no
       exceptions.

## Known gaps

- The match proves the quoted text compiles in its fixture, not that the
  fixture exercises it. A block quoted from a function no test calls passes.
- The cheatsheet repeats the specification's examples in a shorter form, and
  nothing holds it to this rule.
