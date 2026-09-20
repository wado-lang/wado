# WEP: Declared Absence

## Context

A library decides what it will not offer as deliberately as what it will. Wado
does not offer `Option::map_or`, and does not offer any method that borrows a
Rust name for a different signature or a different meaning. That decision
lives in a design document, where the caller never meets it: the compiler
answers a call to `map_or` with "no method named `map_or`", which reads as an
oversight and invites a pull request that adds it.

The same gap opens after a removal. A method taken out of the standard library
leaves no trace, so an upgrade reports the same "no method named" and the
reader has nowhere to learn what replaced it.

Three states want distinct answers, and the language can spell none of them:

| State        | Callable   | Existed before | Required information |
| ------------ | ---------- | -------------- | -------------------- |
| deprecated   | yes (warn) | yes            | replacement          |
| removed      | no (error) | yes            | version, replacement |
| not provided | no (error) | no             | reason               |

The field-tested precedents all attach the explanation to the declaration
rather than to prose: Swift's `@available(*, unavailable, message:)` and
`@available(*, deprecated, renamed:)`, Kotlin's
`@Deprecated(level = DeprecationLevel.ERROR, replaceWith = ...)`, C#'s
`[Obsolete(message, error: true)]`, and C++26's `= delete("should have a
reason")` (P2573), which follows a deliberate progression of message-carrying
diagnostics: `static_assert` (C++11), `[[deprecated]]` (C++14),
`[[nodiscard]]` (C++20).

## Decision

A declaration carries a diagnostic instead of a body, and `#[unavailable]` is
that diagnostic. One argument, the sentence the caller reads:

```wado
#[unavailable("argument order is a known trap; write `map(f).unwrap_or(v)`")]
fn map_or();

#[unavailable("removed in 0.5.0; use `Option::ok_or`")]
fn into_result();
```

```
error: `Option::map_or` is unavailable: argument order is a known trap; write `map(f).unwrap_or(v)`
error: `Option::into_result` is unavailable: removed in 0.5.0; use `Option::ok_or`
```

### One attribute, and the states live in the sentence

Both states answer the same call the same way: the name resolves, the call is
an error, and the reason is what the caller reads. What separates them is a
fact about the past, and the caller is the only one who reads it. Nothing
machine-processes a removal version, so a field for it buys a validation rule
and a second way to be wrong, and buys nothing else. An author who wants to
date a removal writes the date in the sentence.

Swift reaches the same conclusion from the other direction, folding its three
states into `@available`; this goes one step further and folds the argument in
too.

### The reason is mandatory

The reason is a positional string, as `#[compiler_item("option")]`,
`#[cm("future-write")]`, and `#[timeout_ms(5000)]` take theirs. An empty or
missing string is a compile error: an attribute that omits the sentence is
worse than no attribute, since it asserts a decision was made while hiding it.

### The declaration reserves a name, not a signature

The declaration may write an empty parameter list. What it reserves is the
name; parameters and return type, if written, are parsed but neither resolved
nor type-checked. Requiring the refused method's true Rust signature would add
surface that goes stale, and it sits badly with the policy the attribute exists
to record. Wado does not offer a Rust name under different parameters, so
writing those parameters out writes down the thing being refused.

`self` is the exception, because it is not part of the signature but of which
name is reserved: an instance method and a static one are different names on
the same type, and only `self` tells them apart. A declaration reserving
`opt.map_or(…)` writes `fn map_or(&self)`.

A call that reaches such a declaration stops there. Its arguments are never
counted or typed, since the parameters they would be checked against are not
a signature. Naming it without calling it gets the same answer: there is no
value to take, only the reason.

A declaration standing in for a removed method may keep the signature it had,
as a record, under the same rule.

### Name resolution

The declaration participates in name resolution, so the call site reaches it
and receives the reason rather than falling through to "no method named". It
is excluded from everything else. It never satisfies a trait requirement, and
it never reaches codegen.

### Placement

Module-level `fn`, `impl` method, and trait method. Writing it anywhere else is
an error, rather than a line the compiler ignores. Known gaps lists the
placements still to come. `export` is refused with it: the keyword lowers a
function at the component boundary, and this declaration has none to lower.

### Not rendered by `wado doc`

These declarations reach the caller who writes the name from muscle memory,
which is the one reader who needs them. A browsing reader is looking for what
the API offers, and a list of what it does not is noise there.

### A bodyless function is otherwise an error

The attribute sanctions a declaration with no body, so the unsanctioned case
has to be an error first. A function with no body is rejected unless something
supplies one: a Component Model binding (`#[cm]` / `#[canonical]`), a
`core:builtin` intrinsic, a binding or wasm-asset module, or a trait/interface
method declaration (issue #2035). `#[unavailable]` joins that list.

## Roadmap

Nothing is queued. The gaps below are unowned.

## Known gaps

- `#[deprecated("...")]` is the family's third state and is not designed here.
  It stays callable and reports a warning, and it shares the mechanism above.
  What it needs beyond that is a warning path and its own answer on `wado doc`.
  A deprecated item still exists, which is why it is a separate attribute
  rather than a third `#[unavailable]` state, and why the reasoning above for
  leaving absences out does not carry to it.
- Every item should carry the attribute eventually. Today only a module `fn`,
  an `impl` method, and a trait method do; a type, a trait, a global, an
  `interface` operation, and a `resource` method reject it. Each extension
  looks mechanical, and none is owned.
- A trait may declare a name `#[unavailable]` and an `impl` may still supply a
  body for it, which the receiver's own type then dispatches to. Wado has no rule
  for any member that an `impl Trait for T` declares only what the trait
  declares.
- Whether a declaration standing in for a removed method is ever pruned, and on
  what schedule, is undecided. Left alone, they accumulate.
