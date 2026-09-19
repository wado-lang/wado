# WEP: core:prng

## Context

Wado has the pieces of a random-number story and no story. `wasi:random` is the
raw capability, `core:secure_random` buffers it, and `example/prng_bench.wado`
holds four engines written to be timed rather than used. Nothing says which of
them a program reaches for, what a seed is, or what a consumer may rely on
across a release.

Three separate demands pull the design apart, and a single trait serves none of
them well.

A **stream** consumer asks for the next value. It holds state, draws one word at
a time, and wants the draw to cost about what an add costs. A **lane-parallel**
consumer asks for many values at once; it has no use for one word and every use
for sixteen, and the whole point is that eight lanes advance in the same
instructions one would. A **keyed** consumer asks a different question
altogether: not "the next value" but "the value at this key". Terrain
regenerated on a revisit and an entity keyed by its id both need the draw order
to belong to the caller, and need no two callers to agree on who draws first.

The effect system is the fourth pressure. An operation that declares `with
Random` forces the declaration up through every caller, and a simulation whose
inner loop draws a number should not carry a capability through forty
signatures to do it. But randomness genuinely is a capability at the point where
entropy enters the process, and pretending otherwise would be the wrong kind of
convenient.

Measurements below are `example/prng_bench.wado` at `-O3`, medians of three
passes on one machine, in u64 per second. They are a ranking, not a
specification.

|              | scalar | vector |
| ------------ | -----: | -----: |
| RomuTrio     |  398 M |      — |
| xoshiro256++ |  227 M |  693 M |
| SHISHUA      |  155 M | 1.62 G |

## Decision

`core:prng` offers three layers that do not mix: a scalar engine, a vector
engine, and a keyed function. Each is a separate trait, because the thing each
one generates is a different thing. `core:secure_random` keeps the entropy, and
so keeps the only effect.

### Two traits, because a lane is not a draw

```wado
pub trait Rng with () {
    fn next_u64(&mut self) -> u64;
}

pub trait VectorRng with () {
    fn next_batch(&mut self) -> [u64x2, u64x2, u64x2, u64x2, u64x2, u64x2, u64x2, u64x2];
}
```

A unified trait would have to name one width. Named one, the eight-lane engine
spends a cursor and a branch per word to hand out a single u64, and the
measurement is what that costs: SHISHUA falls from 1.62 G handing out a round to
155 M handing out a word, an order of magnitude, all of it spent in the API
shape. Named sixteen, the scalar engine has to buffer, and a consumer that wants
one number pays for fifteen it will not use.

So the traits are separate, and an engine implements whichever it can serve.
xoshiro256++ serves both — its state transition is vertical, so eight lanes need
no permute. SHISHUA serves only the vector trait; its round is eight vectors
wide and nothing smaller exists inside it.

`with ()` is not decoration. It is the rule from `AGENTS.md` applied: every
standard library trait declares it, and these three say in the type system that
no implementation of them may perform I/O.

### The batch is sixteen u64, fixed by the library

The vector trait returns eight `u64x2` and not a width the engine chooses.
Both engines reach sixteen words in a whole number of their own steps — two of
xoshiro's four-vector step, one of SHISHUA's eight-vector round — so neither
keeps a cursor, and the batch crosses the trait boundary in registers: the
return is a tuple the call site destructures, and nothing is allocated for it.
Measured through the trait, SHISHUA gives 1.55 G and xoshiro ×8 gives 703 M,
which is what each gives with no trait at all.

A per-engine width would be the more general design and would cost the
generality: a consumer could not be written against it without either a cursor
or a type-level width, and the cursor is the thing measured above at an order of
magnitude.

### The vector trait generates and nothing more

No derived layer sits on `VectorRng`. Bounded integers need a rejection loop,
which is a branch per lane and therefore not a vector operation; `[0,1)` floats
need a u64→f64 lane convert, which Wasm SIMD does not have. Both would be
scalar code wearing a vector signature. A consumer that wants sixteen bounded
integers spills the batch and uses the scalar layer, and pays the spill
knowingly.

### The derived layer is uniform only

On `Rng`, as defaulted methods, five of them:

|                       |                                                  |
| --------------------- | ------------------------------------------------ |
| an integer in a range | Lemire's multiply-shift, with the rare rejection |
| an f64 in `[0,1)`     | 53 bits, one shift and one multiply              |
| a bool                | one bit                                          |
| shuffle a `List`      | Fisher–Yates over the bounded integer            |
| choose from a `List`  | `Option<T>`, empty list gives `None`             |

Every one of these is arithmetic on a draw, with no table and no state of its
own. That is the line, and it is drawn at **uniform**: a shape other than
uniform is a distribution and does not live here.

"An integer in a range" is one operation over eight widths and two range types,
so it reaches them through two traits rather than eight overloads:
`SampleUniform` is what an integer type implements, `SampleRange` what `..<` and
`..=` implement over one, and `random_range` is the defaulted method that puts
them together. Both are public, because a caller writing a bound over "whatever
`random_range` accepts" needs to name them.

`choose` hands back a value and not a reference because value semantics leave it
no choice: `&T` requires `T: Ref`, which no primitive is, and a list of numbers
is the case the operation exists for. `TreeMap::get` answers the same way for
the same reason.

The excluded case that makes the line worth stating is the normal distribution.
Its good implementation is the Ziggurat, which carries two precomputed tables of
128 or 256 f64 each — two to four kilobytes present in every component that
links `core:prng`, drawn or not. The table-free alternative, Box–Muller, calls
`sqrt`, `ln` and `sin` and is several times slower. Rust draws the same line for
the same reason, keeping `Uniform` in `rand` and `Normal` in `rand_distr`; Go
puts `NormFloat64` in its standard library and does not measure the size of what
it ships.

### Keyed randomness is Squares64, and has no vector form

```wado
pub struct Squares64 { /* one u64 key, private */ }

impl Squares64 {
    pub fn from_seed(seed: Seed) -> Squares64;
    pub fn at(&self, counter: u64) -> u64;
}
```

Squares (Widynski, arXiv:2004.06278) applies five rounds of the middle-square
transform to `counter * key`. Three candidates were implemented and measured
against each other, each in this shape and each checked lane-for-lane against
its own scalar form:

|                  | scalar | vector | two coordinates |
| ---------------- | -----: | -----: | --------------: |
| Squares64        |  379 M |  254 M |       **335 M** |
| Threefry-2x64-13 |  391 M |  345 M |           189 M |
| Philox4x32-10    |  111 M |  121 M |            60 M |

The third column folds two coordinates into the counter with odd constants
before the transform, so what it times is the terrain shape and not a bare
counter walk. Two results decided it.

#### No vector form is worth having

The two candidates worth shipping are slower in the vector column than in the
scalar one, and the third gains 9% and is still the slowest of the three in both
columns. A keyed round is a serial dependency chain, so widening it multiplies
the data without shortening the chain. The scalar instruction set then wins
every exchange that matters. x86 returns both halves of a 64×64 multiply in one
`mulq` and rotates in one `rolq`. Wasm's `i64x2.mul` has no x86 instruction
below AVX512DQ, there is no vector rotate, and the high half of a 32×32 product
takes two widening multiplies and a shuffle. The published rankings, all of them
taken on scalar or GPU hardware, invert here.

#### One u64 per call is the shape the consumer has

Threefry leads on a counter drain and loses by 1.8× on the terrain shape,
because it emits two words per call and a consumer asking for the value at one
coordinate throws one away. Squares emits exactly one. Philox is last in every
column, by 2.1× to 5.6×, and is out.

#### The key is derived, not taken

Squares' output quality depends on its key in a way a block cipher's does not:
the key 0 yields all zeros, and a small integer key yields roughly 2^16/k zeros
before the output becomes usable. `from_seed` therefore does not take the
seed's word as the key. It mixes, tests the result against the key predicate,
and mixes again until it passes — deterministic, and in practice one round. The
predicate is an assert, not a comment, and so is the loop bound.

### A seed is a value, so generation is pure

```wado
pub struct Seed { /* 256 bits, private */ }

impl Seed {
    pub fn from_u64(n: u64) -> Seed;
    pub fn from_bytes<B: AsByteSlice>(bytes: &B) -> Seed;
    pub fn from_str(s: &String) -> Seed;
    pub fn split(&self, index: u64) -> Seed;
}

pub trait Seedable with () {
    fn from_seed(seed: Seed) -> Self;
}
```

Nothing in `core:prng` declares an effect, and nothing in it can: a `Seed` is
data, and every constructor of one is arithmetic. Two hundred and fifty-six bits
is xoshiro's whole state and SHISHUA's own seed, which SHISHUA expands into its
1024-bit state, and is the width Go's ChaCha8Rand and Rust's `ChaCha8Rng`
already take.

`from_u64` expands through SplitMix64, so `Seed::from_u64(0)` is as good a seed
as any other — the property NumPy's `SeedSequence` exists to provide, and the
reason a user may pass 1, 2, 3 as world seeds without the streams correlating.
`from_str` is there because the first consumer names its world.

Entropy lives one module away:

```wado
// core:secure_random
pub fn seed() -> Seed with Random;
```

`core:secure_random` depends on `core:prng` for the type; the dependency does
not run the other way. A program that wants its run to differ from the last
writes one line that carries the effect and none after it:

```wado
use { seed } from "core:secure_random";
use { Xoshiro256pp, Seed } from "core:prng";

export fn run() with (Stdout, Random) {
    let mut rng = Xoshiro256pp::from_seed(seed());
    println(`${rng.random_range(1..=6)}`);

    let fixed = Xoshiro256pp::from_seed(Seed::from_u64(42));
}
```

The reproducible construction on the last line declares nothing, which is the
whole point: a test, a replay and a fixture pay no capability at all.

### Nothing in `core:prng` is cryptographic

xoshiro256++, SHISHUA and Squares64 are simulation generators. Each is built to
pass statistical tests at the least work per word, and none of them is designed
to resist an adversary who has seen its output: xoshiro's state transition is
linear and invertible, and five rounds of middle-square arithmetic are far short
of a block cipher's. A token, a session identifier, a nonce, a password salt or
a key therefore comes from `core:secure_random`, which draws from `wasi:random`
and carries `Random` to say where it came from. `core:prng` is for simulation,
sampling, procedural generation, randomized algorithms and tests.

The boundary is one-way, and `seed()` is the crossing: entropy may start a fast
stream, and no output of that stream goes back to a caller asking for a secret.
No type enforces the direction — a `Seed` is the same value on both sides — so
what marks it is which module a value was asked from, and the `Random` effect
that asking one of them carries.

### Parallel streams: two mechanisms, kept apart

`Seed::split(index)` derives a child seed and works for every engine. It is
pure and takes the index rather than keeping a counter, so worker _n_ derives
its own seed with no coordination and no agreement on who split first — JAX's
`fold_in` rather than NumPy's `spawn`, for the same reason the keyed layer
exists. Independence is statistical.

`Xoshiro256pp::jump()` advances 2^128 steps over the same linear state
transition a step advances one over, so streams a jump apart are disjoint for
2^128 draws. Independence is proven.

`jump` stays an inherent method on the one type that can offer it. Lifting it to
a trait would oblige SHISHUA to supply a version that proves nothing, and the
two guarantees would become one word.

### Streams are not promised

An algorithm named in a type is fixed: `Xoshiro256pp` emits what xoshiro256++
emits, for a given state, forever. Everything else is unpromised — the derived
layer's consumption pattern, how a `Seed` becomes a state, what `split` derives,
which engine an unnamed default resolves to. Two runs of one binary agree;
two versions of `core:prng` need not.

This is Go's policy rather than NumPy's. NumPy froze `RandomState` bit for bit
and has been unable to fix it since (NEP 19); Go names the algorithm in the type
and leaves the rest free, which is the only arrangement under which a bug in
the derived layer can be fixed.

The policy is open for reconsideration at 1.0.0, where a stronger promise costs
something real and may be worth it.

## Known gaps

- The names are Rust's (`random_range`, `random_bool`, `shuffle`, `choose`),
  which is what the library spells. Ratifying that house, or moving to Go's
  (`IntN`, `Float64`), is still open; a mixture is worse than either.
- The derived layer's exact roster is the five operations and no more. Whether
  `next_u32` joins them — the high half of a draw, free to provide and one more
  name to keep — is open.
- A caller folding coordinates into the counter does it by hand. The fold is
  the part that goes wrong (`x * 31 + y` collides along a diagonal) and the
  measurement above is of a folded form, so the operation is real; whether it
  is a method on `Squares64`, a free function, or left to the caller is not
  decided.
- The exact key predicate `Squares64::from_seed` establishes is not settled.
  Widynski's `keys.h` generator draws sixteen nonzero hexadecimal digits with no
  two adjacent alike and an odd lowest digit; reproducing that at bit level
  would reject two thirds of what it is given, so the library rejects the
  failure mode instead — a low nibble of zero, a zero upper half, or more than
  four zero nibbles. Closing it means choosing the predicate against that
  generator and stating it as the assert.
- No known-answer vectors are checked anywhere. Each vector form is checked
  against its own scalar form, which catches a transcription error but not a
  wrong constant shared by both. SHISHUA is the widest case: the library
  expands the seed through SplitMix64 and keeps the reference's thirteen
  discarded rounds rather than its table of digits of phi, so its stream is not
  the reference's. Closing it means a fixture per algorithm against the
  reference implementation's published output, and for SHISHUA the reference's
  own seeding first.
- A generic consumer over `R: VectorRng<Batch = [..V]>, ..V: BitXor<u64x2,
  Output = u64x2>` compiles and runs, so the fixed sixteen-word batch is a
  choice rather than a workaround: what it costs is a width the library names
  instead of the engine, and the measurements above are why it is named.
  Reaching a member of the batch still puts the pack on the left of the
  operator (`v ^ acc`, never `acc ^ v`), as it would in Rust.
- SHISHUA has no jump and no proven stream separation, so its lanes rest on
  seeding alone. Closing it is not possible within the algorithm; the honest
  answer is that a consumer needing proven disjointness uses xoshiro.
- Distributions and noise functions have no home yet. Both sit on this library
  and neither belongs inside it.
