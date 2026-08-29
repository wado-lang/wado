# WEP: Type Stringification

## Context

Converting an arbitrary value to a string splits into two questions a language
answers separately: what a value looks like when a developer is debugging, and
what it looks like in user-facing output.

| Language | Debug output                  | User-facing output      |
| -------- | ----------------------------- | ----------------------- |
| Rust     | `{:?}`, `#[derive(Debug)]`    | `Display`, hand-written |
| Elixir   | `Inspect` protocol, total     | `String.Chars` protocol |
| Python   | `__repr__`, default unhelpful | `__str__`               |
| Ruby     | `inspect`                     | `to_s`                  |
| Kotlin   | `toString()`, data class only | the same `toString()`   |

Wado has no macros, so a derive-style opt-in is not available: debug output is
either always present or absent for most types.

## Decision

Debug output is total; user-facing output is opt-in.

| Form     | Trait     | Availability                                          |
| -------- | --------- | ----------------------------------------------------- |
| `${x:?}` | `Inspect` | Every type. Derived from the type's shape             |
| `${x}`   | `Display` | Only where an impl exists — otherwise a compile error |

The two never substitute for each other. `${x}` does not fall back to debug
output, so `T: Display` certifies that the type has a string form its author
chose; and an `impl Display` never changes what `${x:?}` prints, so the escape
hatch survives a wrong or misleading `Display`.

Debug output follows Wado literal syntax — `Point { x: 10, y: 20 }`,
`[1, "a", true]`, `Option::Some(42)` — so it reads as the expression that
builds the value. The per-type format is specified in
[WEP: Inspect](./wep-2026-02-21-inspect-debug-output.md), the derivation rules
in
[WEP: Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md), and both
traits' signatures in
[WEP: Format Traits](./wep-2026-02-01-format-traits.md).

## Consequences

Debugging needs no preparation: `${x:?}` works on a type the moment it is
declared, and keeps working as fields and cases change. The cost is code size —
each inspected type instantiates an impl — which DCE reclaims for the types a
program never prints.

Requiring `Display` costs a hand-written impl on every struct and variant meant
for user-facing output, and rejects `${x}` on the ones that skip it. That is
what makes the two forms distinguishable at a glance.

### Known gaps

- [ ] Depth limit for recursive types — see
      [WEP: Inspect](./wep-2026-02-21-inspect-debug-output.md).

## References

- [WEP: Inspect (Debug Output)](./wep-2026-02-21-inspect-debug-output.md)
- [WEP: Format Traits](./wep-2026-02-01-format-traits.md)
- [WEP: Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md)
- [WEP: Struct and Trait System](./wep-2026-01-13-struct-and-trait.md)
- [Elixir Inspect Protocol](https://hexdocs.pm/elixir/Inspect.html)
- [Rust Debug and Display](https://doc.rust-lang.org/rust-by-example/hello/print/print_debug.html)
