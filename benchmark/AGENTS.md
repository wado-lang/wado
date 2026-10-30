# benchmark

[README.md](./README.md) holds the results and how to run each benchmark; the
`benchmark` skill refreshes it.

Two measures live here, and they answer different questions:

- Wall clock, through `core:benchmark`: Wado against other implementations.
  Only a quiet machine can time it, so it runs locally and fills `README.md`.
- Fuel, through `core:fuel`: whether a change made Wado's own output do more
  work. Each benchmark's `test "fuel"` block measures its phases once. Only
  `-O2` is a performance claim: fuel does not depend on the host, so CI holds
  every pull request to the `-O2` series in `benchmark/fuel.json` exactly
  (`mise run test-fuel`). gh-pages records `-O1`, `-O2` and `-O3` per commit,
  the other two for reference. A change that moves `-O2` commits the result of
  `mise run update-baselines`. Fuel leaves out GC and other host work by
  design: the wall clock is what shows them.

Read the `wado-performance` skill before measuring a change, and its section
"A/B-ing a stdlib change" before comparing two versions of `wado-compiler/lib`.
Without `WADO_BIN`, `wado.sh` runs `cargo run --release`, and a release build
embeds the stdlib. So an A/B that swaps stdlib files into the tree rebuilds the
compiler for every run.

Dead ends — a change the benchmark ignored, whose WIR diff shows no fewer
instructions, and which does not even come out smaller — live in the
`wado-performance` skill's `dead-ends.md`. A change that is kept belongs in its
commit, not there.
