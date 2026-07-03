# Struct Functional-Update Syntax (`..base`)

## Status

- [ ] Proposed — design only (issue #1216).

## Context

Wado has no equivalent of Rust's `S { foo: new_foo, ..base }`. To rewrite one
field of a struct value, callers must list every other field explicitly:

```wado
Op::RuleCall(RuleCallOp {
    target_name: rc.target_name,
    target_snake: rc.target_snake,
    variant_id: rc.variant_id,
    min_prec_arg: Option::Some(prec),  // the only change
    field: rc.field,
    dedup_base: rc.dedup_base,
})
```

This boilerplate appears wherever a transformation visitor rewrites one field of
an op tree by value (`lower.wado`'s self-ref walkers, `rebind_op_field`,
`rebind_group_shape`, …). Every walker enumerates every field of the structs it
rebuilds, so adding a cached field to `RepeatOp` forces a lockstep edit to every
walker or the build breaks. Functional update collapses each rebuild to:

```wado
Op::RuleCall(RuleCallOp {
    min_prec_arg: Option::Some(prec),
    ..rc
})
```

## Decision

Add functional-update (struct-spread) syntax to struct literals: a trailing
`..base` supplies every field the literal does not list explicitly, taken from
the struct value `base`.

### Syntax

`..base` appears as the final element of a struct-literal field list, after all
explicit fields:

```wado
S { field_a: expr, field_b: expr, ..base }
```

Rules (mirroring Rust):

- At most one `..base` per literal.
- It must be last. No trailing comma may follow it (`{ ..base, }` is a parse
  error).
- `base` is an arbitrary expression whose type is the same struct as the literal
  (see [Type rules](#type-rules)).
- `..` is the existing single `DotDot` token; `S { ...base }` is the same
  "did you mean `..`?" parse error as elsewhere.

It works with both named and implicit (anonymous) struct literals — an implicit
literal takes its struct type from the expected type context exactly as today,
and `base` must match it:

```wado
let updated: Config = { port: 3000, ..base };
```

### Semantics — field projection

`..base` is sugar for reading each **omitted** declared field from `base`. It
desugars to a field-by-field projection, with `base` evaluated exactly once:

```wado
// given  struct S { a: A, b: B, c: C }
S { a: expr_a, ..base }
```

desugars to

```wado
{
    let __base = base;                 // evaluated once
    S { a: expr_a, b: __base.b, c: __base.c }
}
```

Key properties:

- **Only omitted fields are read from `base`.** An explicitly listed field
  (`a`) is never read from `base`; `base.a` is not evaluated or copied. This is
  the efficiency win over a copy-then-mutate model: the field being replaced is
  the one field of `base` that is never touched.
- **Value semantics are unchanged.** Each `__base.f` is an ordinary field read,
  copying that field's value into the new struct, exactly as writing `f:
  __base.f` by hand would. No aliasing, no partial move — `base` remains a fully
  valid value afterwards (unlike Rust, where `..base` can move out of it).
- **`base` is evaluated once.** When `base` is a trivial place (a local), it is
  read directly; otherwise reify binds it to a `__base_N` temporary and projects
  from that, reusing the same mechanism `reify_tuple_literal` already uses for
  `[..expr]` value spread. Evaluation of `base` is ordered after the explicit
  field expressions, matching the source order (`..base` is written last).

### Type rules

- `base` must have the **same struct type** as the literal. For a non-generic
  struct that is nominal identity (same struct, same defining module). For a
  generic struct it is the same type constructor with unifiable type arguments.
- `base` participates in **type-argument inference** the same way a field value
  does: `Wrapper { ..w }` with `w: Wrapper<i32>` infers `Wrapper<i32>`. Explicit
  fields and `base` must agree on the type arguments; a conflict is the usual
  type error.
- A `base` whose type is not the target struct is a type error
  (`..base` requires a value of struct `S`, found `T`).

### Interaction with field defaults

When `..base` is present, **every omitted field is taken from `base`**; field
defaults (`port: i32 = 8080`) are not consulted. `base` already holds a complete,
valid value for every field, so defaults are redundant, and letting `base` win
keeps the rule simple: *listed → explicit; unlisted → from `base`*. Consequently
a literal with `..base` can never be "missing a required field" — the
missing-field diagnostic is suppressed for all fields `base` covers.

Extra/unknown-field and duplicate-field checks on the explicitly listed fields
are unchanged.

### Interaction with visibility

Projecting `__base.f` is a **read** of field `f`, so it is subject to the same
read-reachability as any field access:

- Within the defining module every field is readable, so intra-module `..base`
  (the issue's entire use case) always works.
- Across a module boundary, `..base` may only project fields that are
  reachable-for-read (`pub`, or `internal` within the same package). If the
  literal omits a non-reachable field, that is a compile error — the same error
  as writing `base.secret` there.

This preserves encapsulation: `..base` cannot smuggle a private field across a
boundary. It is deliberately stricter than default-omission (a private field
with a default may be *omitted* cross-module because its default runs in the
defining module and no read crosses the boundary); with `..base` a real read
does cross the boundary, so it must be permitted.

### Evaluation order and effects

- Explicit field expressions evaluate left-to-right in source order.
- `base` evaluates exactly once, after the explicit fields.
- Projected reads (`__base.f`) are pure and introduce no new effects; the only
  effects are those of the explicit field expressions and of evaluating `base`.

### Restrictions and non-goals

- One `..base`, last position, no trailing comma after it.
- Spread for **variants** and **tuples** is out of scope (Rust lacks it too; the
  variant case is what match-and-rebuild covers). `[..expr]` tuple value-spread
  is a separate, already-specified feature.
- Spread in **destructuring** is unrelated: `let { name, .. } = p` already uses
  `..` for "ignore the rest". That `..` is bare (no operand) and appears in
  patterns; `..base` has an operand and appears in expressions, so the two never
  collide.

## Compiler implementation

The feature is pure front-end sugar; it produces an ordinary `TIR
StructLiteral` (whose fields are already fully materialized and index-sorted), so
monomorphize / lower / optimize / codegen need no changes.

Pipeline touchpoints:

- `ast.rs` — add `spread: Option<Box<Expr>>` (plus its span) to
  `StructLiteralExpr`. Extend the `Expr` walkers/rewriters that recurse into
  struct-literal fields to also visit the spread operand.
- `parser.rs` — in `parse_struct_literal`, accept a trailing `DotDot` followed
  by an expression; enforce last-position and no-trailing-comma; store it in the
  new field.
- `elaborator/expr.rs` (`resolve_struct_literal` / the anonymous path) —
  resolve `base`, typecheck it against the struct type, feed its type into
  type-argument inference, suppress the missing-field diagnostic for fields
  `base` covers, and read-reachability-check each omitted field.
- `elaborator/reify.rs` (`reify_struct_literal`) — replace the "fill omitted
  fields from defaults" loop with "fill omitted fields from `base`": reify
  `base`, bind it to a `__base_N` temporary when it is not already a trivial
  place, emit a `FieldAccess` per omitted declared field, and wrap the result in
  a block carrying the temporary `let` (mirroring `reify_tuple_literal`'s spread
  handling). Defaults are only used when there is no spread.
- `unparse.rs` (`unparse_struct_literal`) — emit `..expr` as the final element
  in both inline and one-field-per-line layouts.
- `syntax.rs` — highlight `..base` in struct literals; regenerate the VS Code
  grammar (`mise run update-wado-vscode-grammar`) and formatter fixtures.

Docs to update on implementation: `docs/spec.md` (Struct Construction), 
`docs/cheatsheet.md` (Structs section), and the WEP index in `docs/CLAUDE.md`.

### Tests (red/green TDD)

- E2E fixtures in `wado-compiler/tests/fixtures/`: named and implicit literals;
  overriding zero / one / several fields; `base` a non-trivial expression
  evaluated once (observable via a side-effecting helper); generic struct with
  type args inferred from `base`; `..base` overriding a field that has a default;
  cross-module `..base` over `pub` fields (ok) and over a private field (error);
  parse errors (`..base` not last, trailing comma after `..base`, two spreads).
- `format.rs` fixtures for the formatter layouts.

## Consequences

- Removes the field-enumeration boilerplate from the `lower.wado` walkers, and
  makes adding a cached analysis field to an op struct a one-site change instead
  of a lockstep edit across every walker.
- Encapsulation is preserved by the read-reachability rule; no new escape hatch
  for private fields.
- No IR, monomorphization, or codegen changes — the sugar is gone by the time
  `TIR StructLiteral` is built.
- Slight asymmetry with default-omission semantics (spread reads across a
  boundary, default-omission does not), documented above as an intentional
  consequence of `..base` being a genuine read.
