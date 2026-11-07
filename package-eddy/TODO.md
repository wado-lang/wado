# Eddy TODO

## Performance

The symbol tools cost each evaluation entry an index of its tree before the
reviewer starts, and the load of that index. Each part, measured on this
repository, as a multiple of loading the 91 MB Rust index (reading, decoding
and adding it) before the load read it a document at a time:

| Part                                           | Cost                                    |
| ---------------------------------------------- | --------------------------------------- |
| `rust-analyzer scip` over a whole tree         | ~63×                                    |
| Compiling a program that imports `scip.proto`  | 16× and 1.8 GB the first time, 8× after |
| `fs::read` of the Rust index                   | 0.39×                                   |
| `wado query scip`, per `.wado` file on 4 cores | 1/120×                                  |

The middle two come from a probe under `wado run -O2` timing each step with
`core:benchmark`; the per-file figure from `wado query scip` over 200 e2e
fixtures.

The load now reads the index a document at a time, out of linear memory.
Measured later against `fs::read` of the Rust index, which that did not
change, with a probe under `wado run -O2` timing each step with `wasi:clocks`
(`--gc-heap-initial 1g` for the 1 GiB heap):

| Part                                                       | Cost |
| ---------------------------------------------------------- | ---- |
| `fs::read_linear` of the Rust index, which the load reads  | 0.8× |
| `SymbolIndex::add`, a document at a time                   | 2.3× |
| The same, the guest started at a 1 GiB heap                | 1.9× |
| The same read out of a GC `ByteList`, before linear memory | 3.3× |
| `grog::decode` of the whole index, which the load replaced | 19×  |

- [ ] `wado query scip`: each file is an entry of its own, so each analysis
  loads and checks the standard library again. Analyses that share what they
  have already checked would cut the per-file cost to the file's own.
- [ ] Compiling the Grog-generated `scip.proto` bindings: find whether the
  generated source is larger than it needs to be or the compiler is slow on
  it.
- [ ] `fs::read`: reading one whole file is far below what the host reads.
  Find where the stdlib or the host spends it.
- [ ] `SymbolIndex::add`: the gap between the default heap and a 1 GiB one is
  the copying GC re-copying the live set on each cycle. The input bytes are
  out of the GC heap now; what stays live is the index itself, a `Site`
  object per occurrence.
- [ ] Reuse an entry's indexes across evaluation runs instead of rebuilding
  them with each snapshot. `rust-analyzer scip` dominates and cannot be made
  faster here.
