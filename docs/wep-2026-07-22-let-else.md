# WEP: `let ... else` Statements

## Context

A refutable pattern in a plain `let` is a compile error — `let` demands an
irrefutable pattern. The common shape "bind this or bail out early" therefore
needs `if let`, which pushes the happy path one level of indentation to the
right and leaves the bindings scoped to the nested block:

```wado
if let Ok(port) = i32::from_str(&s) {
    // everything that uses `port` lives here, indented
    ...
} else {
    return -1;
}
```

Rust solves this with `let ... else` (RFC 3137). Wado adopts the same
construct.

## Decision

### Syntax

```
let PATTERN = EXPR else { DIVERGING_BLOCK };
```

- Only valid with an initializer (`= EXPR`); `let x: T else { ... }` is a parse
  error.
- `PATTERN` may be refutable (`Some(x)`, `Ok(v)`, a variant, a tuple/struct
  with refutable sub-patterns, …).
- When the pattern matches, its bindings enter the enclosing scope and are
  visible for the rest of the block — exactly as a plain `let` would bind them.
- When the pattern does not match, the `else` block runs. It must diverge
  on every path (`return`, `break`, `continue`, `panic`, `unreachable`, or a
  call to a `Never`-returning function).
- The `else` block does not see the pattern's bindings.
- An irrefutable pattern is rejected: its `else` block could never run, so a
  plain `let` should be used instead (mirrors Rust's `irrefutable_let_patterns`
  deny lint).

```wado
fn parse_port(s: String) -> i32 {
    let Ok(port) = i32::from_str(&s) else {
        return -1;
    };
    return port;              // `port` in scope
}
```

Because `break`/`continue` diverge, a `let ... else` inside a loop can skip or
stop iteration on a failed bind:

```wado
for let it of items {
    let Some(n) = it else { continue; };
    sum += n;
}
```

### Implementation

`LetStmt` gains an `else_block: Option<Block>`. The parser fills it when an
`else` follows the initializer (statement position only — a C-style for-loop
initializer has no "rest of block" to guard, so `else` is not accepted there).

`let ... else` desugars, at reify time, into a two-arm `Match` — the same
lowering `if let` uses (`reify_let_chain_stmts`), with one twist: the then-arm
is the rest of the enclosing block, so the pattern's bindings are in scope
for it, and the wildcard arm is the diverging `else` block:

```
{ ...; let PAT = EXPR else { ELSE }; REST }
⇒
{ ...; match EXPR { PAT => { REST }, _ => { ELSE } } }
```

The else block is resolved/reified before the pattern bindings enter scope, so
it cannot reference them; the scrutinee, else block, pattern, and continuation
are walked in the same order in the records-only resolve pass and the
TIR-building reify pass, keeping the monotonic local-index allocation identical
between the two.

Divergence is checked with the existing AST control-flow analysis
(`control_flow::block_always_exits`), extended to treat a `break`/`continue`
statement as exiting (it already counted them as `Never` in
`block_result_type`).

## Consequences

- No new TIR/NIR/WIR node: the construct is fully desugared to `Match` at reify,
  so the optimizer and codegen need no changes.
- Match ergonomics, or-patterns, guards-free refutable patterns, and
  destructuring all work, since the pattern flows through the same
  `reify_pattern` path as `if let`.
- The diverging-else and irrefutable-pattern rules are enforced with dedicated
  diagnostics (`LetElseMustDiverge`, and an `InvalidPattern` message).
- `let ... else` is a statement, not an expression; it never produces a value.
