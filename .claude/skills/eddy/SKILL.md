---
name: eddy
description: "Review the branch with Eddy, the repository's own review bot (`package-eddy/`), then answer its findings with /code-review-response. Invoke ONLY when the user asks for it by name — never on your own initiative: a review calls a paid API."
---

# Reviewing the Branch with Eddy

Eddy reviews what the branch changes from its merge base with `origin/main`.
It reads the diff and a checkout of `HEAD`, so an uncommitted change is not
reviewed. It also writes tests into that checkout and runs them with `cargo`.

1. Commit everything the review should see.
2. Run the review in the background, its output in files:

   ```sh
   mise run eddy-review > scratchpad/eddy.json 2> scratchpad/eddy.log
   ```

   It takes the lock the test tasks take, so no test runs beside it. With
   `no-run` as the skill's argument, add `--no-run`: the reviewer then only
   reads, takes no lock, and no finding carries a `repro`.
3. Read `scratchpad/eddy.json`. A failed review exits other than 0, and the
   line of `scratchpad/eddy.log` that starts with `eddy:` gives the reason.
4. Commit the record the run wrote, `package-eddy/reviews/<head>.json`. It
   holds every review of that commit, and Eddy later learns from which of its
   findings were addressed.
5. Answer the findings with the `code-review-response` skill. With no findings,
   say so and stop.

## Reading a Finding

A finding the reviewer reproduced carries a `repro`: a `patch` against the
reviewed commit, the `cargo` arguments in `run`, and `expect`, a text the
failing run prints. `verified` says whether Eddy, after the review, applied
that patch alone to a clean checkout and saw the run fail with that text.

- With `verified` true, the finding is already reproduced. The log names the
  directory that holds the patch of finding `<n>`, counted from 1:
  `git apply scratchpad/eddy/<head>/<n>.patch`, and `cargo <run>` is the
  failing test to fix against. Keep the test where it belongs in the tree once
  it passes.
- With `verified` false, `output` says why: the run passed, failed another way,
  or the patch did not stand alone. Verify the finding as any other claim.
- With no `repro`, no test of the reviewer's was found to reproduce it, and
  `detail` often says why.

One review is not repeatable: on the evaluation set
(`package-eddy/eval/README.md`) the same change gets different findings from
run to run. So an empty list does not clear the branch.

## Requirements

- `CLAUDE_API_KEY` in the environment. `CLAUDE_MODEL` picks the reviewer.
- `git` and `rg` under `/usr/bin`.
- The main checkout, not a linked worktree: a child of `wado run` cannot reach
  a git directory outside the current one.
- Unless `--no-run`: the toolchain `rust-toolchain.toml` names and the crates
  `Cargo.lock` names, already installed and fetched. The reviewer's `cargo`
  cannot write the `cargo` and `rustup` homes.
