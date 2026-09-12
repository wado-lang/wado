// mise run test-hooks
import assert from "node:assert/strict";
import { test } from "node:test";

import { findLegacyNames } from "./internal-names.mjs";

const texts = (source) => findLegacyNames(source).map((hit) => hit.text);

test("flags a minted name spelled with the old prefix", () => {
  assert.deepEqual(texts('let n = format!("__hole_{k}");'), ['"__hole_{k}"']);
  assert.deepEqual(texts('name.starts_with("__test_")'), ['"__test_"']);
});

test("passes a name that carries the internal prefix", () => {
  assert.deepEqual(texts('let n = format!("$hole_{k}");'), []);
  assert.deepEqual(texts('pub const CLOSURE_CALL_METHOD: &str = "$call";'), []);
});

test("allows a name Wado source spells, whatever follows it", () => {
  assert.deepEqual(texts('out.push_str("__DATA__\\n");'), []);
  assert.deepEqual(texts('Err("__DATA__ is not an object")'), []);
  assert.deepEqual(texts('name: "__cm_packed".to_string(),'), []);
});

test("reports the line the literal sits on", () => {
  const hits = findLegacyNames('fn f() {\n    let a = 1;\n    g("__iter_0");\n}');
  assert.deepEqual(hits, [{ line: 3, text: '"__iter_0"' }]);
});

test("flags a name appended to another name, whose prefix it inherits", () => {
  assert.deepEqual(texts('format!("{slot}__n_{field}")'), ['"{slot}__n_{field}"']);
  assert.deepEqual(texts('format!("{label}__{inner}")'), []);
  assert.deepEqual(texts('format!("$effect_dispatch__{label}__{op}")'), []);
});

test("looks at literals only", () => {
  assert.deepEqual(texts("// mints __iter_0 today\nlet __buf = 1;"), []);
  assert.deepEqual(texts('// renamed from "__iter_0"'), []);
  assert.deepEqual(texts('let n = r#"__iter_0"#;'), ['r#"__iter_0"#']);
});
