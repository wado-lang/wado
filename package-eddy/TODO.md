# Eddy TODO

## Performance

The symbol tools cost minutes per evaluation entry before the reviewer starts:
indexing the tree, then loading the indexes. The parts, measured on this
repository:

| Part                                          | Cost                                              |
| --------------------------------------------- | ------------------------------------------------- |
| `rust-analyzer scip` over a whole tree        | ~6 min, 86–91 MB of index                         |
| `wado query scip`, per `.wado` file           | 47 ms on 4 cores                                  |
| Compiling a program that imports `scip.proto` | 90 s and 1.8 GB on the first compile, ~46 s after |
| `fs::read` of the 91 MB Rust index            | 2.2 s, 42 MB/s                                    |
| `grog::decode` of that index                  | 3.1 s, 30 MB/s                                    |
| `SymbolIndex::add` of that index              | 0.4 s                                             |

The last four come from a probe under `wado run -O2` timing each step with
`core:benchmark`; the per-file figure from `wado query scip` over 200 e2e
fixtures.

- [ ] `wado query scip`: each file is an entry of its own, so each analysis
  loads and checks the standard library again. Analyses that share what they
  have already checked would cut the per-file cost to the file's own.
- [ ] Compiling the Grog-generated `scip.proto` bindings: find whether the
  generated source is larger than it needs to be or the compiler is slow on
  it.
- [ ] `fs::read`: 42 MB/s for one whole file is far below what the host reads.
  Find where the stdlib or the host spends it.
- [ ] `grog::decode`: 30 MB/s. Measure where it goes once the two above are
  out of the way.
- [ ] Reuse an entry's indexes across evaluation runs instead of rebuilding
  them with each snapshot. `rust-analyzer scip` dominates and cannot be made
  faster here.
