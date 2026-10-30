// mise run test-hooks
import assert from "node:assert/strict";
import path from "node:path";
import { test } from "node:test";

import { denialReason } from "./vendor-license-guard.mts";

const ROOT = path.resolve(import.meta.dirname, "../..");

const DENIED: [string, string?][] = [
  ["cat vendor/onnx/onnx/defs/math/defs.cc"],
  ["cat vendor/onnx/onnx/helper.py"],
  ["head vendor/onnx/onnx/backend/base.py"],
  ['cat "vendor/onnx/onnx/version_converter/convert.cc"'],
  ["cat vendor/onnx/tools/gen_coverage_report.py"],
  ["rg Attention vendor/onnx/onnx/reference"],
  ["rg -n Attention vendor/onnx/onnx/"],
  ["grep -r softmax vendor/onnx/onnx"],
  ["rg -n Attention vendor/onnx"],
  ["rg x vendor"],
  ["rg x vendor/onnx/onnx/*.h"],
  ["cat vendor/onnx/tests/../onnx/reference/ops/op_attention.py"],
  ["ls vendor/onnx/tests/&&cat vendor/onnx/onnx/reference/ops/op_add.py"],
  ["cat vendor/onnx/onnx/backend/test/case/node/attention.py vendor/onnx/onnx/checker.cc"],
  ["cd vendor/onnx/onnx && cat helper.py"],
  ["cd vendor/onnx && rg -n Attention ."],
  ["cat < vendor/onnx/onnx/helper.py"],
  ["rg --files-with-matches x --glob=vendor/onnx/onnx/helper.py"],
  ["cat onnx/helper.py", "vendor/onnx"],
  ["cat ../onnx/onnx/helper.py", "vendor/antlr4"],
  ["git -C vendor/onnx grep Attention"],
  ["cat vendor/antlr4/tool/src/org/antlr/v4/Tool.java"],
  ["cat vendor/antlr4/runtime/Java/src/org/antlr/v4/runtime/atn/LL1Analyzer.java"],
  ["rg ParserATNSimulator vendor/antlr4/runtime"],
  ["bash -c 'cat vendor/onnx/onnx/helper.py'"],
  ["git show HEAD:vendor/onnx/onnx/helper.py"],
  ["git show :vendor/onnx/onnx/helper.py"],
  ["git log -p -- vendor/onnx/onnx"],
  ["git log --patch vendor/onnx/onnx"],
  ['cat "$PWD/vendor/onnx/onnx/helper.py"'],
  ["cat ${PWD}/vendor/onnx/onnx/helper.py"],
  ["cat $REPO/vendor/onnx/onnx/helper.py"],
  ['cd "$PWD/vendor/onnx/onnx" && cat helper.py'],
  ['cd "$PWD" && cat vendor/onnx/onnx/helper.py'],
  ["cd $REPO/vendor/onnx/onnx && cat helper.py"],
  ["cd ~ && cat vendor/onnx/onnx/helper.py"],
  ["cd vendor/onnx/onnx && cd /tmp && cd - && cat helper.py"],
  ["pushd vendor/onnx/onnx && pushd /tmp && popd && cat helper.py"],
  ["cd vendor/onnx/onnx && cd /tmp && cat ~-/helper.py"],
  ["cat ~+/vendor/onnx/onnx/helper.py"],
  ["cat ${REPO:-.}/vendor/onnx/onnx/helper.py"],
  ["cat $(git rev-parse --show-toplevel)/vendor/onnx/onnx/helper.py"],
  ["cat `pwd`/vendor/onnx/onnx/helper.py"],
];

const ALLOWED: [string, string?][] = [
  ["cat vendor/onnx/onnx/backend/test/case/node/attention.py"],
  ["rg -n Attention vendor/onnx/onnx/backend/test"],
  ["ls vendor/onnx/tests/cpp"],
  ["rg x vendor/onnx/tests"],
  ["cat vendor/onnx/onnx/defs/schema.h"],
  ["cat vendor/onnx/onnx/onnx.proto"],
  ["cat vendor/onnx/onnx/onnx_cpp2py_export/defs.pyi"],
  ["cat vendor/onnx/onnx/fuzz/fuzz_checker.py"],
  ["rg Attention vendor/onnx/docs/Operators.md"],
  ["ls vendor/onnx"],
  ["ls vendor/onnx/onnx/defs"],
  ["cd vendor/onnx && ls onnx"],
  ["git -C vendor/onnx ls-files onnx"],
  ["git submodule update --init --depth 1 vendor/onnx"],
  ["git log --oneline -- vendor/onnx"],
  ["git show HEAD:vendor/onnx/onnx/onnx.proto"],
  ["cat $PWD/vendor/onnx/docs/Operators.md"],
  ["cd vendor/onnx/onnx && cd - && cat helper.py"],
  ["pushd vendor/onnx/onnx && popd && cat helper.py"],
  ["cat vendor/antlr4/doc/lexer-rules.md"],
  ["cat vendor/antlr4/tool/resources/org/antlr/v4/parse/ANTLRLexer.g4"],
  ["rg -n tpu --glob '!vendor' ."],
  ["echo hello"],
  ["cat helper.py", "package-loam"],
];

test("names forbidden source", () => {
  for (const [command, cwd] of DENIED) {
    assert.notEqual(denialReason(command, path.join(ROOT, cwd ?? "")), null, command);
  }
});

test("leaves the rest alone", () => {
  for (const [command, cwd] of ALLOWED) {
    assert.equal(denialReason(command, path.join(ROOT, cwd ?? "")), null, command);
  }
});
