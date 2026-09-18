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
keeps a cursor, and the tuple return stays in registers (`multi_value_return`
flattens a tuple return whose call site destructures). Measured through the
trait, SHISHUA gives 1.55 G and xoshiro ×8 gives 703 M, which is what each gives
with no trait at all.

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

On `Rng`, as defaulted methods:

|                       |                                                  |
| --------------------- | ------------------------------------------------ |
| `next_u32()`          | the high half of a draw                          |
| `random_range(0..<n)` | Lemire's multiply-shift, with the rare rejection |
| `random_f64()`        | 53 bits, one shift and one multiply              |
| `random_bool()`       | one bit                                          |
| `shuffle(&mut list)`  | Fisher–Yates over `random_range`                 |
| `choose(&list)`       | `Option<&T>`, empty list gives `None`            |

Every one of these is arithmetic on a draw, with no table and no state of its
own. That is the line, and it is drawn at **uniform**: a shape other than
uniform is a distribution and does not live here.

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
    pub fn at_xy(&self, x: i64, y: i64) -> u64;
    pub fn at_xyz(&self, x: i64, y: i64, z: i64) -> u64;
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

Two results decided it.

**No vector form is worth having.** All three are at or below their own scalar
form. A keyed round is a serial dependency chain, so widening it multiplies the
data without shortening the chain, and the scalar instruction set wins every
exchange that matters: x86 returns both halves of a 64×64 multiply in one
`mulq` and rotates in one `rolq`, while Wasm's `i64x2.mul` has no x86
instruction below AVX512DQ, there is no vector rotate, and the high half of a
32×32 product takes two widening multiplies and a shuffle. The published
rankings, all of them taken on scalar or GPU hardware, invert here.

**One u64 per call is the shape the consumer has.** Threefry leads on a counter
drain and loses by 1.8× on the terrain shape, because it emits two words per
call and a consumer asking for the value at one coordinate throws one away.
Squares emits exactly one. Philox is 3.5× behind on every axis and is out.

`at_xy` and `at_xyz` fold their coordinates with odd constants before the
transform. They exist because the fold is the part a caller gets wrong — `x * 31

- y` collides along a diagonal — and because the measurement above is of the
  folded form, so nothing is claimed that was not timed.

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
    pub fn from_bytes(bytes: &ByteList) -> Seed;
    pub fn from_str(s: &String) -> Seed;
    pub fn split(&self, index: u64) -> Seed;
}

pub trait Seedable with () {
    fn from_seed(seed: Seed) -> Self;
}
```

Nothing in `core:prng` declares an effect, and nothing in it can: a `Seed` is
data, and every constructor of one is arithmetic. Two hundred and fifty-six bits
is xoshiro's four words and SHISHUA's state with no expansion, and is the width
Rust's `SeedableRng` and ChaCha8Rand already use.

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
not run the other way. A program reaching for unpredictable randomness writes
one line that carries the effect and none after it:

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

## Roadmap

1. `Seed`, `Seedable`, and `core:secure_random::seed`. Nothing else can be
   constructed without them.
2. `Rng` with `Xoshiro256pp`, and the uniform derived layer over it.
3. `Squares64`, with the key predicate asserted.
4. `VectorRng` with `XoshiroSimd` and `Shishua`.
5. `example/prng_bench.wado` retargeted at the library, so the numbers above
   keep being checked against the thing that ships rather than against a copy.

## Known gaps

- The exact key predicate `Squares64::from_seed` establishes is not settled.
  Widynski's `keys.h` generator draws hexadecimal digits under constraints that
  a bit-level test approximates rather than reproduces. Closing it means
  choosing the predicate against that generator and stating it as the assert.
- No known-answer vectors are checked anywhere. Each vector form is checked
  against its own scalar form, which catches a transcription error but not a
  wrong constant shared by both. Closing it means a fixture per algorithm
  against the reference implementation's published output.
- `VectorRng` cannot be written against generically. Projecting the batch
  through an associated type resolves (`R: VectorRng<Batch = [..V]>`
  type-checks and `for let v of` unrolls), but bounding what the members admit
  does not: a bound takes only `Name = Type`, and `BitXor`'s `Rhs` is a type
  parameter no bound can name. The fixed sixteen-word batch is what stands in
  for that until it can.
- SHISHUA has no jump and no proven stream separation, so its lanes rest on
  seeding alone. Closing it is not possible within the algorithm; the honest
  answer is that a consumer needing proven disjointness uses xoshiro.
- Distributions and noise functions have no home yet. Both sit on this library
  and neither belongs inside it.
