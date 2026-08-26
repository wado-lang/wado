// mise run test-hooks
import assert from "node:assert/strict";
import { test } from "node:test";

import { commandNames, denialReason } from "./forbidden-commands.mts";

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
  "find . -name '*.rs' | \\\n  xargs sed -i 's/a/b/'",
  "a=1 \\\n sed -i x f",
  "case $x in *) sed -i s/a/b/ f;; esac",
  "bash -lc 'sed -i x f'",
  "sh -ec 'awk 1 f'",
  "env -u FOO python3 x.py",
  "sudo -u root sed -i x f",
  "xargs -a list sed -i x f",
  "eval 'sed -i x f'",
  "( sed -i x f )",
  "cat f | while read l; do sed -i x $l; done",
  "__proto__ -x; sed -i x f",
  "$'sed' -i x f",
  '$"sed" -i x f',
  "$'\\x73ed' -i x f",
  "$'\\163ed' -i x f",
  "env -S 'sed -i x f'",
  "env -S'awk 1 f'",
  "env --split-string='sed -i x f'",
  "$'\\UFFFFFFFF'; sed -i x f",
  "cat <<EOF\n$(sed -i x f)\nEOF",
  "cat <<EOF\n`awk 1 f`\nEOF",
  "bash <<'EOF'\nsed -i x f\nEOF",
  "sh <<EOF\nawk 1 f\nEOF",
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
  "find . -name '*.log' -exec mv {} /tmp/awk \\;",
  "cp x {} /tmp/sed",
  "command -v python3",
  "cargo build \\\n  --release",
  "mkdir -p /tmp/{a,b}",
  "git rev-parse 'HEAD^{commit}'",
  "{ cargo build; cargo test; }",
  "trap 'echo bye' EXIT; echo hi",
  "env FOO=1 cargo test",
  "env -u RUSTFLAGS cargo build",
  "cat <<'EOF' > note.md\n$(sed -i x f)\nEOF",
  "cat <<EOF > note.md\n\\$(sed -i x f)\nEOF",
  "node <<'EOF'\nconsole.log(1)\nEOF",
];

test("denies a forbidden command word", () => {
  for (const command of DENIED) assert.ok(denialReason(command), command);
});

test("allows a command that only mentions one", () => {
  for (const command of ALLOWED) assert.equal(denialReason(command), null, command);
});

test("resolves the command word of every nested source", () => {
  assert.deepEqual(commandNames("cat f | while read l; do sed -i x $l; done"), [
    "cat",
    "read",
    "sed",
  ]);
  assert.deepEqual(commandNames("find . -exec mv {} /tmp/awk \\;"), ["find", "mv"]);
  assert.deepEqual(commandNames("echo $(bash -c 'awk 1')"), ["echo", "bash", "awk"]);
});

test("gives the reason of the ban that applies", () => {
  assert.match(denialReason("sed -i x f")!, /editing tools/);
  assert.match(denialReason("nohup cargo test &")!, /background/);
});
