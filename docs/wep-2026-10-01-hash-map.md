# WEP: HashMap

## Context

`core:collections` has one map, `TreeMap<K, V>`. It iterates in insertion order,
and its search tree serves only lookup: no public API reads the keys in sorted
order. Under `K: Ord`, a lookup costs about log₂(n) key comparisons, and a
balanced tree of any shape makes about that many.

Those comparisons are where the time goes. In the `gale_gen` benchmark,
`TreeMap<String, i32>::find_index_str` takes 15% of the samples, more than any
other function. A B-tree in place of the AA tree speeds up insertion, removal,
copying and iteration on `example/treemap_bench.wado`, but leaves `gale_gen`
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

### Iteration keeps insertion order

A `HashMap` iterates in insertion order, as `TreeMap` does. With a random seed,
any order derived from the hash would change from run to run. Insertion order
keeps a program's output the same under every seed.

The layout is the one `TreeMap` already uses for its entries: keys, values and
liveness in parallel arrays, indexed by insertion. An open-addressing table of
entry indices replaces the tree.

### `Hash` joins the prelude

```wado
pub trait Hasher with () {
    fn write_u64(&mut self, x: u64);
    fn write_bytes(&mut self, bytes: ByteSlice);
}

pub trait Hash with () {
    fn hash<H: Hasher>(&self, h: &mut H);
}
```

`Hash` is bound-driven, as `Eq` is. It is derived for structs, variants, tuples,
`List`, `String`, integers, `char` and `bool` wherever a use needs it. Every
implementation keeps `a == b` implying equal hashes. `String` and `StrSlice`
hash alike, so `HashMap<String, V>` offers `get_str` as `TreeMap` does.

The hash function is a seeded multiply-and-fold hash, chosen by measurement. Wasm
has no 64×64→128 multiply, so the candidates are timed as Wasm rather than taken
from their native rankings. SipHash is too slow for the role.

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
- [ ] `HashSeed`, `HashMap` and `HashSet` in `core:collections`. Done when they
  offer `TreeMap`'s and `TreeSet`'s lookup and iteration API.
- [ ] `gale_gen` builds its maps with `HashSeed::fixed`. Done when its benchmark
  row is re-measured.
- [ ] `#[unavailable]` on trait impls, and the `Serialize` and `Deserialize`
  refusals on `HashMap` and `HashSet`.

## Known gaps

- Floating-point keys have no `Hash`. Their `==` follows IEEE 754, under which
  `-0.0 == 0.0` and `NaN != NaN`, and `Ord` follows the total order instead.
- Until the refusal lands, deriving `Serialize` for a `HashMap` writes its
  internals, seed included.
- Derivation serializes private fields of any type without a hand-written impl.
  `core:prng`'s `Seed` serializes its state words this way.
