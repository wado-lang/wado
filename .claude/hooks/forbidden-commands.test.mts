// mise run test-hooks
import assert from "node:assert/strict";
import { test } from "node:test";

import { denialReason } from "./forbidden-commands.mts";

const DENIED = [
  "sed -i 's/a/b/' f.rs",
  "set -o pipefail; cat f | sed -n '1,5p'",
  "git log | awk '{print $1}'",
  "python3 -c 'print(1)'",
  "/usr/bin/python3 x.py",
  "python3.11 x.py",
  "./scripts/python x.py",
  "FOO=1 python x.py",
  "sudo env python3 x.py",
  "timeout 30 python x.py",
  "ls | xargs sed -i s/a/b/",
  "find . -name '*.rs' -exec sed -i s/a/b/ {} +",
  "echo $(awk 'END{print NR}' f)",
  "echo `awk 1 f`",
  'echo "$(sed -n 1p f)"',
  "cat <(awk 1 f)",
  '"sed" -i x f',
  "\\sed -i x f",
  "bash -c 'sed -i x f'",
  "if true; then sed -i x f; fi",
  "for f in *.rs; do sed -i x $f; done",
  "cargo build > out.log 2>&1 && awk 1 out.log",
  "2>err.log sed -i x f",
  "python - <<'PY'\nprint(1)\nPY",
  "nohup cargo test &",
  "nohup mise run test > run.log 2>&1 &",
];

const ALLOWED = [
  "",
  "cargo test",
  "mise run test",
  "node script.mjs",
  "grep -n sed AGENTS.md",
  "rg 'python' docs/",
  "echo used awkward",
  "cat /usr/share/python-docs/readme",
  "printf '%s' \"x | awk 1\"",
  "echo 'sed -i is forbidden'",
  "git commit -m 'drop the sed recipe'",
  "cargo run -- compile x.wado -o /tmp/out.wasm 2>/tmp/after.log",
  "cat <<'EOF' > note.md\nuse sed | awk here\nEOF",
  "gawk --version",
  "grep -rn 'python3 -c' .claude",
  "grep -n nohup AGENTS.md",
];

test("denies a forbidden command word", () => {
  for (const command of DENIED) assert.ok(denialReason(command), command);
});

test("allows a command that only mentions one", () => {
  for (const command of ALLOWED) assert.equal(denialReason(command), null, command);
});

test("gives the reason of the ban that applies", () => {
  assert.match(denialReason("sed -i x f")!, /editing tools/);
  assert.match(denialReason("nohup cargo test &")!, /background/);
});
