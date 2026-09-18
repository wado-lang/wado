# WEP: Randomness (`core:prng`)

## Context

Randomness in Wado today is three fragments and no design.

| fragment                                                                  | what it is                                                           |
| ------------------------------------------------------------------------- | -------------------------------------------------------------------- |
| `wasi:random` (`Random`, `Insecure`, `InsecureSeed`)                      | the generated host interfaces, one effect each                       |
| `core:secure_random`                                                      | `BufferedRandom`, a `Random` handler that draws 4096 bytes at a time |
| `example/prng_bench.wado`, `example/arbitrary.wado`, `benchmark/microgpt` | six hand-written generators                                          |

SplitMix64 is written out three times — as `SplitMix64` in `prng_bench`, as
`Gen` in `arbitrary`, as `Rng` in `microgpt` — and each copy grows the derived
draws its own consumer needed: `below` and `bool` in one, `random`, `gauss` and
`choice` in another, `pair` in the third. The engine is not the duplicated part.
The layer above it is.

`core:uuid` is the only stdlib consumer of host entropy, and it carries its own
short-read loop (`fill_random`) to do it.

The open question is not which algorithm. It is where the effect goes. An
effect on every draw is the propagation problem Koka users report, and
[WEP: Effect System and Randomness in Collections](./wep-2026-01-20-effect-system-randomness.md)
already answered one instance of it for `HashMap` with `#[benign]`. A general
answer is cheaper than a second carve-out.

## Measurements

Per-draw throughput of one `u64`, `-O3`, wasmtime, one machine, two runs
(run-to-run spread is about 10%).

| source                                       | state       | M u64/s | vs. a host call |
| -------------------------------------------- | ----------- | ------: | --------------: |
| `wasi:random/random`, one host call a draw   | —           |     1.2 |              1× |
| `wasi:random/insecure`, one host call a draw | —           |     1.3 |            1.1× |
| ChaCha8 in the guest, scalar                 | 32-byte key |      53 |             44× |
| xoshiro256++ behind `&mut`                   | 4 × u64     |     175 |            146× |
| PCG-XSH-RR 64/32 behind `&mut`               | 1 × u64     |     205 |            171× |
| RomuTrio behind `&mut`                       | 3 × u64     |     330 |            275× |
| SplitMix64 behind `&mut`                     | 1 × u64     |     390 |            325× |
| xoshiro256++, state in four locals           | 4 × u64     |     548 |            457× |
| RomuTrio, engine a local                     | 3 × u64     |     782 |            650× |

From `example/prng_bench.wado`, over `core:simd`: xoshiro256++ across eight
lanes reaches 646 M u64/s, and SHISHUA consumed a round at a time reaches
1.42 G u64/s.

Four things follow.

### A host call is all cost

`insecure` is not measurably faster than `random` (1.3 against 1.2), because
the algorithm behind the boundary is noise next to the crossing. Any guest
engine beats both by two orders of magnitude. 870 ns for one `u64` is also
worth a look on its own; it is not the price the Component Model should charge
for a call returning a scalar.

### Secure randomness belongs in the guest too

A scalar ChaCha8 keyed once from the host is 44× faster than asking the host
per draw, and Go made exactly this trade for `math/rand/v2`'s global source.

### The trait costs nothing

The same loop through a `R: Rng` bound and through a concrete method measures
170 and 177 M u64/s; `next_below(6)` and `next_f64()` through default methods
measure 184 and 158. All inside the noise. `prng_bench` measures the same for
a round-at-a-time trait: 1.39 against 1.42 G u64/s.

### The engine ranking is an optimizer artifact, not an algorithm fact

xoshiro256++ with its state hand-scalarized into four locals computes the
identical stream (same checksum) 3.2× faster than the same engine as a struct
local: 548 against 173. The chain behind it is visible in NIR and in
`WADO_TRACE=sroa`. RomuTrio's `next_u64` is inlined, so nothing holds its
state and `sroa` decomposes it into `$sroa_rng_x/y/z` (2.4× faster);
SplitMix64's one word goes the same way (1.5×). `Xoshiro256pp::draw` stays out
of line, so the `&mut self` it takes escapes the local, and `sroa` declines it
— the trace says `escaped, not soft`. So it is the inline decision that
decides whether an engine's state lives in locals or in struct fields, and the
ranking above is an artifact of where that decision fell. The engines are
therefore proposed on merit, with the gap filed rather than designed around.

## Decision

Proposed, not settled: the calls this leaves open are listed under Known gaps.

### The seam is reproducibility, not security

```
core:prng            seeded, reproducible, effect-free. Engines and draws.
core:secure_random   unpredictable, host-keyed. One capability, one door.
```

A cryptographic engine is a seeded engine like any other — ChaCha8 lives in
`core:prng`. What `core:secure_random` owns is not an algorithm but the
crossing to `wasi:random`.

### The effect appears once, at the seed

```wado
let mut rng = Xoshiro256pp::seed(42);         // pure, reproducible
let mut rng = Xoshiro256pp::from_entropy();   // `with Random` — the only effectful line
rng.next_below(6);                            // pure, from here on forever
```

Entropy is a capability; generation is arithmetic. Nothing downstream of the
seed declares an effect, installs a handler, or is polymorphic in one. Every
`core:prng` trait says `with ()`, as every standard library trait does, and the
`#[benign]` question does not arise.

### A PRNG is a value because streams are plural

The deeper reason not to make generation an operation of an `interface`: an
effect has one handler per `with` scope, so it can only ever name one stream. A
generator's state is the point of it — held, copied, forked, snapshotted,
compared in a test, carried in a struct field. Two independent streams in one
function is ordinary code with values and impossible with a handler.

That is the rule for the rest of the library too: an ambient singleton is an
effect; a plural thing is a value. Host entropy is one shared, unforgeable
tap, so `Random` is rightly an effect. Streams are plural, so engines are
values.

### `Rng`: one required operation, everything else defaulted

Go's `math/rand/v2` replaced `Int63` with `Uint64` and dropped `Seed` from the
`Source` interface, on the grounds that a shortened return and a fixed seed
type were both wrong. Wado starts there.

```wado
pub trait Rng with () {
    fn next_u64(&mut self) -> u64;

    fn next_u32(&mut self) -> u32 { ... }             // the high 32 bits
    fn next_bool(&mut self) -> bool { ... }
    fn next_f64(&mut self) -> f64 { ... }             // [0, 1), 53 bits
    fn next_below(&mut self, n: i32) -> i32 { ... }   // Lemire, over 32 bits
    fn next_in(&mut self, range: RangeInclusive<i32>) -> i32 { ... }
    fn fill_bytes(&mut self, out: &mut ByteList) { ... }
    fn shuffle<T>(&mut self, xs: &mut List<T>) { ... }
    fn choose<T>(&mut self, xs: &List<T>) -> Option<T> { ... }
    fn fork<R: Seedable>(&mut self) -> R { ... }      // R::seed(self.next_u64())
}

pub trait Seedable with () {
    fn seed(n: u64) -> Self;
}
```

`next_below` is Lemire's nearly divisionless method over 32 bits, so it needs
no 128-bit multiply — `u128` is a GC type here, and a 32-bit bound covers every
`i32` range. The measurements above are of this exact shape, including the
generic default `shuffle<T>`: the whole trait type-checks and runs today
(spike, four tests).

Seeding stays a one-`u64` entry point rather than an associated `Seed` type.
Every duplicated copy in the repository already takes a `u64`, and an engine
that wants its exact state takes it through a static of its own
(`ChaCha8::from_key`), outside the trait.

### Engines are named by their algorithm

No `Prng` alias, no `DefaultRng`. A seeded engine's stream _is_ its contract —
fixtures and reproducible builds depend on it — so a name that could change
algorithm would be a lie. This is Go's reasoning for `PCG` and `ChaCha8` over
`NewSource`, and it means a better engine arrives as a new name, never as a
changed stream.

| engine         | words    | for                                                                                 |
| -------------- | -------- | ----------------------------------------------------------------------------------- |
| `SplitMix64`   | 1        | seeding and forking; the cheapest stream there is                                   |
| `Xoshiro256pp` | 4        | the general recommendation: `jump()` / `long_jump()` give provably disjoint streams |
| `ChaCha8`      | 32 B key | prediction-resistant, rekeying every 16 blocks for forward secrecy                  |

RomuTrio is not offered despite winning on speed: it has no jump function, so
it cannot answer "give me a second stream that provably never overlaps this
one", which is the question a parallel or sharded consumer actually asks.
SHISHUA and the eight-lane SIMD xoshiro stay in `example/prng_bench.wado` until
a bound can constrain the members of a type pack — the file's own comment
records what does not parse.

### A copy forks the stream

Wado has value semantics, so `let b = a;` on an engine duplicates its state and
two identical streams walk away. Rust 0.10 removed `Clone` from `StdRng` and
the ChaCha generators for exactly this hazard; Wado cannot remove a copy. So:

- Every draw takes `&mut self`, and every helper takes `&mut R`. No API takes
  an engine by value except a constructor.
- The intended fork is spelled: `fork()` derives a child from a drawn seed, and
  `Xoshiro256pp::jump()` advances 2^128 steps for a stream proven disjoint.
  `prng_bench`'s copy-then-jump is the legitimate case and reads as one.
- `fork()` carries 64 bits into a 256-bit state, so thousands of forks are fine
  and 2^32 of them are not. Many streams want `jump()`.
- The security case is the shaped one: a duplicated ChaCha8 keystream is a bug,
  and effect-installed handlers pass `&mut h`, which shares rather than copies.
  Secure randomness therefore stays effect-shaped and fast randomness
  value-shaped — each half takes the shape that defeats its own hazard.
- A lint (`rng_copied`, on a by-value binding or argument of an `Rng` type)
  belongs in [WEP: wado lint](./wep-2026-08-31-wado-lint.md).

### Three host interfaces, two of them used

| interface      | stdlib use                                                                                                                                              |
| -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Random`       | `core:secure_random`: keys, tokens, `Uuid::v4`, and keying `ChaCha8`                                                                                    |
| `InsecureSeed` | `HashMap` seeding under `#[benign]`, and `from_insecure_seed()` for jitter and backoff, where unpredictability is wanted but a CSPRNG capability is not |
| `Insecure`     | nothing                                                                                                                                                 |

`Insecure` earns no stdlib API: measured, it is a host call like any other
(1.3 M u64/s), and it is neither reproducible nor secure — the two reasons to
draw at all. The generated binding stays for completeness.

### `core:secure_random` becomes a door, not a buffer

`BufferedRandom` holds up to 4096 bytes of _future_ entropy in guest memory,
which is the wrong thing to hold: a memory disclosure exposes randomness not
yet handed out. A ChaCha8 keyed once and rekeyed from its own output holds 32
bytes, exposes only recent output, makes exactly one host call ever, and is 44×
faster per draw.

```wado
pub fn secure_rng() -> ChaCha8 with Random;   // keyed from the host, once

pub struct SecureRandom { ... }               // impl Random for SecureRandom
```

`SecureRandom` installs as a `Random` handler, so existing `with Random`
consumers keep working and get the expander underneath. `BufferedRandom` is
replaced rather than kept beside it; its only users are its own tests and one
doc line in `core:uuid`.

The WIT contract asks for data "at least as cryptographically secure and fast
as an adequately seeded CSPRNG", which is what the expander is by
construction.

### Determinism under test, for the effectful half too

A seeded engine installed as a `Random` handler makes every effect-based
consumer reproducible:

```wado
let mut fake = SeededRandom::seed(1);
with Random => &mut fake do {
    assert Uuid::v4() == Uuid::from_str("...").unwrap();
}
```

This is the payoff of keeping `Random` an effect, and it is test-only: it
defeats the security of anything that draws a key. It lives in `core:prng`
under a name that says so.

## Roadmap

Ordered, and each entry finishes when what it names exists with tests. The
list starts once the design above is adopted.

1. `core:prng` with `Rng`, `Seedable`, `SplitMix64` and `Xoshiro256pp`
   (`jump` / `long_jump`): done when the derived draws, `shuffle`, `choose`
   and `fork` are there, and the jump matches the reference's polynomials
   under the commute test `prng_bench` already carries.
2. `ChaCha8` with rekeying: done when the block function passes the RFC 8439
   vectors and the rekeying interval is asserted.
3. `core:secure_random` reduced to a door: done when `secure_rng()` and
   `SecureRandom` exist, `BufferedRandom` is gone, and `core:uuid`'s doc line
   points at the expander.
4. `SeededRandom` plus `Uuid::v4_from(&mut R)`: done when a `Uuid::v4` under a
   seeded handler is asserted equal to a fixed value.
5. Deduplicate the three SplitMix64 copies: done when `microgpt`, `arbitrary`
   and `prng_bench` draw through `core:prng` and their outputs are unchanged.
6. Track the numbers: done when `prng_bench` lives under `benchmark/prng/` and
   `mise run benchmark-all` reports it.
7. File what the measurements exposed: done when the un-inlined draw that
   costs an engine its scalarized state (3.2×) and the ~870 ns host call are
   issues with the reproductions attached. Raising the inline budget is not
   the reproduction — at `--optimize-inline-threshold 400
   --optimize-inline-growth 500` the spike did not finish compiling in 15
   minutes, which is a second thing to look at.

## Known gaps

The calls this WEP does not make:

- The recommended engine. `Xoshiro256pp` is proposed for its jump functions,
  and it is half the speed of `RomuTrio` until its draw is inlined and its
  state scalarized. Choosing speed now would mean a stream that changes name
  later.
- Whether `BufferedRandom` goes at once or lives beside the expander through a
  deprecation. Its only users are its own tests and one doc line.
- Whether `core:distribution` (normal, exponential, weighted choice) waits for
  a second consumer. `microgpt`'s `gauss` is the only one today.

What is missing either way:

- `choose` returns `Option<T>` by copy; a view would want `Option<&T>`, which
  needs the iterator reference model to reach a `List` element.
- `next_in` covers `i32` only. A range generic over the integer types wants a
  `Sample` trait whose bound the compiler can express; `f64` and `char` ranges
  follow it.
- `fork` gives no tree-structured guarantee, so independence across many forks
  rests on 64 bits. Closing it means NumPy's `SeedSequence.spawn`.
- Bulk vector engines (SHISHUA, eight-lane xoshiro) cannot be written
  generically: a bound takes only `Name = Type`, so nothing constrains the
  members of a type pack. `prng_bench` records what does not parse.
- A global auto-seeded generator is deliberately absent — offering none keeps
  the misuse Go inherited from arising. A `global mut` engine cannot seed from
  entropy either, since an initializer may declare no effect, so a program
  wanting one holds it in a parameter or reseeds inside `run`.

## References

- [Evolving the Go Standard Library with math/rand/v2](https://go.dev/blog/randv2) — `Uint64` over `Int63`, seeding out of the interface, algorithm names over `NewSource`.
- [Secure Randomness in Go 1.22](https://go.dev/blog/chacha8rand) — ChaCha8Rand: 32-byte key, rekeying for forward secrecy, secure-by-default as the argument.
- [The Rust Rand Book: updating to 0.10](https://rust-random.github.io/book/update-0.10.html) and [to 0.9](https://rust-random.github.io/book/update-0.9.html) — the fallible/infallible split, and `Clone` removed from the CSPRNGs to stop keystream duplication.
- [NumPy: parallel random number generation](https://numpy.org/doc/stable/reference/random/parallel.html) — `SeedSequence.spawn` for reproducible independent streams.
- [Lemire, nearly divisionless random integer generation](https://lemire.me/blog/2019/06/06/nearly-divisionless-random-integer-generation-on-various-systems/), and [Swift's division-free successor](https://github.com/swiftlang/swift/pull/39143).
- [xoshiro / xoroshiro generators](https://prng.di.unimi.it/) — jump polynomials and equidistribution.
- [The Koka experience](https://zephyrtronium.github.io/articles/koka-experience.html) — what an effect on every draw costs, and why the seam is at the seed.
- [WEP: Effect System and Randomness in Collections](./wep-2026-01-20-effect-system-randomness.md), [WEP: Effect System Design](./wep-2026-01-27-effect-system-design.md), [WEP: Effect Handler](./wep-2026-04-11-effect-handler.md).
