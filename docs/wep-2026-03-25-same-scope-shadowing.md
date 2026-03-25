# WEP: Same-Scope Shadowing with Self-Reference

## Decision

Allow `let` declarations to shadow a variable in the same scope **if and only if** the initializer expression references the variable being shadowed. The reference must be to the variable itself — not to a same-named parameter, field, or method.

```wado
let x = 1;
let x = x + 1;           // OK: RHS references `x`
let x = transform(x);    // OK: RHS references `x` (inside a call)
let x = 2;               // Error: RHS does not reference `x`
```

This preserves Wado's protection against accidental shadowing while enabling the most common and intentional pattern: transforming a value in place without `mut`.

## Context

### Current Behavior

Wado prohibits all same-scope shadowing:

```wado
let x = 1;
let x = x + 1;  // Error: cannot redeclare 'x' in the same scope
```

The workarounds are:

1. **Mutation**: `let mut x = 1; x = x + 1;` — forces `mut` on a conceptually immutable pipeline
2. **Renaming**: `let x2 = x + 1;` — proliferates meaningless names (`x2`, `x3`, ...)
3. **Nested scope**: `scope: { let x = x + 1; ... }` — adds unnecessary indentation

### Rust's Unrestricted Shadowing

Rust allows any same-scope shadowing without restriction:

```rust
let x = 1;
let x = "hello";  // OK in Rust — completely unrelated rebinding
```

This is controversial in the Rust community. Clippy provides `shadow_unrelated` to lint against shadowing where the old value is not used. The consensus is that **self-referential shadowing** (`let x = x + 1`) is good practice, while **unrelated shadowing** (`let x = "hello"`) is a code smell.

### Motivation from Real Code

Generated parser code (e.g., `package-gale/`) frequently transforms values in sequence. Without same-scope shadowing, these pipelines require either `mut` or invented names:

```wado
// Current: forced mut
let mut result = parse(input);
result = result.trim();
result = result.to_lowercase();

// With this WEP:
let result = parse(input);
let result = result.trim();
let result = result.to_lowercase();
```

The second form is clearer: each `let` signals a new immutable binding derived from the previous value.

## Specification

### Rule

A `let` declaration may shadow a same-scope binding of name `N` if and only if the initializer expression contains at least one **variable reference** to `N` that resolves to that same-scope binding.

### What Counts as a Reference

The check is performed during the bind phase. A "variable reference" is an `Ident` AST node that resolves to the variable being shadowed. The check walks the entire initializer expression, including nested sub-expressions:

| Expression | References `x`? | Reason |
|---|---|---|
| `let x = x + 1` | Yes | Direct reference |
| `let x = transform(x)` | Yes | Passed as argument |
| `let x = if cond { x } else { 0 }` | Yes | In branch |
| `let x = match opt { Some(v) => x, None => 0 }` | Yes | In match arm |
| `let x = [x, 1, 2]` | Yes | In array/tuple literal |
| `let x = \|\| x + 1` | Yes | Captured in closure |
| `let x = some_fn(\|\| x)` | Yes | Captured and passed |
| `let x = 2` | **No** | No reference to `x` |
| `let x = y + 1` | **No** | References `y`, not `x` |
| `let x = \|x\| x + 1` | **No** | `x` in body refers to the parameter, not the outer variable |
| `let x = foo.x()` | **No** | `.x()` is a method call, not a variable reference |
| `let x = point.x` | **No** | `.x` is a field access, not a variable reference |

### Semantics

The shadowing declaration creates a **new binding** that replaces the previous one in the current scope. The old binding becomes inaccessible after the declaration. The initializer is evaluated with the old binding still in scope (the new binding does not exist yet during RHS evaluation).

This is identical to how nested-scope shadowing already works:

```wado
let x = 1;
let x = x + 1;  // RHS `x` refers to the old binding (value 1), LHS creates new binding (value 2)
// `x` is now 2; the old binding (value 1) is inaccessible
```

### Mutability

The new binding's mutability is independent of the old binding:

```wado
let x = 1;
let mut x = x + 1;  // OK: new mutable binding from old immutable one
```

### Error Messages

When same-scope shadowing is rejected (RHS does not reference the variable):

```
3:5: error: cannot redeclare 'x' in the same scope (first defined at 2:5)
  hint: shadowing is allowed when the new value is derived from the old one (e.g., `let x = x + 1`)
```

When the user might have intended to reassign instead of redeclare:

```
3:5: error: cannot redeclare 'x' in the same scope (first defined at 2:5)
  hint: shadowing is allowed when the new value is derived from the old one (e.g., `let x = x + 1`)
```

## Alternatives Considered

### Allow All Same-Scope Shadowing (Rust-style)

Simpler rule but loses protection against accidental unrelated shadowing — the exact footgun Rust's `shadow_unrelated` lint addresses.

### Keep Current Behavior (No Same-Scope Shadowing)

Safe but forces `mut` or naming proliferation for common transformation patterns.
