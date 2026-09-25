// mise run test-hooks
import assert from "node:assert/strict";
import { test } from "node:test";

import { bracketed, rewritten } from "./bash-rewrite.mts";

const P = "set -o pipefail; ";

test("prepends pipefail to every command", () => {
  assert.equal(rewritten("cargo test | tail"), `${P}cargo test | tail`);
  assert.equal(rewritten("grep pgrep f"), `${P}grep pgrep f`);
});

test("brackets the pattern of a full-command-line match", () => {
  const cases: [string, string][] = [
    ["pgrep -f 'mise run test-wado'", "pgrep -f '[m]ise run test-wado'"],
    [
      'until ! pgrep -f "mise run test"; do sleep 30; done',
      "until ! pgrep -f '[m]ise run test'; do sleep 30; done",
    ],
    ["pgrep -af wado", "pgrep -af '[w]ado'"],
    ["pgrep --full wado", "pgrep --full '[w]ado'"],
    ["pgrep -f -u goro wado", "pgrep -f -u goro '[w]ado'"],
    ["pgrep -fu goro wado", "pgrep -fu goro '[w]ado'"],
    ["pgrep -f --parent 1 wado", "pgrep -f --parent 1 '[w]ado'"],
    ["pgrep -f -- -wado", "pgrep -f -- '-[w]ado'"],
    ["pkill -9 -f cargo", "pkill -9 -f '[c]argo'"],
    ["pkill -TERM -f cargo", "pkill -TERM -f '[c]argo'"],
    ["timeout 5 pgrep -f wado", "timeout 5 pgrep -f '[w]ado'"],
    ["echo $(pgrep -f wado) x", "echo $(pgrep -f '[w]ado') x"],
    ['echo "$(pgrep -f wado)"', "echo \"$(pgrep -f '[w]ado')\""],
    ["pgrep -f \"it's\"", "pgrep -f '[i]t'\\''s'"],
    ["pgrep -f a; pgrep -f bc", "pgrep -f a; pgrep -f '[b]c'"],
  ];
  for (const [command, expected] of cases) assert.equal(rewritten(command), P + expected, command);
});

test("leaves what it cannot bracket safely", () => {
  for (const command of [
    "pgrep -x cargo",
    "pgrep -P 123",
    'pgrep -f "$PAT"',
    "pgrep -f 'a|b'",
    "pgrep -f x",
    "bash -c 'pgrep -f wado'",
  ]) {
    assert.equal(rewritten(command), P + command, command);
  }
});

test("brackets a letter the match still requires after it", () => {
  assert.equal(bracketed("mise run"), "[m]ise run");
  assert.equal(bracketed("^wado"), "^[w]ado");
  assert.equal(bracketed("\\bwado"), "\\b[w]ado");
  assert.equal(bracketed("[ab]cd"), "[ab][c]d");
  assert.equal(bracketed("[[:space:]]ab"), "[[:space:]][a]b");
  assert.equal(bracketed("a{2}bc"), "a{2}[b]c");
  assert.equal(bracketed("a.*"), null);
  assert.equal(bracketed("ab?"), null);
  assert.equal(bracketed("x"), null);
  assert.equal(bracketed("(a|bc)"), null);
});
