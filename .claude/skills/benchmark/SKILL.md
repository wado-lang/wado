---
name: benchmark
description: Measure Wado's performance or the size of the Wasm it generates, and refresh the benchmark/ and wasm-size/ README files. Use for any benchmarking or size-measurement request, whichever benchmark it names.
---

# Benchmark

Run the benchmarks and update `benchmark/README.md` (and `wasm-size/README.md`).

## Prerequisites

```sh
mise run on-task-started
```

- `vendor/wasmtime` submodule must exist (the SessionStart hook handles it;
  otherwise `git submodule update --init --recommend-shallow vendor/wasmtime`).
- http-routing needs `oha` (`cargo install oha`); `bun` is mise-managed.
- gale-gen's and sqlite-parse's ANTLR4 references need `java` and `javac`, which
  `benchmark/mise.toml` provides; the jar is fetched to `~/.cache/gale`. Those
  rows are skipped if the tool is absent.
- wasm-size needs `rustup target add wasm32-wasip1` and Moonbit
  (`curl -fsSL https://cli.moonbitlang.com/install/unix.sh | bash`, then
  `moon update` in each `wasm-size/*` dir).

## Procedure

1. Run `mise run benchmark-all` **three times**, each to its own log in
   `scratchpad/`, then pick per row with
   `mise exec -- node benchmark/pick.ts scratchpad/run{1,2,3}.log` (throttling
   only ever slows things down). Use that tool rather than reading the logs by
   eye: it keys rows by (task, implementation, phase) and selects on ms/iter, so
   a rate that rounds to a tie across runs cannot pair with the wrong ms/iter.
   Which benchmarks run, and in what order, is `benchmark/mise.toml`'s `all`
   task.
2. Run http-routing separately (needs `oha` + pinned cores):
   `SLICE=10 ROUNDS=3 SHAPES="1 4" mise run benchmark-http-routing`. It keeps
   the per-(server, request) max internally, so one invocation suffices, and it
   measures one worker shape per entry in `SHAPES`. Add `HEADROOM_CHECK=1` when
   `CONNECTIONS_PER_WORKER`, `OHA_CORE_COUNT` or `SHAPES` changed, to confirm
   `oha` was not the ceiling.
3. Measure the three PGO builds for the README's Profile-Guided Optimization
   section (see [PGO](#pgo)).
4. Refresh the README Environment line versions: `mise exec -- node --version`,
   `mise -C benchmark exec -- bun --version`, `rustc --version`,
   `cc --version | head -1`. `mise -C benchmark ls` names wasi-sdk, and wasmtime's
   version is the `vendor/wasmtime` workspace version.
5. Update the tables, following README.md's existing layout. http-routing is
   req/s (higher is better), one table per worker shape. Keep measured figures
   inside tables — prose around them is not re-measured and drifts. An input
   size in the prose comes from the program's own header line, such as
   `gale-gen (Rust): 42786 bytes`, so compare each one with the log.
6. wasm-size: `mise run report-wasm-size`, then update `wasm-size/README.md`.
   `mise -C wasm-size ls` names Zig, and `moon version` names Moonbit.
7. Compare every Wado row with the README's previous figure, and attribute each
   one that moved (see [PGO](#pgo)). Report those to the user.

## PGO

Every Wado row in the procedure is a PGO build: `wado run` and `wado compile`
apply the profile in the benchmark's `pgo/` by default. wasm-size has no
profile, so a size change is the compiler's or the program's.

The README's PGO section compares three builds, and its commands stand beside
its tables. Take the wall clock from three rounds that change the order of the
builds. `all-wado` leaves out the table's `gale_html` row, so run
`mise run -C benchmark gale-highlight-html` the same way.

Compare two builds with
`mise exec -- node benchmark/ab.ts --base <no-pgo logs> --head <PGO logs>`. Its
verdict gives the table's `noise`.

A profile can move a row through what the program allocates, because a change of
allocation moves where collections land. `wado run --gc-log` shows that, and
`microgpt` needs a new heap sweep when it happens. A size that beats the default
gets a second row, as `GALE_GEN_BIG_HEAP` does.

A Wado row that moved may have moved through the profile and not the compiler.
Before naming a cause, compare the row's `-f no-pgo` figure with the README's
previous one. If they match, the profile moved it, and `git log` on the row's
`pgo/` file shows when. Whether to record the profile again is a separate
decision, and `benchmark/AGENTS.md` says what a re-recording commits.

## Sweeping a compiler knob

`WADO_BENCH_FLAGS` is appended to every `wado compile` / `wado run` the harness
issues, so an arm costs a benchmark run rather than a release rebuild:

```sh
set -e  # a failed arm would leave pick.ts choosing among the rest
for t in 13 20 32; do
  WADO_BENCH_FLAGS="--optimize-inline-threshold $t" mise run benchmark-all > scratchpad/thr$t.log 2>&1
done
mise exec -- node benchmark/pick.ts scratchpad/thr{13,20,32}.log
```

Read the sweep with `pick.ts` the same way as a best-of-three: it keys rows by
(task, implementation, phase), so the "best" column names the winning arm per
row. Only a knob both `wado compile` and `wado run` accept can be swept this
way, since the harness passes the flags to both. The knob a sweep settles on is
a default in `optimize.rs`, not a flag the README's numbers were taken under —
re-run the suite unflagged before updating the tables.

Comparing the settled default against `origin/main` is a different measurement,
and `WADO_BIN` plus `ab.ts` is how: see the `wado-performance` skill.

## Reading output

Each program prints a throughput line — `<rate> <unit>/s   (<ms> ms/iter, <n> iter)` — per phase (zlib prints two phases, `Compress:`/`Decompress:`).
Read the rate and the ms/iter straight off; the iteration count auto-calibrates
to ~1s, so there is no total to report. The unit is the benchmark's own
(numbers/s, px/s, conversions/s, MB/s, req/s); `vs best` = fastest rate / this
rate.

Report a change between two measurements with improvement as positive: `+9%`
means faster, `-13%` slower. This is the sign `ab.ts` prints.

Each benchmark prints the implementations it compared, so read them off the run
rather than from a list here. Two are conditional: the ANTLR4 rows for
`sqlite-parse` and `gale-gen` are skipped when `java` is absent, and the run
says `SKIP:` when it drops one.

## Workload sizing

Benchmarks auto-calibrate their iteration count to run ~1s, so no manual
denomination is needed. A workload whose single iteration approaches that
reports one iteration and stops averaging — shrink its problem size, across
every language implementation of that benchmark, until it calibrates again.

## Notes

- Tool versions come from mise, not the system.
- Cloud VMs are noisy; best-of-three absorbs the drift.
