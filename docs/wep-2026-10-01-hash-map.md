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

`core:collections` adds `HashMap<K, V>` and `HashSet<T>`. Both take their seed as
a value. The one call that asks the host for a seed is separate, and it is the
only one with an effect.

### The seed is a value, and every map takes one

```wado
pub struct HashSeed { k0: u64, k1: u64 }

impl HashSeed {
    pub fn random() -> HashSeed with InsecureSeed;
    pub fn fixed(k0: u64, k1: u64) -> HashSeed;
}

impl<K: Hash + Eq, V> HashMap<K, V> {
    pub fn new(seed: HashSeed) -> HashMap<K, V>;
}
```

`HashSeed::random()` is for maps that hold keys from outside the program.
`HashSeed::fixed` is for maps whose keys the program trusts, such as a generator
reading its own grammar. The two names pair, so the choice is visible at every
construction site.

`random()` calls `get-insecure-seed`, which the host is not obliged to fill with
randomness. Its doc comment says so. The interface asks to be called only once,
so a program fetches one seed at its entry and passes it down. Nothing caches it
in a global.

There is no `HashMap::new()` without a seed and no `impl Default`. A default
seed would have to be fixed, and a fixed seed reached by accident is the hazard
this design exists to prevent.

### Trusted keys get a newtype

A package whose maps all hold trusted keys declares a newtype over `HashMap`
under its own fixed seed. The newtype's `new()` takes no seed, and it implements
`Default`, `Serialize` and `Deserialize` itself. A fixed seed reveals nothing
when written out, so none of the reasons that refuse these on `HashMap` hold.
The choice of seed is made once, where the newtype is declared, rather than at
every construction site.

```wado
global SEED: HashSeed = HashSeed::fixed(0x243F6A8885A308D3, 0x13198A2E03707344);

pub type GaleMap<K, V> = HashMap<K, V>;

impl<K: Hash, V> GaleMap<K, V> {
    pub fn new() -> GaleMap<K, V> {
        return HashMap::<K, V>::new(SEED) as GaleMap<K, V>;
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
code that cannot pass a seed along, and the Component Model `map<K, V>` stays
`TreeMap` ([WEP: CM map type](./wep-2026-08-25-cm-map-type.md)). `core:value`
objects, which hold parsed external input, stay `TreeMap` as well.

### No serialization, refused by declaration

`HashMap` and `HashSet` implement neither `Serialize` nor `Deserialize`.
Deserializing has no way to receive a seed. Serializing a derived form would
write the seed out, and a leaked seed disables the defence.

Leaving the impls out is not enough. Bound-driven derivation reaches private
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
- [ ] `#[unavailable]` on trait impls, and the `Serialize` and `Deserialize`
  refusals on `HashMap` and `HashSet`.

## Known gaps

- Floating-point keys have no `Hash`. Their `==` follows IEEE 754, under which
  `-0.0 == 0.0` and `NaN != NaN`, and `Ord` follows the total order instead.
- Until the refusal lands, a `HashMap` is refused `Serialize` only because the
  `Array` behind its entries has none. The error walks the private fields to
  `Array` rather than stating the reason.
- Derivation serializes private fields of any type without a hand-written impl.
  `core:prng`'s `Seed` serializes its state words this way.
