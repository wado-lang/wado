# A Bare Name in a Pattern Always Binds

## Context

A bare identifier in a pattern means one of three things today:

- a binding, when it is neither of the others;
- a case, when it names one of the scrutinee type's cases (`None`, `Red`);
- a constant, when it reaches an immutable `global` and the pattern is
  refutable.

Only the elaborator can tell the first two apart, since only it knows the
scrutinee's type. The third depends on the names in scope and on the construct
around the pattern, so `let limit = v;` binds while `let limit = v else { … }`
compares with the global `limit`. Adding a declaration anywhere the pattern can
see changes what the pattern means. A misspelled case (`Rde =>`) binds and
matches every value.

A grammar that cannot say which names a pattern binds also holds back every
tool that reads one: the bind phase, the shadowing lint, the formatter, and a
highlighter cannot answer without types.

A call form (`Some(x)`) and a qualified path (`Color::Red`, `i32::MAX`) are not
ambiguous: no binding is spelled either way. Only the bare identifier is.

## Decision

The parser decides what every pattern node is, by its shape alone.

### A bare identifier binds

`x` and `mut x` bind, in every pattern, whatever the scrutinee's type and
whatever the name reaches. `let` and `let … else` read a pattern the same way.

A binding whose name is a case of the type it matches is an error. Writing
`Red =>` out of habit would otherwise bind and match every value.

### A case is a path or a call

| Form                 | Example                       | Type of the case    |
| -------------------- | ----------------------------- | ------------------- |
| Qualified            | `Color::Red`, `Option::Some(x)` | the qualifier       |
| Inferred qualifier   | `_::Red`, `_::Some(x)`        | the scrutinee's     |
| Call                 | `Some(x)`                     | the scrutinee's     |
| `null`               | `null`                        | its `None`          |

`_::` reads as a qualifier left for inference, as `with _` is an effect left
for inference. An expression whose expected type supplies the case takes the
same form: `let c: Color = _::Red;`.

### A constant is a qualified path or `==`

A qualified path that names an associated constant or a namespaced immutable
`global` (`i32::MAX`, `tok::TK_FOO`) matches by value, as now.

`== X` matches where `scrutinee == X` holds. Its operand is a path naming an
immutable `global` or an associated constant, bare or qualified. A path ends
before a `|` (an or-pattern) and before a `&&` (a guard) without the parser
asking which was meant.

```wado
match token {
    == TK_FOO | == TK_BAR => "keyword",
    i32::MAX => "max",
    other => `other ${other}`,
}
```

A range bound is never a binding, so a bare name there still names a constant
(`LOW..<HIGH`).

### `let … else` over a test alone

`let == X = y else { … }` is a pattern like any other, and binds nothing. The
lint `binding_free_let_else` warns where a `let … else` pattern binds no name,
and suggests `if y != X { … }`.

### What goes away

- The rule that a refutable pattern reads a bare name as a global, and the
  scope it is read from.
- The `shadowed_name` exemption for a name a case or a constant answers.
- `[mut limit, limit]` binding the first element and testing the second.

## Roadmap

- [ ] Parse `_::Case` in patterns and expressions, and `== operand` in
      patterns.
- [ ] `wado-dev-tools migrate-pattern-names`, which rewrites each bare name the compiler resolves to a
      case or a constant into the new form: `None` to `null`, another case to
      `_::Case`, a global to `== NAME`. It edits where the compiler resolved
      the name, never where the text matches.
- [ ] Migrate the stdlib, the packages, the fixtures and the docs.
- [ ] A bare identifier always binds; a binding named after a case of its type
      is an error.
- [ ] The `binding_free_let_else` lint.
- [ ] Move the rules into `spec-patterns.md`.

## Known gaps

- `==` takes a constant only. A local, or any other expression, is still
  compared by a guard (`v && v == x`).

- An expression still names a case bare where its expected type supplies one
  (`let c: Color = Red;`). That form keeps one expression node whatever it
  resolves to, so it does not have the pattern's problem, but it does resolve
  by type.
