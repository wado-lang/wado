# Bodyless Declarations State Every Fact

## Context

A function with a body has its facts read from that body: what its result
shares, what it keeps past the return, when it traps, and how it touches linear
memory. A declaration with no body has nothing to read, so it states those facts
as attributes: `#[result]`, `#[retain]`, `#[trap]` and `#[linear_memory]`. The
facts exist so the optimizer can go as far as correctness allows. A missing one
costs either way: a cautious reading gives up an optimization, and a hopeful one
miscompiles.

Each attribute read its absence its own way:

| Attribute          | Absence read as                                    | Cost                                      |
| ------------------ | -------------------------------------------------- | ----------------------------------------- |
| `#[result]`        | a new result that may hold what any argument holds | a lost optimization where it holds less   |
| `#[retain]`        | keeps nothing                                      | a miscompile where the call does keep one |
| `#[trap]`          | may trap                                           | a lost optimization where it never does   |
| `#[linear_memory]` | touches none                                       | a miscompile where the call does touch it |

Of the 353 body-less `core:builtin` declarations, 60 state no `#[trap]`. Some of
them, such as `copy_value`, `variant_tag` and `cold_path`, never trap. The
optimizer still keeps every call to them. Only 19 state `#[linear_memory]`. The
rest read as touching none, and nothing checks that they don't.

`owned` meant two things. `#[result(owned)]` said the result's storage was new
and held nothing it was handed. The reading of absence was also called owned,
though it let the result hold anything. And the vocabulary had no way to state
the precise answer for `builtin::select`, whose result is one of its two
operands.

## Decision

### A Fact Is Stated or Proved, Never Defaulted

Each fact a body-less declaration owes is either written on it, or proved from
its signature or its kind. Where neither holds, leaving it out is an error. An
absence never stands for an answer.

The vocabulary holds exactly the distinctions the optimizer reads. A word that
would change no optimization is not added, even where it would describe the call
more fully.

### Where the Facts Come From

| Declaration                                              | Facts                                                                                                                           |
| -------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| `core:builtin`, body-less                                | stated, except where the signature proves one                                                                                   |
| Component Model import                                   | read off the kind: the boundary copies, so the result is `fresh` and nothing is retained; it may trap, and writes linear memory |
| `.wasm` / `.wat` asset import                            | read off the kind: only numbers cross, so nothing is shared or retained; it may trap, and writes linear memory                  |
| `trait` / `interface` method requirement                 | an error: the call reaches an impl, whose body states them                                                                      |
| anything with a body                                     | an error: the body states them                                                                                                  |
| [declared absence](./wep-2026-09-13-declared-absence.md) | none: it is never called                                                                                                        |

On an import, an attribute is an error. Every import of one kind has the same
answer, so it is read off the kind rather than written on each.

### `#[result]`: Where the Result's Storage Lives

`#[result(fresh)]` says the result's storage is new. `#[result(part_of = p)]`
says the result is `p`, or a part of it. The attribute repeats for a second
parameter, one name per attribute, as `#[retain]` and `#[trap]` do:

```wado
#[result(part_of = a)]
#[result(part_of = b)]
pub fn select<T>(cond: bool, a: T, b: T) -> T;
```

`fresh` stands alone.

A declaration owes one where its result type can carry storage and some
parameter can carry storage, by value or by reference. Where either side
cannot, the result is `fresh` by proof.

`owned` is renamed `fresh`. The new name speaks of the storage alone. What the
result holds is `#[retain]`'s to say.

### `#[retain]`: What the Call Keeps, and Where

`#[retain(p)]` keeps `p` itself. `#[retain(elements_of = p)]` keeps what `p`
holds. `into = q` names the parameter it lands in, and `into = result` names
the result. Without `into`, the destination is unknown.
`#[retain(none)]` says the call keeps nothing, and stands alone.

A declaration owes one where some parameter can carry storage. Otherwise
nothing can be kept, by proof.

A `fresh` result holding an argument says so with `into = result`. That
replaces the reading in which an unstated result might hold what any argument
holds.

### `#[trap]`: When the Call Traps

A body-less `core:builtin` declaration states one of these:

- `#[trap]` says it may trap.
- `#[trap(never)]` says it never traps.
- A check names the one condition it traps on, and repeats for another, as
  [the spec](./spec-attributes.md#trap) lists.

A declaration with no stated contract can only be read as one that may trap.
So "unknown" is not a separate word: it would change nothing the optimizer
does. Writing bare `#[trap]` makes that reading a decision rather than an
oversight.

### `#[linear_memory]`: How the Call Touches Linear Memory

A body-less `core:builtin` declaration states `none`, `read` or `write`.
`write` covers reading too, since a store is ordered against every other
access. A separate read-and-write would change nothing the optimizer does.

The declaration is the only source of this fact. A linear-memory address is a
plain `i32`, so no parameter type proves anything, and every declaration states
it, one by one.

## Roadmap

- [ ] `#[result]`: rename `owned` to `fresh`, accept a repeated `part_of`, and
  owe the attribute where a by-value parameter can carry storage too.
- [ ] `#[retain]`: add `none` and `into = result`, owe the attribute where a
  parameter can carry storage, and retire the reading in which an unstated
  result holds what any argument holds.
- [ ] `#[trap]`: accept bare `#[trap]`, owe one on every body-less
  `core:builtin` declaration, and audit the 60 that state none.
- [ ] `#[linear_memory]`: add `none`, and owe one on every body-less
  `core:builtin` declaration.
- [ ] Imports: report an attribute on one, and read its facts off its kind.
- [ ] Write the rules into [the spec](./spec-attributes.md#retain--result).

## Known gaps

`#[trap]` has no check for a linear-memory access out of bounds. A load or a
store traps exactly there, but it can state only bare `#[trap]`, and the
optimizer keeps every such call.

A stated fact is trusted. A `core:builtin` declaration that states less than
its lowering does miscompiles, and nothing compares the two.

An asset import's facts are read off its kind, though its body is in the binary
the compiler embeds.

A `core:builtin` carrying a canonical name, a Component Model operation such as
`stream_read`, is read as opaque whatever its attributes state.

## References

- [WEP: Value Semantics and Reference Retention](./wep-2026-01-12-value-semantics-and-retention.md)
  defines retention, the fact `#[result]` and `#[retain]` state.
- [Compiler Attributes](./spec-attributes.md#retain--result)
