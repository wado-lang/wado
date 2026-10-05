# WEP: HashMap

## Context

`core:collections` has one map, `TreeMap<K, V>`. It iterates in insertion order,
and its search tree serves only lookup: no public API reads the keys in sorted
order. Under `K: Ord`, a lookup costs about log₂(n) key comparisons, and a
balanced tree of any shape makes about that many.

Those comparisons are where the time goes. In the `gale_gen` benchmark,
`TreeMap<String, i32>::find_index_str` takes 15% of the samples, more than any
other function. A B-tree in place of the AA tree speeds up insertion, removal,
copying and iteration on `example/map_bench.wado`, but leaves `gale_gen`
within noise. A hash table needs one hash and, usually, one comparison per
lookup.

A hash table brings a hazard that a tree does not have. When an attacker chooses
the keys, colliding keys turn every operation linear, which is a denial of
service. The defence is a hash keyed by a seed the attacker cannot predict.
`wasi:random/insecure-seed` exists for exactly that: the host hands out 128 bits
once, for hash-map seeding.

Asking the host is an effect, and a collection should not carry one. A map built
deep inside a pure function would otherwise force `with InsecureSeed` up through
every caller.

## Decision

`core:collections` adds `HashMap<K, V>` and `HashSet<T>`. `new()` hashes under a
default seed that the host supplies once per instance, and `with_seed` takes a
seed of the caller's choosing. Neither carries an effect.

### The default seed, and a seed of the caller's choosing

```wado
pub struct HashSeed { k0: u64, k1: u64 }

impl HashSeed {
    pub fn random() -> HashSeed with InsecureSeed;
    pub fn fixed(k0: u64, k1: u64) -> HashSeed;
}

#[benign(InsecureSeed)]
global DEFAULT_HASH_SEED: HashSeed = HashSeed::random();

impl<K: Hash + Eq, V> HashMap<K, V> {
    pub fn new() -> HashMap<K, V>;                    // under DEFAULT_HASH_SEED
    pub fn with_seed(seed: HashSeed) -> HashMap<K, V>;
}
```

`HashSet` has the same two constructors. `DEFAULT_HASH_SEED` is private to
`core:collections`.

`HashSeed::random()` calls `get-insecure-seed`, which the host is not obliged to
fill with randomness. Its doc comment says so. The interface asks to be called
only once, and a global initializer is the one place a program runs once. The
seed never shows through a map: iteration keeps insertion order (below), so the
effect is unobservable, which is what `#[benign]` asserts.

`with_seed(HashSeed::fixed(…))` is for maps whose keys the program trusts and
whose hashing must be the same on every run. `with_seed(HashSeed::random())`
gives a map a seed of its own.

### `#[benign]` extends to globals

A global's initializer may not perform an effect
([Global Variables](./spec-expressions.md#global-variables)). `#[benign(E, …)]`
on a global admits the listed effects in its initializer, as it admits them in a
function body. Nothing propagates, since an initializer has no caller, and the
world import of each listed effect is still required. Only the listed effects
are admitted: an initializer that may do anything would hide I/O behind every
export.

No handler is installed while an initializer runs. An effect a host import
serves, such as `InsecureSeed`, works there. An `interface` effect falls to its
default implementation, or traps without one.

### Whether and when an initializer runs is unspecified

The order in which initializers run, and whether one runs at all, is
unspecified. The one guarantee is that a
global's initializer has run before the global is read. A cycle among
initializers stays an error. A `#[benign]` global is no exception.

This replaces the rule that an initializer runs at module initialization whether
or not anything reads the global. Under the old rule `DEFAULT_HASH_SEED` would
ask the host for a seed, and keep the `insecure-seed` import, in every program
that imports `core:collections`, `TreeMap` users included. Under the new one, a
program that never reaches `HashMap::new()` imports nothing for it.

### `#[benign]` on functions stays

`#[ambient]` cannot take over the two functions that carry `#[benign]`:

- `coverage_probe` (`#[benign(CoverageHost)]`) must never be removed. An
  `#[ambient]` call whose result goes unused may be removed, and a probe returns
  `()`. [WEP: Test Coverage](./wep-2026-09-28-test-coverage.md) rests on no pass
  removing a probe that could run.
- `eval` (`#[benign(EvalHost)]`) would still work, but `#[ambient]` is for
  best-effort output and lifts the check from the whole body. `eval`'s case is
  the one `#[benign]` names: an effect that does not show through the interface.

### Trusted keys get a newtype

A package whose maps all hold trusted keys declares a newtype over `HashMap`
under its own fixed seed. Its hashing is the same on every run, and it needs no
`insecure-seed` import. The newtype implements `Default`, `Serialize` and
`Deserialize` itself, under its own seed. A fixed seed reveals nothing when
written out, so the reason that refuses `Serialize` on `HashMap` does not hold.
The choice of seed is made
once, where the newtype is declared, rather than at every construction site.

```wado
global SEED: HashSeed = HashSeed::fixed(0x243F6A8885A308D3, 0x13198A2E03707344);

pub type GaleMap<K, V> = HashMap<K, V>;

impl<K: Hash, V> GaleMap<K, V> {
    pub fn new() -> GaleMap<K, V> {
        return HashMap::<K, V>::with_seed(SEED) as GaleMap<K, V>;
    }
}
```

Gale does this for its generator, its corpus tools, and the baselines they read
and write.

### Iteration keeps insertion order

A `HashMap` iterates in insertion order, as `TreeMap` does. With a random seed,
any order derived from the hash would change from run to run. Insertion order
keeps a program's output the same under every seed.

Both maps keep their entries the same way: keys, values and liveness in parallel
arrays, indexed by insertion. One internal type holds them, and the two maps
differ only in the index over it, a B-tree or an open-addressing table of entry
indices. The iterators walk the entries, so the two maps and the two sets share
them: `MapKeysRefIter`, `MapEntriesRefIter` and the rest. Neither map is the
other's variant, and `core:collections` holds them as peers:
`collections/treemap.wado`, `collections/hashmap.wado`, and the shared
`collections/entries.wado` behind the facade.

Comparison reads the same entries. Two maps are equal when they hold equal
entries in the same insertion order, and order entry by entry, then by length,
as a `List` of pairs does. A seed, the index layout, and what was removed on the
way take no part.

### `Hash` joins the prelude

```wado
pub trait Hasher with () {
    fn write_u64(&mut self, x: u64);
    fn write_bytes(&mut self, bytes: ByteSlice);
}

pub trait Hash: Eq with () {
    fn hash<H: Hasher>(&self, h: &mut H);
}
```

`Hash` is bound-driven, as `Eq` is. It is derived for structs, variants, tuples,
`List`, `String`, integers, `char` and `bool` wherever a use needs it. Every
implementation keeps `a == b` implying equal hashes. `String` and `StrSlice`
hash alike, so `HashMap<String, V>` offers `get_str` as `TreeMap` does.

The hash function is a seeded multiply-and-fold hash, chosen by measurement.
Each word is folded into the state by a full-width multiply with a seeded key,
the two halves of the product XORed together. A multiply that keeps only the
low half carries a difference only upward, so the next word can cancel a
difference in the top bit whatever the seed. The 64×64→128 multiply is Wasm's
wide-arithmetic proposal, or a software fallback without it, so the candidates
are timed as Wasm rather than taken from their native rankings. SipHash is too
slow for the role.

### `TreeMap` stays

`TreeMap` needs no seed, and its worst case is logarithmic. It stays the map for
a world without `insecure-seed`, and the Component Model `map<K, V>` stays
`TreeMap` ([WEP: CM map type](./wep-2026-08-25-cm-map-type.md)). `core:value`
objects, which hold parsed external input, stay `TreeMap` as well.

### `Default` and `Deserialize` under the default seed

`HashMap` and `HashSet` implement `Default` and `Deserialize`, by hand. Both
build the map under `DEFAULT_HASH_SEED`, as `new()` does. `Deserialize` reads
the entries alone and inserts them in input order, as `TreeMap`'s does. A
derived `Deserialize` would instead read the seed from the input, which is the
attacker's to choose.

### `Serialize`, refused by declaration

`HashMap` and `HashSet` do not implement `Serialize`. A derived form would write
the seed out, and a leaked seed disables the defence.

Leaving the impl out is not enough. Bound-driven derivation reaches private
fields across modules, so a type without a hand-written impl serializes its
internals. `#[unavailable(reason)]` therefore extends to trait impls, and
`HashMap` declares its refusal:

```wado
#[unavailable("a HashMap's layout holds its hash seed")]
impl<K, V> Serialize for HashMap<K, V>;
```

A use that needs the impl reports the reason, as a call to a declared-absent
function does ([WEP: Declared absence](./wep-2026-09-13-declared-absence.md)).

## Roadmap

- [ ] `Hash` and `Hasher` in the prelude, with bound-driven derivation. Done when
  every type listed above hashes consistently with its `Eq`.
- [ ] The hash function, chosen by measurement on Wasm.
- [x] `HashSeed`, `HashMap` and `HashSet` in `core:collections`. Done when they
  offer `TreeMap`'s and `TreeSet`'s lookup and iteration API.
- [x] `gale_gen` builds its maps as `GaleMap` and `GaleSet`. Done when its
  benchmark row is re-measured.
- [ ] `#[unavailable]` on trait impls, and the `Serialize` refusals on `HashMap`
  and `HashSet`.
- [ ] `#[benign(E, …)]` on a global, stated in `spec-attributes.md`. Done when a
  fixture shows the listed effects admitted in its initializer, an unlisted one
  still rejected, and the world import still required.
- [ ] The unspecified-initializer rule in `spec-expressions.md`, replacing the
  fixture that pins an unread global's trapping initializer as run. Done when
  the optimizer removes an unread global whose initializer performs a benign
  effect, along with the import that only it reached.
- [ ] `DEFAULT_HASH_SEED`, `new()` under it and `with_seed(seed)` on `HashMap`
  and `HashSet`. Done when every caller of the old `new(seed)` has moved to
  `with_seed`.
- [ ] `Default` and `Deserialize` on `HashMap` and `HashSet`. Done when a JSON
  object decodes into a `HashMap<String, V>` in input order.

## Known gaps

- Every map built by `new()` in one instance shares `DEFAULT_HASH_SEED`. A
  long-lived instance, such as an HTTP service, gives an attacker many requests
  against one seed to learn its collisions from timing.
- A hand-written `Serialize` that writes the entries alone would leak nothing,
  but whether `HashMap` and `HashSet` offer one is undecided.
- Floating-point keys have no `Hash`. Their `==` treats every NaN as one value
  and `-0.0` as equal to `0.0`, so a `Hash` has to agree on each of those
  groups, which bit patterns do not.
- Until the refusal lands, a `HashMap` is refused `Serialize` only because the
  `Array` behind its entries has none. The error walks the private fields to
  `Array` rather than stating the reason.
- Derivation serializes private fields of any type without a hand-written impl.
  `core:prng`'s `Seed` serializes its state words this way.
