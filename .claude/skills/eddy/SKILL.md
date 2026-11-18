---
name: eddy
description: "Review the branch with Eddy, the repository's own review bot (`package-eddy/`), then answer its findings with /code-review-response. Invoke ONLY when the user asks for it by name — never on your own initiative: a review calls a paid API."
---

# Reviewing the Branch with Eddy

Eddy reviews what the branch changes from its merge base with `origin/main`.
It reads the diff and a checkout of `HEAD`, so an uncommitted change is not
reviewed. It also writes tests into that checkout and runs them with `cargo`,
`mise`, or a tool `mise` installed for the project.

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
   The line that starts with `cost:` gives what the review cost, in total and
   for each model; tell the user.
4. Push the record the run wrote, a file under `reviews/<head>/` in the wiki's
   clone `scratchpad/wiki`. That directory holds every review of the commit,
   and Eddy later learns from which of its findings were addressed. A rejected
   push takes `git -C scratchpad/wiki pull --rebase` first: every review adds
   its own file, so nothing conflicts.

   ```sh
   git -C scratchpad/wiki add reviews
   git -C scratchpad/wiki commit -qm "Record Eddy's review of $(git rev-parse HEAD)"
   git -C scratchpad/wiki push -q
   ```

   In a Claude Code cloud session the git proxy serves no wiki, so the push
   carries a token itself. The maintainer's cloud environment sets
   `GITHUB_TOKEN` to a personal access token for this; another environment
   has none unless its owner provides one:
   `git -C scratchpad/wiki -c http.extraheader="Authorization: Basic $(printf 'x-access-token:%s' "$GITHUB_TOKEN" | base64 -w0)" push -q`.
5. Answer the findings with the `code-review-response` skill. With no findings,
   say so and stop.

## Reading a Finding

A finding the reviewer reproduced carries a `repro`: a `patch` against the
reviewed commit, the command in `run`, and `expect`, a text the failing run
prints. `verified` says whether Eddy, after the review, applied
that patch alone to a clean checkout and saw the run fail with that text.

- With `verified` true, the finding is already reproduced. The log names the
  directory that holds the patch of finding `<n>`, counted from 1:
  `git apply scratchpad/eddy/<head>/<n>.patch`, and the command in `run` is the
  failing test to fix against. Keep the test where it belongs in the tree once
  it passes.
- With `verified` false, `output` says why: the run passed, failed another way,
  the patch did not stand alone, or, where `patch` is empty, the test could not
  be tried at all. Verify the finding as any other claim.
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
  `Cargo.lock` names, already installed and fetched, and the tools `mise.toml`
  names, already installed. The reviewer's `cargo` and `mise` cannot write
  the `cargo`, `rustup` and `mise` homes.
