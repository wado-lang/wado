---
name: ci-timing
description: Measure how long GitHub Actions jobs and steps took across past runs, and find the pull request that made CI slower. Use for any question about CI duration, a slow job, or when CI got slow.
---

# CI Timing

`ci-times.mjs` reads run, job and step timings from the GitHub API with `gh`.
Run it with mise's Node, and redirect its output to a file in `scratchpad/`:
a sweep over a month makes hundreds of API calls.

## Find When a Job Got Slower

Sweep the runs on `main`, one row per run, oldest first:

```sh
mise exec -- node .claude/skills/ci-timing/ci-times.mjs \
  --since 2026-09-01 --job 'E2E\+stdlib' --step stdlib \
  > scratchpad/ci-times.tsv
```

Each matched job prints as `name=STEP/JOB` in seconds. `-` means the step did
not run or there was no `--step`. The row ends with the run's title, which
names the merged pull request. Look for the row where a job's total steps up
and stays up.

Only runs on `main` from `push` events give a clean timeline: each one is a
single merge. The defaults are `--workflow ci.yml --branch main --event push`.

## Find Which Step Grew

The job total can grow while the step you guessed stays flat. A step that a
pull request added is invisible to `--step`. Print every step of the runs on
either side of the jump and compare them:

```sh
mise exec -- node .claude/skills/ci-timing/ci-times.mjs --sha <before> --job 'stdlib O3'
mise exec -- node .claude/skills/ci-timing/ci-times.mjs --sha <after>  --job 'stdlib O3'
```

`--run <run id>` does the same for a run that is not on `main`, such as a pull
request's run.

## Read the Numbers

- Hosted runners vary from run to run. Several runs on each side of a jump
  show a real change. A single run does not.
- The slowest job sets how long CI takes. A step that grew in a job that
  finishes early does not slow CI down.
- A test step grows as tests are added. Compare its growth with the growth in
  the count of what it runs (`wado-compiler/tests/fixtures/*.wado`, `test`
  blocks under `wado-compiler/lib/`) before calling it a regression.
