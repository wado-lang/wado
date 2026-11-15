---
name: eddy
description: "Review the branch with Eddy, the repository's own review bot (`package-eddy/`), then answer its findings with /code-review-response. Invoke ONLY when the user asks for it by name — never on your own initiative: a review calls a paid API."
---

# Reviewing the Branch with Eddy

Eddy reviews what the branch changes from its merge base with `origin/main`.
It reads the diff and a checkout of `HEAD`, so an uncommitted change is not
reviewed.

1. Commit everything the review should see.
2. Run the review in the background, its output in files:

   ```sh
   mise run eddy-review > scratchpad/eddy.json 2> scratchpad/eddy.log
   ```

   With `run` as the skill's argument, add `--run`. The reviewer then writes
   tests into its checkout and runs them with `cargo`, so it reports what it
   saw. It takes the lock the test tasks take, so no test runs beside it.
3. Read `scratchpad/eddy.json`. A failed review exits other than 0 and leaves
   the reason on the last line of `scratchpad/eddy.log`.
4. Commit the record the run wrote, `package-eddy/reviews/<head>.json`. It
   holds every review of that commit, and Eddy later learns from which of its
   findings were addressed.
5. Answer the findings with the `code-review-response` skill. With no findings,
   say so and stop.

## Requirements

- `CLAUDE_API_KEY` in the environment. `CLAUDE_MODEL` picks the reviewer.
- `git` and `rg` under `/usr/bin`.
- For `--run`, the toolchain `rust-toolchain.toml` names and the crates
  `Cargo.lock` names, already installed and fetched: the reviewer's `cargo`
  cannot write `~/.cargo` or `~/.rustup`.

## Reading the Findings

One review is not repeatable: on the evaluation set
(`package-eddy/eval/README.md`) the same change gets different findings from
run to run, and how many of them are real is not measured. So a finding is a
claim to verify, and an empty list does not clear the branch.
