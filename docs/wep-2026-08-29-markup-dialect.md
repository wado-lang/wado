# WEP: Markup Dialect — Where the Top Level Lives

## Context

Wado has to emit HTML on two fronts and has a surface for neither.

- Server-side. The blog at `wado-lang.github.io` renders through
  [Marl](./wep-2026-07-05-marl.md), and a `wasi:http/service` handler dispatched
  by [`core:router`](./wep-2026-05-06-core-router.md) returns HTML. Marl already
  exports `escape_text` / `escape_attr` for HTML-templating consumers; the
  consumer does not exist.
- Client-side. [`web:dom`](./wep-2026-04-01-tide.md) is a 55-line seed — one
  interface and five resources — and
  [Reactive Signals](./wep-2026-04-04-reactive-signals.md) is designed around a
  UI surface it does not have.

The obvious import is JSX, and the ground is already prepared for it: a leading
`<` in expression position belongs to JSX by
[Overload Resolution](./wep-2026-07-31-overload-resolution.md), which is why
`<Type as Trait>::method` is permanently out and the trait-qualified form is
what Wado spells instead. Reactive Signals writes its examples in JSX and lists
"integrate with JSX codegen" as work.

None of it exists. There is no `Element` type, no component model, no event
wiring; `reactive` is a parser flag (`is_reactive` in the AST and the symbol
table) with no entry in `docs/spec.md`. JSX would be the surface of a UI
framework that has not been built.

What changed is that the surface question is now separable from the language
question. [Kiln](./wep-2026-04-12-kiln.md) admits any file as a generator input,
including a Wado dialect, and a dialect adds nothing to the language: no lexer
mode in `wado`, no parser rule, nothing for `wado format`, the LSP, or the
highlighter to learn. A markup surface can therefore be built, published, used
and abandoned outside this repository, and the language's `<` reservation stays
unspent while that happens.

So the open question is not "JSX or not". It is which shape a markup surface
should take, and the first axis to settle is where the top level lives.

### Prior art

Five positions, distinguished by the nesting direction and by how many grammars
own one file.

|   | Shape                                                | Examples                                                |
| - | ---------------------------------------------------- | ------------------------------------------------------- |
| A | Wado ⊃ markup (markup in expression position)        | JSX, XHP/Hack, ReScript, Scala 2 XML literals, E4X      |
| B | Markup ⊃ Wado (code in holes)                        | PHP, ERB, Razor, Elixir HEEx, Jinja, Go `html/template` |
| C | One file, sections, neither nested in the other      | Vue SFC, Svelte, Astro                                  |
| D | Separate files, the module boundary as the interface | Angular templates, Razor Pages, Go `.tmpl`              |
| 0 | No markup syntax; a builder API                      | Flutter, SwiftUI, lucid/blaze, kotlinx.html             |

Two lessons the record carries. Languages with a macro system never touched
their grammar — Rust (`html!`, `view!`, `rsx!`), Elixir (`~H`), OCaml (PPX),
Haskell (QuasiQuotes) all took position A or B through a macro, not through
syntax. And of the languages that did put markup in the grammar, two removed it:
E4X was deleted, and Scala 3 deprecated XML literals in favour of interpolators.
Wado has no macros, which is why the question reaches the grammar at all — and
Kiln is the answer that keeps it away from the grammar.

The record also refuses to converge on one position. Server-side rendering
settled on B and D (Razor, HEEx, `templ`); client-side UI settled on A and C
(JSX, Svelte). Wado has one consumer of each shape.

## Decision

This WEP fixes the axis, the criteria, and the order of investigation. It
proposes no change to the Wado language.

### The axis

The five positions above are the axis. Position 0 is the control: without it the
study can only compare markup surfaces to each other, never establish that one
is needed. Every candidate is measured against the builder API, not against the
next candidate.

### Criteria

| Criterion                        | A                                     | B / D                                            | C                    | 0                  |
| -------------------------------- | ------------------------------------- | ------------------------------------------------ | -------------------- | ------------------ |
| Grammar reuse                    | `Wado.g4` + one lexer mode            | needs an HTML parser                             | needs an HTML parser | nothing            |
| Diagnostics under today's Kiln   | layout-preserving output is plausible | generated lines do not correspond to input lines | partial              | exact              |
| Paste fidelity (real HTML in)    | lost                                  | kept                                             | kept                 | lost               |
| Composition and control flow     | the language's own                    | a template language grows its own                | the language's own   | the language's own |
| Non-Wado author can own the file | no                                    | yes                                              | yes                  | no                 |
| Consumer it fits                 | playground, signals                   | blog, `wasi:http/service`                        | either               | either             |

### Investigate A first

The ordering is forced by two facts about this repository, not by taste.

Lexer modes are proven on the real grammar. `Wado.g4` already lexes a nested
language: a backtick pushes `mode TEMPLATE`, `${` pushes `DEFAULT_MODE` back for
the interpolated expression, and a brace pair balances the return. A markup
literal needs exactly that machinery with `<` in place of the backtick. Gale
accepts `mode` inside a combined `grammar` — a deliberate superset over ANTLR4,
which restricts modes to a `lexer grammar` — so the dialect grammar keeps the
shape `Wado.g4` already has.

B, C and D need an HTML parser that does not exist here. Marl escapes HTML and
never parses it, by design. Tolerant HTML5 parsing — implied end tags, raw-text
elements, attribute quirks — is a larger artefact than the experiment it would
serve. Restricting the input to an XML-strict subset avoids it and simultaneously
discards the one advantage B has over A, that real HTML pastes in unedited.

Kiln's diagnostics rule decides the rest. The compiler's own type errors land on
the generated source; only a generator's `emit-diagnostic` spans reach the input
file. In A the generated module is the input file with its markup regions
replaced, so line-for-line layout preservation is a generator-side discipline
and needs no compiler change. In B and D the generated module is a new render
function whose lines have no relation to the template's, so every type error in
a hole points at generated code — which makes those positions depend on Kiln
growing an output-side span map first.

An advance in either direction reopens the order: if the span map lands, B and D
become measurable on equal terms.

### Deliberate omissions

- No change to the Wado grammar is proposed, at any position on the axis. The
  `<` reservation stays reserved and unspent.
- The dialect ships as a Kiln generator in its own repository, following the
  package shape `package-gale-highlight-wado` already demonstrates: a `.g4`
  beside the source, consumed at a `use` site with
  `generator: { module: "lib:gale" }`.
- Promotion of any surface into the language is out of this WEP. It becomes
  arguable only against evidence this study produces, and the reservation is
  what keeps that door open.

### What the study must produce

Each question is answered with an artefact, not an opinion.

1. A dialect grammar — `Wado.g4` plus a markup mode — that parses a realistic
   component with Gale's resilient CST intact. The known hazard is `>`: closing a
   tag and comparing two values want the same character, in a grammar that has no
   JSX-style expression/type disambiguation to lean on.
2. A layout-preserving generator, and the measured cost of that discipline: which
   markup forms cannot expand within their own line count.
3. The paste-fidelity bill, measured by converting the blog's real HTML rather
   than estimated.
4. A diff of the dialect's generated output against the same component written
   by hand at position 0. This is the readability comparison that decides whether
   any surface is warranted, and Kiln makes it mechanical.
5. A compile-time budget. Gale parses a 13366-byte fixture in ~4.4 ms/iter
   (build only, dev host, guest `-O2`); a dialect file re-parses on every edit
   because its content is part of the cache key.

## Known gaps

- Grammar import resolution in Gale. `import S;` parses, but slave grammars are
  not resolved, so a dialect must copy `Wado.g4` and drift from it. Gale's TODO
  records this as actionable on its own and notes that Kiln's multi-input
  plumbing it needs already exists. Until it closes, a dialect vendors the
  grammar and something has to keep the copy honest — `mise run check-grammar`
  holds the original to the compiler's parser, and nothing holds a copy to the
  original.
- An output-side span map for Kiln, which is what would let B and D be judged on
  the same footing as A. Recorded as an open question in the Kiln WEP.
- What the markup lowers to. Every position needs an `Element` type, a component
  model, and event wiring; position 0 is that target, and it does not exist. The
  study cannot start from the surface.
- Which consumer goes first. The blog is B/D-shaped and the playground is
  A-shaped, so investigating A first means the client-side consumer leads. If the
  blog is to lead instead, the span-map gap moves onto the critical path.
- Whether one surface serves both consumers at all. The prior art suggests not.

## Consequences

- The JSX question is answered by construction rather than by argument: whatever
  the study concludes, it ships outside the language, so the grammar carries no
  risk while the answer is found.
- The `<` reservation keeps its value. It cost `<Type as Trait>::method`, and it
  stays available for a surface that has earned promotion.
- Gale gains a second real-world grammar beyond `Wado.g4` and the corpus, and a
  dialect exercises Kiln, Gale, and the language together — the first test of
  whether Kiln's ceiling is "IDL converter" or "the mechanism a dialect needs".
- A negative result is a result. If the diff against position 0 shows the builder
  API reads well enough, the language keeps its surface and the reservation stays
  unspent — and that conclusion is reached without a single grammar change.

## Related WEPs

- [Kiln — Keyed IDL Lowering Notation](./wep-2026-04-12-kiln.md) — the mechanism a dialect ships as
- [Reactive Signals](./wep-2026-04-04-reactive-signals.md) — reserves `<` for JSX and assumes it in its codegen
- [Overload Resolution](./wep-2026-07-31-overload-resolution.md) — spends the reservation, ruling out `<Type as Trait>::method`
- [Gale — Grammar Adaptive LL Engine](./wep-2026-03-02-gale.md) — the parser generator a dialect grammar feeds
- [WebIDL Binding Generator (`wado-from-idl`)](./wep-2026-04-01-tide.md) — `web:dom`, the client-side target
- [Marl — Markdown Renderer and Formatter](./wep-2026-07-05-marl.md) — the server-side consumer, and its escaping helpers
- [HTTP Path Router (`core:router`)](./wep-2026-05-06-core-router.md) — the other server-side consumer
- [Tagged Template Literals](./wep-2026-01-10-tagged-template-literals.md) — the interpolation-based surface, whose hole support is still a future extension
