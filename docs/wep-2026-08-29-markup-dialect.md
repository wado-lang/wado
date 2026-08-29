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

| Position      | Shape                                                | Examples                                                |
| ------------- | ---------------------------------------------------- | ------------------------------------------------------- |
| Wado-hosted   | Wado at the top level, markup in expression position | JSX, XHP/Hack, ReScript, Scala 2 XML literals, E4X      |
| Markup-hosted | Markup at the top level, Wado in holes               | PHP, ERB, Razor, Elixir HEEx, Jinja, Go `html/template` |
| Sectioned     | One file, sections, neither nested in the other      | Vue SFC, Svelte, Astro                                  |
| Paired        | A markup-only file, its logic in a sibling module    | Angular templates, Razor Pages, Go `.tmpl`              |
| Builder       | No markup surface; a typed builder API               | Flutter, SwiftUI, lucid/blaze, kotlinx.html             |

Two lessons the record carries. Languages with a macro system never touched
their grammar — Rust (`html!`, `view!`, `rsx!`), Elixir (`~H`), OCaml (PPX),
Haskell (QuasiQuotes) all reached Wado-hosted or Markup-hosted through a macro,
not through syntax. And of the languages that did put markup in the grammar, two
removed it: E4X was deleted, and Scala 3 deprecated XML literals in favour of
interpolators. Wado has no macros, which is why the question reaches the grammar
at all — and Kiln is the answer that keeps it away from the grammar.

The record also refuses to converge on one position. Server-side rendering
settled on Markup-hosted and Paired (Razor, HEEx, `templ`); client-side UI
settled on Wado-hosted and Sectioned (JSX, Svelte). Wado has one consumer of
each shape.

## Decision

This WEP fixes the axis, the names for its positions, and the criteria. It
proposes no change to the Wado language.

### The axis

The five positions above are the axis. Builder is the control: without it the
study can only compare markup surfaces to each other, never establish that one
is needed. Every candidate is measured against the builder API, not against the
next candidate.

### Criteria

| Position      | Grammar reuse             | Diagnostics today           | HTML-authorable | Control flow          | Consumer it fits          |
| ------------- | ------------------------- | --------------------------- | --------------- | --------------------- | ------------------------- |
| Wado-hosted   | `Wado.g4` + a markup mode | layout-preserving expansion | no              | the language's own    | playground, signals       |
| Markup-hosted | needs an HTML parser      | needs a source map          | yes             | a template language's | blog, `wasi:http/service` |
| Sectioned     | needs an HTML parser      | needs a source map          | yes             | a template language's | either                    |
| Paired        | needs an HTML parser      | needs a source map          | yes             | a template language's | blog, `wasi:http/service` |
| Builder       | none                      | exact                       | no              | the language's own    | either                    |

HTML-authorable is one property under two names: whether real HTML pastes in
unedited, and whether someone who does not write Wado can own the file.

### Findings

Four facts about this repository bear on the criteria. None of them settles the
axis.

Lexer modes are proven on the real grammar. `Wado.g4` already lexes a nested
language: a backtick pushes `mode TEMPLATE`, `${` pushes `DEFAULT_MODE` back for
the interpolated expression, and a brace pair balances the return. Wado-hosted
markup needs that machinery with `<` in place of the backtick. Gale accepts
`mode` inside a combined `grammar` — a deliberate superset over ANTLR4, which
restricts modes to a `lexer grammar` — so a dialect grammar keeps the shape
`Wado.g4` already has.

No HTML parser exists here. Marl escapes HTML and never parses it, by design.
Markup-hosted, Sectioned and Paired all need one, and tolerant HTML5 parsing —
implied end tags, raw-text elements, attribute quirks — is a larger artefact
than the experiment it serves. An XML-strict subset avoids it, and discards the
one advantage those positions hold over Wado-hosted.

Diagnostics have two answers, and only one of them exists today. The compiler
reports its own type errors against the generated source; only a generator's
`emit-diagnostic` spans reach the input file. Layout preservation — emitting a
module whose lines correspond to the input's — is a generator-side discipline
needing no compiler change, but it is available only where the output is an
in-place expansion of the input, which is to say Wado-hosted. A source map is
the general answer: the generator already knows the correspondence, so Kiln
carrying it from output spans back to input spans serves every position equally.
templ, Svelte and Vue all ship one. It is Kiln infrastructure rather than
language surface, so it costs a dialect none of its independence.

Paired binding is not reachable the way its prior art practises it. Angular and
Razor Pages bind a template to a companion class by name: the template names the
class's members and the compiler resolves them. A Kiln generator receives files
by value and has no access to the compiler's types, so it could reproduce that
only by parsing the sibling `.wado` itself and redoing name resolution the
compiler already does. What stays reachable is a template file declaring its own
typed parameters, compiling to a module whose exported render function is the
whole interface — nearer a function than a pairing, and separated from
Markup-hosted only by whether the file may also declare Wado items of its own.

### Deliberate omissions

- No change to the Wado grammar is proposed, at any position on the axis. The
  `<` reservation stays reserved and unspent.
- A dialect ships as a Kiln generator in its own repository, following the
  package shape `package-gale-highlight-wado` already demonstrates: a `.g4`
  beside the source, consumed at a `use` site with
  `generator: { module: "lib:gale" }`.
- Promotion of any surface into the language is out of this WEP. It becomes
  arguable only against evidence this study produces, and the reservation is
  what keeps that door open.

### What the study must produce

Each question is answered with an artefact, not an opinion.

1. For Wado-hosted: a dialect grammar — `Wado.g4` plus a markup mode — parsing a
   realistic component with Gale's resilient CST intact. The known hazard is `>`:
   closing a tag and comparing two values want the same character, in a grammar
   with no JSX-style expression/type disambiguation to lean on.
2. For Wado-hosted: a layout-preserving generator, and the measured cost of that
   discipline — which markup forms cannot expand within their own line count. A
   source map retires this question.
3. For Markup-hosted, Sectioned and Paired: which HTML subset the parser accepts,
   and the paste-fidelity bill of that choice, measured by converting the blog's
   real HTML rather than estimated.
4. For every position: a diff of the generated output against the same component
   written by hand at Builder. This is the readability comparison that decides
   whether any surface is warranted, and Kiln makes it mechanical.
5. A compile-time budget. Gale parses a 13366-byte fixture in ~4.4 ms/iter (build
   only, dev host, guest `-O2`); a dialect file re-parses on every edit because
   its content is part of the cache key.

## Known gaps

- A source map for Kiln: a generator-supplied correspondence from output spans
  back to input spans, applied when the compiler renders a diagnostic. It is the
  general answer to the diagnostics criterion, and it puts every position on
  equal footing. Recorded as an open question in the Kiln WEP.
- Grammar import resolution in Gale. `import S;` parses, but slave grammars are
  not resolved, so a dialect must copy `Wado.g4` and drift from it. Gale's TODO
  records this as actionable on its own and notes that Kiln's multi-input
  plumbing it needs already exists. Until it closes, a dialect vendors the
  grammar and something has to keep the copy honest — `mise run check-grammar`
  holds the original to the compiler's parser, and nothing holds a copy to the
  original.
- What the markup lowers to. Every position needs an `Element` type, a component
  model, and event wiring; Builder is that target, and it does not exist. No
  surface can be studied before it.
- Whether one surface serves both consumers. The blog is Markup-hosted- and
  Paired-shaped, the playground is Wado-hosted-shaped, and the prior art
  suggests the answer is no.
- Whether Paired is a position of its own. Stripped of name binding to a
  companion type, what is left is a template module with typed parameters, which
  may be Markup-hosted under another name. The axis carries both until a dialect
  is written against each and the difference either shows up or does not.

## Consequences

- The JSX question is answered by construction rather than by argument: whatever
  the study concludes ships outside the language, so the grammar carries no risk
  while the answer is found.
- The `<` reservation keeps its value. It cost `<Type as Trait>::method`, and it
  stays available for a surface that has earned promotion.
- A dialect exercises Kiln, Gale and the language at once — the first test of
  whether Kiln's ceiling is "IDL converter" or "the mechanism a dialect needs" —
  and gives Gale a second real-world grammar beyond `Wado.g4` and its corpus.
- A negative result is a result. If the diff against Builder shows the builder
  API reads well enough, the language keeps its surface and the reservation
  stays unspent — reached without a single grammar change.

## Related WEPs

- [Kiln — Keyed IDL Lowering Notation](./wep-2026-04-12-kiln.md) — the mechanism a dialect ships as
- [Reactive Signals](./wep-2026-04-04-reactive-signals.md) — reserves `<` for JSX and assumes it in its codegen
- [Overload Resolution](./wep-2026-07-31-overload-resolution.md) — spends the reservation, ruling out `<Type as Trait>::method`
- [Gale — Grammar Adaptive LL Engine](./wep-2026-03-02-gale.md) — the parser generator a dialect grammar feeds
- [WebIDL Binding Generator (`wado-from-idl`)](./wep-2026-04-01-tide.md) — `web:dom`, the client-side target
- [Marl — Markdown Renderer and Formatter](./wep-2026-07-05-marl.md) — the server-side consumer, and its escaping helpers
- [HTTP Path Router (`core:router`)](./wep-2026-05-06-core-router.md) — the other server-side consumer
- [Tagged Template Literals](./wep-2026-01-10-tagged-template-literals.md) — the interpolation-based surface, whose hole support is still a future extension
