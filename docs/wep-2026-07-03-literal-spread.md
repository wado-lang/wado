# Literal Spread (`..base`)

## Context

Wado has no way to build a `{ … }` literal from an existing value. Rewriting one
field of a struct forces listing every other field; building a map from another
map, or composing a payload from several field sets, has the same boilerplate.
Wado's `{ … }` literal already covers three shapes — named structs, anonymous
structs (synthesized from field shape, auto-`Serialize`), and key-value
collections via `KeyValueLiteralBuilder` — so the fix should serve all three with
one rule.

## Decision

Add a `..base` spread member to `{ … }` literals, following JS spread rather than
Rust's Functional Record Update.

Model: a literal is a left-to-right sequence of members — explicit `k: v` or
`..base`. Members are applied in source order, **last write wins**. Every
subexpression is evaluated once, in source order; precedence is independent of
evaluation.

One rule governs everything — a dead-write check:

> A member is an error iff every field/key it contributes is also contributed by
> a **later** member (it is fully shadowed).

This is decidable only when field sets are statically known, which is exactly
what makes the three cases differ without any per-case special rule:

- Named struct `S { .. }` — a `..base` of type `S` is complete, so it shadows
  everything before it. The only non-dead form is `S { ..base, overrides… }`:
  spread first, single. A trailing or interior spread, a second spread, or an
  explicit field before the spread is a dead-write error.
- Anonymous `{ ..a, ..b, … }` — no target type; the result is a fresh anonymous
  struct whose fields are the union of the contributors (name collisions
  resolved last-wins, type from the last contributor). Each spread is partial
  w.r.t. that union, so any position and multiple spreads are allowed; only a
  fully-covered member (e.g. `..a` immediately followed by a same-typed `..b`)
  errors. The synthesized struct auto-derives `Serialize` when its fields do, so
  `info(msg, { ..ctx, request_id })` composes and serializes like today's
  `{ user_id, ip }`.
- Key-value `{ ..m, "k": v }` — keys are dynamic, so no member is ever provably
  shadowed; any position and multiple spreads are allowed, last-wins at runtime.
  As a bonus the same rule flags duplicate literal keys (`{ "k": 1, "k": 2 }`).

Which mode applies follows the existing anonymous-vs-named literal split: an
expected nominal type (`A { … }`, `let x: A = { … }`) constrains every `..base`
to that type; an inferred literal synthesizes the union.

Standalone `..base` (no overriding members) is an error: under value semantics it
is just a deep copy of `base`, which `base` itself already is.

Type rules: in a nominal literal, each `..base` must be the literal's type
(generic args unifiable) and participates in type-argument inference like an
explicit member. In an anonymous literal, spreads may differ and drive the union.
Field reads via a spread obey the same read-reachability as `base.f`, so `..base`
cannot pull a private field across a module boundary.

## Implementation

Named and anonymous struct literals and key-value literals share the
`StructLiteralExpr` AST node; the elaborator already dispatches between them, so
the syntax and AST change once.

- `ast.rs` / `parser.rs` — accept `..expr` members in the literal; record them on
  `StructLiteralExpr`.
- `elaborator` — apply the dead-write check; for nominal literals resolve/typecheck
  each `..base` against the literal type and fill shadowed-complement fields from
  it; for anonymous literals synthesize the union struct type from the spread
  sources' field sets; evaluate `base` once (temporary when non-trivial).
- `reify` — struct path emits a plain `StructLiteral` (fields filled from
  `base.f`); key-value path emits the existing builder block plus a new
  `insert_all(base)` per spread. `KeyValueLiteralBuilder` gains
  `fn insert_all(&mut self, base: Self::Output)`, implemented for `TreeMap`.
- `unparse.rs` / `syntax.rs` — format and highlight `..base`; regenerate the VS
  Code grammar and formatter fixtures.

Downstream (monomorphize → codegen) is unchanged: both paths lower to shapes the
compiler already produces.

Phasing: named-struct FRU and key-value merge land first; anonymous composition
(union synthesis) is additive under the same rule and can follow, without
changing the semantics of the first two.

## Consequences

- One rule (last-wins + dead-write) covers structs, anonymous composition, and
  maps; users learn `..base` once.
- JS-aligned reading (`{ ..base, changed }`), and the earlier "explicit silently
  clobbered" footgun becomes a compile error where it is decidable.
- Anonymous `{ ..a, ..b }` gives typed structural composition — a Wado-specific
  capability enabled by anonymous structs plus bound-driven derivation.
- Diverges from Rust FRU (position and precedence differ); the key-value path
  costs one library method; no IR or codegen changes.
