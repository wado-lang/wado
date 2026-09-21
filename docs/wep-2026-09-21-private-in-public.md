# WEP: Private Types in a Public Signature

## Context

A signature is a promise the caller has to be able to write down. Wado made no
such promise: an item could reach further than the types it names, so a caller
held a value whose type had no name it could spell.

`core:prelude` shipped seven of these. `String::split` is `pub` and returns
`StrSplitIter`, which was `internal`, so `for let p of s.split(" ")` worked
while `fn count(it: StrSplitIter)` read "unknown type" and importing the name by
path said to mark it `pub`. `core:collections` is worse: `TreeMap::keys`,
`TreeMap::values`, `TreeMap::entries` and five siblings return types with no
visibility modifier at all, file-private to `collections.wado`. The `pub` on the
method was a promise the module could not keep.

Nothing reports this. Both batches were found by grep, and a grep finds them
only when someone thinks to look.

Rust met the same problem as E0446 and now splits it across three lints:
`private_interfaces` for the signature, `private_bounds` for the bounds, and
`unnameable_types` for a type that is reachable but has no path. All three warn
or allow rather than fail, because Rust had a corpus to keep compiling.

## Decision

An item's signature may not name a declaration that reaches less far than the
item itself. A violation is a `PRIVATE_SYMBOL` compile error, worded at the
reference, not the declaration: that is where the promise is written.

It is a hard error, not a lint. Wado is pre-stable, the whole corpus holds a few
dozen violations, and every one of them is a defect its author would fix. A
warning here would be a warning nobody reads and a promise still broken.

`export` counts as `pub` on this axis. It already implies `pub`, and the CM
boundary asks a separate question — whether the signature is representable —
that the definition site already answers.

The rule holds at every rung of the ladder, not just at `pub`. An `internal`
item naming a file-private type breaks the same promise one rung down, and a
rule that fired only at `pub` would be two rules.

An item's reach is what its own modifier says, except where the item has no
modifier of its own:

- A trait impl's method reaches as far as the trait, which is what decides
  whether a caller can name the method at all.
- A struct field reaches no further than its struct, so the field is checked at
  the narrower of the two.

A type parameter, `Self`, and an associated-type projection (`Self::Output`,
`I::Item`) are binders rather than declarations. They carry no visibility of
their own — the item or trait introducing them carries it — so they are not
checked.

### No exception for sealed traits

Rust keeps `private_bounds` a lint so the sealed-trait pattern survives:
`pub trait Foo: private::Sealed` seals `Foo` against outside implementations
precisely because `Sealed` cannot be named. The seal is a use of reduced
visibility, so a rule against reduced visibility would outlaw it.

Wado seals nothing. Every `pub trait X: Y` in the corpus has a `pub` `Y`, so the
bound axis takes the same hard error with no waiver. Should sealing be wanted,
it gets a mechanism that says so — a keyword on the trait — rather than a
visibility trick the checker has to be taught to overlook.

## Roadmap

- [ ] The check runs once over each module's declarations, after resolution,
      where a reference site already answers with the declaration it names. It
      is finished when every signature shape is covered: function parameters,
      return type, struct and variant fields, global type, trait bounds, and the
      trait head's supertraits.
- [ ] `core:collections` publishes the eight iterator types its `pub` methods
      return, beside `TreeMap` and `TreeSet`. Done when the types are `pub` and
      the module re-exports them.
- [ ] `package-marl` and `package-gale` widen or narrow what the check finds.
      Each site is its author's choice between publishing the type and taking
      the `pub` off the item; done when the corpus compiles.
- [ ] `docs/spec.md` and `docs/cheatsheet.md` state the rule in their Visibility
      sections.

## Known gaps

- A `pub` type still needs a path to be named. `core:`'s implementation modules
  mark sibling-only symbols `pub`, so a consumer can reach one by file path that
  the facade never meant to publish; narrowing those is the gap
  [WEP: Visibility](./wep-2026-06-25-visibility-internal-pub-export.md) already
  records. This rule makes a signature's types nameable in principle; it does
  not decide which module is the one to name them through.
- Nothing checks a type parameter's default (`fn info<T: Serialize = NoFields>`)
  against the item's reach, since a default is written where the caller can omit
  it and read where the caller cannot see it.

## References

- [Visibility — `internal` / `pub` / `export`](./wep-2026-06-25-visibility-internal-pub-export.md)
- [Super Traits](./wep-2026-07-27-super-traits.md)
- [Declaration Identity](./wep-2026-08-12-declaration-identity.md)
