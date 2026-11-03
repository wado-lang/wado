# Eddy TODO

## Performance

The symbol tools cost each evaluation entry an index of its tree before the
reviewer starts, and the load of that index. Each part, measured on this
repository, as a multiple of loading the 91 MB Rust index (reading, decoding
and adding it):

| Part                                           | Cost                                    |
| ---------------------------------------------- | --------------------------------------- |
| `rust-analyzer scip` over a whole tree         | ~63×                                    |
| Compiling a program that imports `scip.proto`  | 16× and 1.8 GB the first time, 8× after |
| `fs::read` of the Rust index                   | 0.39×                                   |
| `grog::decode` of that index                   | 0.54×                                   |
| `SymbolIndex::add` of that index               | 0.07×                                   |
| `wado query scip`, per `.wado` file on 4 cores | 1/120×                                  |

The middle four come from a probe under `wado run -O2` timing each step with
`core:benchmark`; the per-file figure from `wado query scip` over 200 e2e
fixtures.

- [ ] `wado query scip`: each file is an entry of its own, so each analysis
  loads and checks the standard library again. Analyses that share what they
  have already checked would cut the per-file cost to the file's own.
- [ ] Compiling the Grog-generated `scip.proto` bindings: find whether the
  generated source is larger than it needs to be or the compiler is slow on
  it.
- [ ] `fs::read`: reading one whole file takes nearly as long as decoding it,
  far below what the host reads. Find where the stdlib or the host spends it.
- [ ] `grog::decode`: measure where it goes once the two above are out of the
  way.
- [ ] Reuse an entry's indexes across evaluation runs instead of rebuilding
  them with each snapshot. `rust-analyzer scip` dominates and cannot be made
  faster here.
