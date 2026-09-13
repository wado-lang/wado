# WEP: Declared Absence

## Context

A library decides what it will not offer as deliberately as what it will. Wado
refuses `Option::map_or` because its argument order is a known trap, and
refuses any method that borrows a Rust name for a different signature or a
different meaning. Today that decision lives in a design document, where the
caller never meets it: the compiler answers a call to `map_or` with "no method
named `map_or`", which reads as an oversight and invites a pull request that
adds it.

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

Three attributes over one mechanism: a declaration carries a diagnostic
instead of a body.

```wado
#[not_provided("argument order is a known trap; write `map(f).unwrap_or(v)`")]
fn map_or();

#[removed(since = "0.5.0", "use `Option::ok_or`")]
fn into_result();
```

```
error: `Option::map_or` is not provided: argument order is a known trap; write `map(f).unwrap_or(v)`
error: `Option::into_result` was removed in 0.5.0: use `Option::ok_or`
```

`#[deprecated(since = "...", "...")]` is the third member — still callable,
reported as a warning — and is left to a later WEP. It is named here because
the three share one implementation and one place in `wado doc`, and because
the shape of the first two is chosen to leave room for it.

### Why three attributes rather than one with a state argument

Swift unifies its three under `@available`, but that attribute's primary axis
is the OS version, and carrying a message is optional there — an
`unavailable` with no explanation is expressible. The whole value here is that
the reason is mandatory, and the required information differs per state: a
removal without a version tells an upgrading reader nothing, while a `since`
on something that never existed is meaningless. Separate attributes let each
require exactly its own fields, and the attribute name is already the verb the
diagnostic needs.

### The reason is mandatory

Each attribute takes its reason as a positional string, matching the existing
`#[compiler_item("option")]`, `#[cm("future-write")]`, and
`#[timeout_ms(5000)]`. An empty or missing string is a compile error: the
attribute exists for the sentence, and one that omits it is worse than no
attribute at all, since it asserts a decision was made while hiding it.

`#[removed]` additionally requires `since = "<version>"`, in the
`#[param(from_env = "PORT")]` key-value form.

### The declaration reserves a name, not a signature

A `#[not_provided]` declaration may write an empty parameter list. What it
reserves is the name; parameters and return type, if written, are parsed but
neither resolved nor type-checked. Requiring the refused method's true Rust
signature would be surface that rots, and it sits badly with the policy the
attribute exists to record — Wado does not offer a Rust name under different
parameters, so writing those parameters out is writing down the thing being
refused.

`#[removed]` may keep the signature the method had, as a record. It is held to
the same rule: parsed, not resolved.

### Name resolution

The declaration participates in name resolution, which is the entire feature:
the call site must reach it and receive the reason, not fall through to "no
method named". It is excluded from everything else — trait implementation
checking does not count it as satisfying a requirement, and it never reaches
codegen.

### Documentation

`wado doc` renders a "Not provided" and a "Removed" section from these
declarations. The decision is written once, in the code, and arrives at both
the caller who tries it and the reader who browses the API.

### Placement

Module-level `fn`, `impl` method, and trait method. Types, traits, and globals
are not covered; extending to them is mechanical and waits for a use.

## Prerequisite: a bodyless function is an error

These attributes sanction a declaration with no body, so the unsanctioned case
has to mean something first. It did not: a bodyless `fn` at module level or in
an `impl` was accepted, bound as a symbol, and reached WIR, where the call it
could not resolve panicked with `[WIR] unresolved Call` (issue #2035).

`analyze` now rejects a function with no body unless something supplies one —
a Component Model binding (`#[cm]` / `#[canonical]`), a `core:builtin`
intrinsic, a binding or wasm-asset module, or a trait/interface method
declaration. The two attributes above join that list.

## Alternatives considered

**Leave it to documentation.** What the caller meets is the compiler, and the
compiler said "no method named `map_or`" — indistinguishable from an
oversight. A decision that only a design document carries is a decision the
caller never receives.

**One `#[unavailable(state, "...")]` attribute.** Nesting the state costs a
level of syntax on every use, and the per-state required fields still have to
be checked separately. The attribute name is free; spending it on the verb is
the better trade.

**Infer "removed" from a `since` argument on `#[not_provided]`.** Compact and
obscure: two materially different statements would differ only by the presence
of an optional argument.
