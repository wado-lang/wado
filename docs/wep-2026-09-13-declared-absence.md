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

A declaration carries a diagnostic instead of a body, and one attribute
per state says which diagnostic. Working spelling:

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

### One attribute per state

The states differ in what they must carry, not only in wording: a removal
without a version tells an upgrading reader nothing, while a `since` on
something that never existed is meaningless. Separate attributes let each
require exactly its own fields, and the attribute name is then the verb the
diagnostic needs.

Swift unifies its three under `@available`, but that attribute's primary axis
is the OS version, and a message is optional there — an `unavailable` with no
explanation is expressible. Mandatory reasons are the whole point here.

### The reason is mandatory

Each attribute takes its reason as a positional string, as
`#[compiler_item("option")]`, `#[cm("future-write")]`, and
`#[timeout_ms(5000)]` take theirs. An empty or missing string is a compile
error: an attribute that omits the sentence is worse than no attribute, since
it asserts a decision was made while hiding it.

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

`#[removed]` may keep the signature the method had, as a record, under the
same rule.

### Name resolution

The declaration participates in name resolution, which is the whole feature:
the call site reaches it and receives the reason rather than falling through
to "no method named". It is excluded from everything else — it never satisfies
a trait requirement, and it never reaches codegen.

### Placement

Module-level `fn`, `impl` method, and trait method.

### Not rendered by `wado doc`

These declarations reach the caller who writes the name from muscle memory,
which is the one reader who needs them. A browsing reader is looking for what
the API offers, and a list of what it does not is noise there.

### A bodyless function is otherwise an error

These attributes sanction a declaration with no body, so the unsanctioned case
has to mean something first. It did not: a bodyless `fn` at module level or in
an `impl` was accepted and reached WIR, where the call it could not resolve
panicked (issue #2035). A function with no body is now rejected unless
something supplies one — a Component Model binding (`#[cm]` / `#[canonical]`),
a `core:builtin` intrinsic, a binding or wasm-asset module, or a
trait/interface method declaration. The two attributes join that list.

## Roadmap

1. Settle the attribute spelling (see Known gaps). Nothing below starts until
   the names are fixed, since each step writes them into source.
2. Parse both attributes on a bodyless `fn` at module level, in an `impl`, and
   in a `trait`, rejecting a missing or empty reason and a `#[removed]` with
   no `since`. Done when a declaration carrying either parses and a
   malformed one is diagnosed.
3. Carry them through name resolution so a call reaches the declaration and
   reports the reason, and so nothing else sees the name — no trait
   requirement satisfied, nothing emitted. Done when a call to a
   `#[not_provided]` method reports its reason and an `impl` carrying one does
   not count as implementing it.
4. Mark the methods Wado has already decided against, starting with
   `Option::map_or` and `Option::map_or_else` (see
   [WEP: Option and Result Value Methods](./wep-2026-09-13-option-result-methods.md)).
   Done when calling either reports the reason rather than "no method named".

## Known gaps

- The attribute names are not settled. `not_provided` / `removed` are the
  working spelling used above; the alternatives raised are a single
  `#[unavailable(state, "...")]` carrying the state as an argument, and
  inferring "removed" from the presence of `since`. Closing this is a naming
  call, and every step of the roadmap depends on it.
- `#[deprecated(since = "...", "...")]` — still callable, reported as a
  warning — is the third member of the family and is not designed here. It
  shares the mechanism; what it needs beyond that is a warning path, a
  decision on whether to match Rust's `note = "..."` argument spelling, and
  its own answer on `wado doc` — a deprecated item still exists, so the
  reasoning above for leaving absences out does not carry to it.
- Types, traits, and globals cannot carry these attributes. Extending to them
  looks mechanical, and no use has asked for it.
- Whether a `#[removed]` declaration is ever pruned, and on what schedule, is
  undecided. Left alone, they accumulate.
