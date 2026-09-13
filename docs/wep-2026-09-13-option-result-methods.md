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
Two measurements decided the set.

**The Wado corpus.** Every `.wado` file under `wado-compiler/lib`,
`package-*`, and `example` — 1,388 files — yields 382 two-arm matches on an
`Option` or a `Result`, classified by the method that would collapse each:

| Shape                                       | Option | Result |
| ------------------------------------------- | -----: | -----: |
| identity arm + plain fallback               |     30 |     34 |
| both arms multi-statement (no method fits)  |     38 |     25 |
| identity arm + diverging arm (`?`/let-else) |     22 |     69 |
| transform + plain fallback                  |     35 |      3 |
| `map` then `?`                              |     29 |      0 |
| Ok passed through, Err transformed          |      4 |     39 |
| None arm yields an `Err`                    |     20 |      – |

**Idiomatic Rust.** 1,313 files and 435k lines of `vendor/wasmtime` and
`vendor/wasm-tools`, counted by method call: `map_err` 368, `ok_or_else` 354,
`unwrap_or` 270, `and_then` 242, `ok_or` 76, `unwrap_or_else` 74 — against
`map_or` 40, `or_else` 13, `unwrap_or_default` 6, `flatten` 6, `xor` 1, and
`map_or_else` **2**. The methods the community criticizes are the methods it
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
`Result` ("cannot use ? on Option in a function returning Result"). Nothing
else bridges the two.

`map_err` is not made redundant by `?` performing `From` conversion. The 39
sites that want it add call-site context — `Err(e) => Err(failure(path, e))` —
which a type-directed `From` impl cannot supply.

### A Rust name keeps its Rust meaning

Wado does not offer a method that borrows a Rust name for different parameters
or different behavior. Gleam's `unwrap(option, or: default)` is a better
design read aloud, and unavailable to us: Rust's `unwrap` means something else
and the knowledge that carries over is worth more than the improvement.

Where Wado refuses a name, the refusal is written at the declaration with
`#[not_provided("...")]` rather than left to this document — see
[WEP: Declared Absence](./wep-2026-09-13-declared-absence.md).

### Not provided

- **`map_or(default, f)` / `map_or_else(g, f)`.** The argument order is a
  known trap: the name reads "map, or else", the signature takes the fallback
  first. Rust's maintainers have accepted the complaint twice and concluded it
  is too late to fix. Scala's identically shaped `Option.fold` is advised
  against by its own community, Odersky included ("methods like cata that take
  two closures as arguments are often overdoing it. Do you really gain in
  readability over map + getOrElse?"). The 38 Wado sites are `map(f)` followed
  by `unwrap_or(v)`, which risks no ordering at all.
- **`is_some` / `is_none` / `is_some_and`.** `opt matches { Some(_) }` covers
  the test, with a guard for the predicate form.
- **`and` / `or` / `xor` / `filter` / `zip` / `flatten` /
  `unwrap_or_default`.** No Wado site wants them, and idiomatic Rust barely
  does.

### Deferred, not refused

`unwrap_or_else`, `ok_or_else`, and `and_then` are simply absent for now; they
carry no `#[not_provided]`, which records a decision rather than a pause.

- The lazy fallbacks wait on a `??` operator. One infix operator would cover
  `unwrap_or`'s 64 sites with a right-hand side that is lazy by construction,
  taking the eager-evaluation trap out of the language rather than teaching
  each caller to dodge it — the shape Zig, Swift, Kotlin, and C# all settled
  on. It would also make `unwrap_or_else` unnecessary. The open question is
  `Result`, where `??` would silently drop `E`.
- `and_then` is covered by `?`, which does propagate an `Option` in an
  `Option`-returning function. Rust calls it 242 times in the measured corpus;
  Wado wants it nowhere yet.

One shape has no non-`match` form under this set and is left as-is: a fallback
that reads the `Err` payload without diverging. `let ... else` cannot bind it,
and `unwrap_or_else` is deferred. There are 15 such sites.

## Alternatives considered

**Add Rust's surface wholesale.** The measurements say most of it is unused
even in Rust, and the parts that are heavily used are the six above plus the
lazy twins.

**Add nothing; the language covers it.** `?`, `let ... else`, `if let`,
`matches`, and labeled blocks do cover every arm that diverges — 91 of the 382
sites. They do not cover an arm that produces a value, which is 141 of them.
