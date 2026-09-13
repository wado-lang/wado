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

Three states want distinct answers, and only the first is a name the language
has:

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
that diagnostic:

```wado
#[unavailable("argument order is a known trap; write `map(f).unwrap_or(v)`")]
fn map_or();

#[unavailable(removed, since = "0.5.0", "use `Option::ok_or`")]
fn into_result();
```

```
error: `Option::map_or` is unavailable: argument order is a known trap; write `map(f).unwrap_or(v)`
error: `Option::into_result` was removed in 0.5.0: use `Option::ok_or`
```

### One attribute, two states

Both states answer the same call the same way — the name resolves, the call is
an error, and the reason is what the caller reads. What separates them is
whether the name existed before, which is one word and a version rather than a
second attribute. Swift reaches the same conclusion from the other direction,
folding its three states into `@available`.

`removed` names the state that existed, and requires `since`. Bare
`#[unavailable("...")]` is the state that never existed, where a `since` would
have nothing to date; writing one there is an error.

### The reason is mandatory

The reason is the last argument, a positional string, as
`#[compiler_item("option")]`, `#[cm("future-write")]`, and
`#[timeout_ms(5000)]` take theirs. An empty or missing string is a compile
error: an attribute that omits the sentence is worse than no attribute, since
it asserts a decision was made while hiding it.

`since` takes the `#[param(from_env = "PORT")]` key-value form.

### The declaration reserves a name, not a signature

The declaration may write an empty parameter list. What it reserves is the
name; parameters and return type, if written, are parsed but neither resolved
nor type-checked. Requiring the refused method's true Rust signature would be
surface that rots, and it sits badly with the policy the attribute exists to
record. Wado does not offer a Rust name under different parameters, so writing
those parameters out writes down the thing being refused.

A `removed` declaration may keep the signature the method had, as a record,
under the same rule.

### Name resolution

The declaration participates in name resolution, so the call site reaches it
and receives the reason rather than falling through to "no method named". It
is excluded from everything else. It never satisfies a trait requirement, and
it never reaches codegen.

### Placement

Module-level `fn`, `impl` method, and trait method.

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

1. Parse `#[unavailable]` on a bodyless `fn` at module level, in an `impl`, and
   in a `trait`, rejecting a missing or empty reason, a `removed` with no
   `since`, and a `since` without `removed`. Done when a declaration carrying
   it parses and a malformed one is diagnosed.
2. Carry it through name resolution so a call reaches the declaration and
   reports the reason, and so nothing else sees the name: no trait
   requirement satisfied, nothing emitted. Done when a call to an
   `#[unavailable]` method reports its reason and an `impl` carrying one does
   not count as implementing it.
3. Mark the methods Wado has already decided against, starting with
   `Option::map_or` and `Option::map_or_else` (see
   [WEP: Option and Result Value Methods](./wep-2026-09-13-option-result-methods.md)).
   Done when calling either reports the reason rather than "no method named".

## Known gaps

- `#[deprecated(since = "...", "...")]` is the family's third state and is not
  designed here. It stays callable and reports a warning, and it shares the
  mechanism above. What it needs beyond that is a warning path, a decision on
  whether to match Rust's `note = "..."` argument spelling, and its own answer
  on `wado doc`. A deprecated item still exists, which is why it is a separate
  attribute rather than a third `#[unavailable]` state, and why the reasoning
  above for leaving absences out does not carry to it.
- Types, traits, and globals cannot carry the attribute. Extending to them
  looks mechanical, and no use has asked for it.
- Whether a `removed` declaration is ever pruned, and on what schedule, is
  undecided. Left alone, they accumulate.
