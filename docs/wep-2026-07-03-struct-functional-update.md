# Functional-Update / Spread Syntax for `{ … }` Literals (`..base`)

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

The same `{ key: value, … }` surface also builds key-value collections
(`TreeMap`) via the `KeyValueLiteralBuilder` desugar. "Clone this map and change
a few keys" has the same boilerplate problem, so this WEP scopes the `..base`
**syntax** to `{ … }` literals in general, covering both struct literals and
key-value literals with one grammar and one precedence rule.

## Decision

Add a `..base` spread element to `{ … }` literals. `..base` supplies every
member the literal does not list explicitly, taken from the value `base`. It
applies uniformly to:

- **struct literals** — `base` is a struct value of the same type; spread is a
  compile-time projection of the omitted declared fields;
- **key-value literals** — `base` is a collection of the same type (e.g.
  `TreeMap<String, V>`); spread seeds the builder with `base`'s entries at
  runtime.

The two share the grammar, the precedence rule, and the AST node; only the reify
lowering differs.

### Syntax

`..base` is an element of a `{ … }` literal's element list, in **any position**:

```wado
S { a: expr, ..base }        // tail
S { ..base, a: expr }        // head
S { a: expr, ..base, b: e }  // between
```

Rules:

- **At most one** `..base` per literal.
- Position is free (see [Precedence](#precedence-explicit-always-wins) for why).
- A trailing comma after `..base` is allowed, matching every other element
  (`{ ..base, }`); the only restriction is the single-spread rule.
- `base` is an arbitrary expression whose type is the same as the literal being
  constructed (see [Type rules](#type-rules)).
- `..` is the existing single `DotDot` token; `{ ...base }` is the same
  "did you mean `..`?" parse error as elsewhere.

It works with named and implicit (anonymous) struct literals, and with key-value
literals, taking the target type from context exactly as today:

```wado
let updated: Config = { port: 3000, ..base };     // struct, implicit
let m2: TreeMap<String, i32> = { "a": 1, ..m };   // key-value
```

### Precedence — explicit always wins

For a duplicated member, the **explicitly listed** value wins; `..base` only
supplies members whose key/field is **not** listed explicitly. Precedence is by
explicitness, **not by source order** — Wado does not adopt JS's positional
last-wins.

This single rule is what lets `..base` sit anywhere: since a member is either
explicit or from `base` (never a race between them), moving `..base` around the
literal cannot change the result. It is also what keeps the struct case correct:
`{ min_prec_arg: Some(prec), ..rc }` and `{ ..rc, min_prec_arg: Some(prec) }`
both mean "`min_prec_arg` explicit, every other field from `rc`."

Trade-off (key-value only): you cannot express "let `base` override my explicit
keys." That base-wins merge is rare and belongs to an explicit method
(`m.merge(base)`), not to literal syntax.

### Evaluation order and single evaluation

- Every subexpression — each explicit value and `base` — is evaluated **once**,
  left-to-right in source order.
- Value precedence (explicit-wins) is applied when the value is constructed and
  is independent of that evaluation order.
- When `base` is a trivial place (a local) the compiler reads it directly;
  otherwise it binds `base` (and any effectful explicit value it must reorder
  past) to a temporary, reusing the mechanism `reify_tuple_literal` already uses
  for `[..expr]` value spread.

### Struct semantics — field projection

For a struct literal, `..base` desugars to reading each **omitted** declared
field from `base`:

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
- The result is an ordinary `TIR StructLiteral`; nothing downstream changes.

### Key-value semantics — builder seed

For a key-value literal, `..base` seeds the `KeyValueLiteralBuilder` with
`base`'s entries, then applies the explicit `insert_literal` calls **after** the
seed so explicit keys win regardless of where `..base` was written:

```wado
{ "a": 1, ..base }
```

desugars to

```wado
{
    let __b = TreeMap::new_literal(cap);
    __b.insert_all(base);        // seed: base's entries (complement)
    __b.insert_literal("a", 1);  // explicit — overrides base's "a"
    break __kv_lit: __b.build();
}
```

This needs **one** new method on the `KeyValueLiteralBuilder` trait, because
`base`'s keys are unknown at compile time (so the compiler cannot emit an
`insert_literal` per key):

```wado
internal trait KeyValueLiteralBuilder {
    type Value;
    type Output;
    fn new_literal(capacity: i32) -> Self;
    fn insert_literal(&mut self, key: String, value: Self::Value);
    fn insert_all(&mut self, base: Self::Output);   // NEW: bulk-seed from a base
    fn build(&self) -> Self::Output;
}
```

`insert_all` for `TreeMap<String, V>` iterates `base` and `self.insert`s each
entry — a normal library method, no new IR. The rest of the desugar is the
existing `reify_key_value_coercion` path unchanged. Since explicit inserts run
after the seed, they overwrite, giving the explicit-wins rule for free.

### Type rules

- `base` must have the **same type** as the literal: the same struct for a
  struct literal, or the same collection type (e.g. `TreeMap<String, V>`) for a
  key-value literal. For a generic type it is the same constructor with unifiable
  type arguments.
- `base` participates in **type-argument inference** the same way an explicit
  member does: `Wrapper { ..w }` with `w: Wrapper<i32>` infers `Wrapper<i32>`;
  `{ ..m }` with `m: TreeMap<String, i32>` infers the value type. Explicit
  members and `base` must agree; a conflict is the usual type error.
- A `base` whose type is not the literal's type is a type error
  (`..base` requires a value of type `T`, found `U`).

### Interaction with field defaults (struct only)

When `..base` is present, **every omitted field is taken from `base`**; struct
field defaults (`port: i32 = 8080`) are not consulted. `base` already holds a
complete value for every field, so a struct literal with `..base` can never be
"missing a required field" — the missing-field diagnostic is suppressed for all
fields `base` covers. Extra/unknown-member and duplicate-member checks on the
explicit members are unchanged. (Key-value literals have no defaults.)

### Interaction with visibility (struct only)

Projecting `__base.f` is a **read** of field `f`, subject to the same
read-reachability as any field access:

- Within the defining module every field is readable, so intra-module `..base`
  (the issue's entire use case) always works.
- Across a module boundary, `..base` may only project fields that are
  reachable-for-read (`pub`, or `internal` within the same package). Omitting a
  non-reachable field is the same compile error as writing `base.secret` there.

This preserves encapsulation: `..base` cannot smuggle a private field across a
boundary. It is deliberately stricter than default-omission (a private field
with a default may be _omitted_ cross-module because its default runs in the
defining module and no read crosses the boundary); with `..base` a real read
does cross the boundary, so it must be permitted. (Key-value literals have no
per-key visibility; `base` is read as a whole value.)

### Restrictions and non-goals

- **One** `..base` per literal (multi-source merge — `{ ..a, ..b }` — is a
  future question; it would reintroduce order-dependence among spreads and is not
  needed for the issue).
- Spread for **variants** and **tuples** is out of scope (Rust lacks it too; the
  variant case is what match-and-rebuild covers). `[..expr]` tuple value-spread
  is a separate, already-specified feature.
- Spread in **destructuring** is unrelated: `let { name, .. } = p` already uses
  `..` for "ignore the rest". That `..` is bare (no operand) and appears in
  patterns; `..base` has an operand and appears in expressions, so the two never
  collide.

## Compiler implementation

Struct literals and key-value literals share one AST node (`StructLiteralExpr`);
the elaborator decides between the two via the recorded key-value-coercion fact.
So the syntax and AST change once and both paths pick it up.

Pipeline touchpoints:

- `ast.rs` — add `spread: Option<Box<Expr>>` (plus its span) to
  `StructLiteralExpr`, independent of the element list so its position does not
  matter. Extend the `Expr` walkers/rewriters to visit the spread operand.
- `parser.rs` — in `parse_struct_literal`, accept a `..expr` element anywhere in
  the field list; enforce the single-spread rule; store it in the new field.
- `elaborator/expr.rs` (`resolve_struct_literal` / the anonymous path) —
  resolve `base`, typecheck it against the literal's type, feed its type into
  type-argument inference, and for the struct case suppress the missing-field
  diagnostic and read-reachability-check each field `base` covers.
- `elaborator/reify.rs`:
  - `reify_struct_literal` — fill omitted fields from `base` (field projection)
    instead of from defaults when a spread is present; bind `base` to a
    `__base_N` temporary when non-trivial.
  - `reify_key_value_coercion` — emit `insert_all(base)` before the explicit
    `insert_literal` calls when a spread is present.
- `lib/core/prelude/traits.wado` + `lib/core/collections.wado` — add
  `insert_all` to `KeyValueLiteralBuilder` and implement it for `TreeMap`
  (and any other key-value builder).
- `unparse.rs` (`unparse_struct_literal`) — emit `..expr` in its source position
  in both inline and one-field-per-line layouts.
- `syntax.rs` — highlight `..base`; regenerate the VS Code grammar
  (`mise run update-wado-vscode-grammar`) and formatter fixtures.

Downstream (`monomorphize` / `lower` / `optimize` / `codegen`) is untouched: the
struct path yields a plain `StructLiteral`, and the key-value path yields the
same builder-call block the coercion already produces.

Docs to update on implementation: `docs/spec.md` (Struct Construction; key-value
literals), `docs/cheatsheet.md`, and the WEP index in `docs/CLAUDE.md`.

### Tests (red/green TDD)

- Struct: named and implicit literals; overriding zero / one / several fields;
  `..base` at head, tail, and between fields (identical result); `base` a
  non-trivial expression evaluated once (observable via a side-effecting helper);
  generic struct with type args inferred from `base`; `..base` overriding a field
  with a default; cross-module `..base` over `pub` fields (ok) and a private
  field (error).
- Key-value: `{ "k": v, ..m }` and `{ ..m, "k": v }` (explicit-wins, same
  result); seeding from an empty base; overriding an existing key; type inference
  from `base`.
- Parse errors: two spreads in one literal; `{ ...base }`.
- `format.rs` fixtures for the formatter layouts.

## Consequences

- Removes the field-enumeration boilerplate from the `lower.wado` walkers, and
  makes adding a cached field to an op struct a one-site change instead of a
  lockstep edit across every walker.
- One grammar and one precedence rule (explicit-wins, position-free) cover both
  struct and key-value literals, so users learn `..base` once.
- The key-value path costs exactly one new library method (`insert_all`); no new
  IR, monomorphization, or codegen changes on either path.
- We forgo JS-style base-wins merge for key-value literals; an explicit `.merge`
  method covers that rare case.
