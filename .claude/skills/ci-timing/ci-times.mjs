// Prints how long CI jobs and steps took, one row per workflow run.
//
//   ci-times.mjs --since 2026-09-01 [--until 2026-10-07] [--workflow ci.yml]
//                [--branch main] [--event push] [--job REGEX] [--step REGEX]
//   ci-times.mjs (--run RUN_ID | --sha COMMIT) [--workflow ci.yml] [--job REGEX]
//
// The first form prints, per run, `job=step/job` seconds for each job matching
// --job, the step being the first matching --step (`-` without --step). The
// second prints every step of each matching job in one run.

import { execFileSync } from "node:child_process";
import { parseArgs } from "node:util";

const { values: opt } = parseArgs({
  options: {
    since: { type: "string" },
    until: { type: "string", default: new Date().toISOString().slice(0, 10) },
    workflow: { type: "string", default: "ci.yml" },
    branch: { type: "string", default: "main" },
    event: { type: "string", default: "push" },
    job: { type: "string", default: "" },
    step: { type: "string" },
    run: { type: "string" },
    sha: { type: "string" },
    repo: { type: "string", default: "wado-lang/wado" },
  },
});

// The GitHub API answers 502 now and then on a long sweep.
const gh = (path) => {
  for (let attempt = 1; ; attempt++) {
    try {
      return JSON.parse(execFileSync("gh", ["api", path], { maxBuffer: 1 << 28 }).toString());
    } catch (e) {
      if (attempt === 5) throw e;
    }
  }
};
const secs = (o) => (o?.started_at && o?.completed_at ? Math.round((new Date(o.completed_at) - new Date(o.started_at)) / 1000) : "-");
const jobRe = new RegExp(opt.job);
const jobsOf = (runId) => gh(`repos/${opt.repo}/actions/runs/${runId}/jobs?per_page=100`).jobs.filter((j) => jobRe.test(j.name)).sort((a, b) => a.name.localeCompare(b.name));
const workflowRuns = `repos/${opt.repo}/actions/workflows/${opt.workflow}/runs`;

if (opt.sha) {
  const sha = execFileSync("git", ["rev-parse", opt.sha]).toString().trim();
  opt.run = String(gh(`${workflowRuns}?head_sha=${sha}`).workflow_runs[0].id);
}
if (opt.run) {
  for (const job of jobsOf(opt.run)) {
    console.log(`${job.name}\t${secs(job)}`);
    for (const step of job.steps) console.log(`  ${step.name}\t${secs(step)}`);
  }
} else {
  if (!opt.since) throw new Error("--since, --run or --sha is required");
  // A run listing over a long range comes back incomplete and out of order, so
  // ask one day at a time.
  const runs = new Map();
  for (let d = new Date(opt.since); d <= new Date(opt.until); d.setUTCDate(d.getUTCDate() + 1)) {
    const day = d.toISOString().slice(0, 10);
    const page = gh(`${workflowRuns}?branch=${opt.branch}&event=${opt.event}&created=${day}&per_page=100`);
    for (const run of page.workflow_runs) runs.set(run.id, run);
  }
  const stepRe = opt.step && new RegExp(opt.step);
  for (const run of [...runs.values()].sort((a, b) => a.created_at.localeCompare(b.created_at))) {
    const cols = jobsOf(run.id).map((job) => `${job.name}=${stepRe ? secs(job.steps.find((s) => stepRe.test(s.name))) : "-"}/${secs(job)}`);
    console.log([run.created_at.slice(0, 16), run.conclusion ?? "", run.head_sha.slice(0, 11), cols.join("  "), run.display_title.slice(0, 60)].join("\t"));
  }
}
