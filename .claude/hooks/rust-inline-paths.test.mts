// mise run test-hooks
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { denialReason, editedRust } from "./rust-inline-paths.mts";

/** A `.rs` file holding `source`, so a hook input can point at something real. */
function rustFile(source: string): string {
  const path = join(mkdtempSync(join(tmpdir(), "inline-paths-")), "a.rs");
  writeFileSync(path, source);
  return path;
}

test("denies an edit that names an item it does not import", () => {
  const file_path = rustFile("fn f() {}\n");
  assert.match(
    denialReason({ file_path, old_string: "fn f() {}", new_string: "fn f() -> crate::ast::Ty { g() }" })!,
    /use` item/,
  );
  assert.match(denialReason({ file_path, content: "fn f() { super::g() }" })!, /line 1/);
});

test("allows an edit that imports what it names", () => {
  const file_path = rustFile("fn f() {}\n");
  assert.equal(denialReason({ file_path, content: "use crate::ast::Ty;\nfn f() -> Ty { g() }" }), null);
});

test("allows renaming a path inside the use item that imports it", () => {
  // An Edit supplies a replacement, not a file: `crate::new` alone reads as an
  // unimported path, and only the file it lands in shows the `use` around it.
  const file_path = rustFile("use crate::ast::old;\nfn f() { old() }\n");
  assert.equal(denialReason({ file_path, old_string: "crate::ast::old", new_string: "crate::ast::new" }), null);
});

test("allows an edit to a file that already carries inline paths", () => {
  // `--check` ratchets per file, so what the corpus already has is not this
  // edit's to answer for; only a path the edit adds is.
  const file_path = rustFile("fn f() -> crate::ast::Ty { g() }\n");
  assert.equal(denialReason({ file_path, old_string: "g()", new_string: "h()" }), null);
  assert.match(denialReason({ file_path, old_string: "g()", new_string: "super::h()" })!, /super::/);
});

test("leaves every file that is not Rust alone", () => {
  const file_path = rustFile("fn f() {}\n").replace(/\.rs$/, ".md");
  assert.equal(denialReason({ file_path, content: "call crate::ast::Ty" }), null);
  assert.deepEqual(editedRust({ new_string: "crate::a" }), { before: "", after: "" });
  assert.deepEqual(editedRust({}), { before: "", after: "" });
});

test("reports where every added path is", () => {
  const file_path = rustFile("fn f() {}\n");
  const reason = denialReason({ file_path, content: "crate::a();\nsuper::b();" })!;
  assert.match(reason, /line 1: crate::, line 2: super::/);
});
