# WEP: Option and Result Value Methods

## Context

`Option<T>` and `Result<T, E>` carried only methods that extract a value or
trap: `unwrap`, `expect`, and on `Result` also `is_ok`, `is_err`,
`unwrap_err`, `expect_err`. None transforms a value or supplies a fallback, so
a step that handles a failure rather than trapping on it spells both arms of a
`match` (issue #2035).

The gap is real but small, and the risk of closing it badly is larger than the
gap. Rust's own `Option` and `Result` carry methods its maintainers call
mistakes they cannot fix, so "Rust has it" is not a reason to add anything.
Two measurements bounded the set.

### The Wado corpus

The 1,388 `.wado` files under `wado-compiler/lib`, `package-*`, and `example`
hold 382 two-arm matches on an `Option` or a `Result`, classified here by the
method that would collapse each:

| Shape                                       | Option | Result |
| ------------------------------------------- | -----: | -----: |
| identity arm + plain fallback               |     30 |     34 |
| both arms multi-statement (no method fits)  |     38 |     25 |
| identity arm + diverging arm (`?`/let-else) |     22 |     69 |
| transform + plain fallback                  |     35 |      3 |
| `map` then `?`                              |     29 |      0 |
| Ok passed through, Err transformed          |      4 |     39 |
| None arm yields an `Err`                    |     20 |      – |

### Idiomatic Rust

Counted by method call over 1,313 files and 435k lines of `vendor/wasmtime`
and `vendor/wasm-tools`, the leaders are `map_err` 368, `ok_or_else` 354,
`unwrap_or` 270, `and_then` 242, `ok_or` 76, and `unwrap_or_else` 74. Against
those, `map_or` 40, `or_else` 13, `unwrap_or_default` 6, `flatten` 6, `xor` 1,
and `map_or_else` 2. The methods the community criticizes are the methods it
does not use.

## Decision

Six methods, each with the name, parameters, and meaning Rust gives it:

```wado
impl<T> Option<T> {
    pub fn unwrap_or(self, default: T) -> T;
    pub fn map<U>(self, mut f: fn mut(T) -> U) -> Option<U>;
    pub fn ok_or<E>(self, err: E) -> Result<T, E>;
}

impl<T, E> Result<T, E> {
    pub fn unwrap_or(self, default: T) -> T;
    pub fn map<U>(self, mut f: fn mut(T) -> U) -> Result<U, E>;
    pub fn map_err<F>(self, mut f: fn mut(E) -> F) -> Result<T, F>;
}
```

`ok_or` earns its place beyond its 20 sites: `?` propagates an `Option` only
in a function returning `Option`, and rejects one in a function returning
`Result`. Nothing else bridges the two.

`map_err` is not made redundant by `?` performing `From` conversion. The 39
sites that want it add call-site context, as in
`Err(e) => Err(failure(path, e))`, which a type-directed `From` impl cannot
supply.

### A Rust name keeps its Rust meaning

Wado does not offer a method that borrows a Rust name for different parameters
or different behavior. Gleam's `unwrap(option, or: default)` is a better
design read aloud, and unavailable to us: Rust's `unwrap` means something else,
and the knowledge that carries over is worth more than the improvement.

### Deliberately omitted

`map_or(default, f)` and `map_or_else(g, f)` are not offered. The argument
order is a known trap: the name reads "map, or else" while the signature takes
the fallback first. Rust's maintainers have accepted the complaint twice and
concluded it is too late to fix. Scala's identically shaped `Option.fold` is
advised against by its own community, Odersky included ("methods like cata
that take two closures as arguments are often overdoing it. Do you really gain
in readability over map + getOrElse?"). The 38 Wado sites are `map(f)`
followed by `unwrap_or(v)`, which risks no ordering at all.

`is_some`, `is_none`, and `is_some_and` are not offered: `opt matches
{ Some(_) }` covers the test, with a guard for the predicate form.

`and`, `or`, `xor`, `filter`, `zip`, `flatten`, and `unwrap_or_default` are
not offered. No Wado site wants them, and idiomatic Rust barely does.

A `??` operator is not adopted. The case for it was real: one infix operator
covers `unwrap_or`'s 64 sites with a right-hand side lazy by construction, and
Zig, Swift, Kotlin, and C# all settled on that shape. It is refused because an
operator is a large addition for a small return — grammar, precedence,
formatter, and language service all grow, and what they buy is laziness at one
call shape. It also stops at `Result`, where it would drop `E` without saying
so. Whatever answers the eager fallback answers it as a method.

`map_or` and `map_or_else` carry `#[unavailable]`, so calling either reports
the reason above; see
[WEP: Declared Absence](./wep-2026-09-13-declared-absence.md). The rest are
carried in prose, since no caller reaching for them writes a Rust name Wado
answers differently.

`unwrap_or_else`, `ok_or_else`, and `and_then` are absent rather than
declined, so nothing marks them. Known gaps says what each waits on.

## Roadmap

Nothing is queued. The gaps below are unowned.

## Known gaps

- `unwrap_or_else` and `ok_or_else` are absent. Each is the lazy form of a
  method that is offered, and with `??` refused nothing else keeps a fallback
  from being evaluated where it goes unused. The corpus splits them. Thirteen
  sites build an error the success path never reads — `DeserializeError`, a
  formatted message — which `ok_or` would evaluate and `ok_or_else` would not.
  About five want `unwrap_or_else`, and most of those are a `String::new()` or
  a `TreeMap::new()` that costs nothing to evaluate early. Idiomatic Rust
  keeps that ratio: `ok_or_else` 354 calls to `unwrap_or_else`'s 74. Adopting
  one is adopting both, since the argument for each is the same.
- A fallback that reads the `Err` payload without diverging has no
  non-`match` form, since `let ... else` cannot bind the payload. There are 15
  such sites, and `Result::unwrap_or_else` is what would cover them.
- `and_then` is absent. `?` covers it, including propagating an `Option` in an
  `Option`-returning function, and no Wado site wants it. Idiomatic Rust calls
  it 242 times in the measured corpus, so this is the kind of gap that shows
  up later rather than never.
