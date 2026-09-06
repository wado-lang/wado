// mise run test-hooks
import assert from "node:assert/strict";
import { test } from "node:test";

import { addedRust, denialReason } from "./rust-inline-paths.mts";

test("denies an edit that names an item it does not import", () => {
  assert.match(
    denialReason({ file_path: "a.rs", new_string: "fn f() -> crate::ast::Ty { g() }" })!,
    /use` item/,
  );
  assert.match(denialReason({ file_path: "a.rs", content: "fn f() { super::g() }" })!, /line 1/);
});

test("allows an edit that imports what it names", () => {
  assert.equal(denialReason({ file_path: "a.rs", new_string: "use crate::ast::Ty;" }), null);
  assert.equal(denialReason({ file_path: "a.rs", content: "fn f() -> Ty { g() }" }), null);
});

test("leaves every file that is not Rust alone", () => {
  assert.equal(denialReason({ file_path: "a.md", new_string: "call crate::ast::Ty" }), null);
  assert.equal(addedRust({ new_string: "crate::a" }), "");
  assert.equal(addedRust({}), "");
});

test("reports where every path is", () => {
  const reason = denialReason({ file_path: "a.rs", new_string: "crate::a();\nsuper::b();" })!;
  assert.match(reason, /line 1: crate::, line 2: super::/);
});
