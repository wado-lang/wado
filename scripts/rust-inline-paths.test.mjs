// mise run test-hooks
import assert from "node:assert/strict";
import { test } from "node:test";

import { findInlinePaths, stripNonCode } from "./rust-inline-paths.mjs";

const texts = (source) => findInlinePaths(source).map((hit) => hit.text);

test("flags a path written inline instead of imported", () => {
  assert.deepEqual(texts("fn f() -> crate::ast::Ty { super::g() }"), ["crate::", "super::"]);
  assert.deepEqual(texts("<crate::ast::Ty as Trait>::NAME"), ["crate::"]);
  assert.deepEqual(texts("x.map(super::name::LocalMethodName::is_trait_method)"), ["super::"]);
});

test("allows a path brought into scope by a use item", () => {
  assert.deepEqual(texts("use crate::ast::Ty;"), []);
  assert.deepEqual(texts("pub use super::x;"), []);
  assert.deepEqual(texts("pub(crate) use crate::a::{b, c};"), []);
  assert.deepEqual(texts("use crate::{\n    a::B,\n    c::super_d,\n};"), []);
  assert.deepEqual(texts("use crate::a as b;\nfn f() { crate::c() }"), ["crate::"]);
});

test("reads through a use item that never terminates", () => {
  assert.deepEqual(texts("use crate::a"), []);
});

test("ignores comments", () => {
  assert.deepEqual(texts("// crate::a::B\nlet x = 1;"), []);
  assert.deepEqual(texts("/// See [`derive`](super::derive).\nfn f() {}"), []);
  assert.deepEqual(texts("/* crate::a /* super::b */ crate::c */ fn f() {}"), []);
});

test("ignores literals", () => {
  assert.deepEqual(texts('let s = "crate::a";'), []);
  assert.deepEqual(texts('let s = r#"super::a"#;'), []);
  assert.deepEqual(texts('let s = br"crate::a";'), []);
  assert.deepEqual(texts('let s = c"crate::a";'), []);
  assert.deepEqual(texts("let s = '\\'';\nfn f() { crate::a() }"), ["crate::"]);
});

test("keeps code that only looks like a literal", () => {
  assert.deepEqual(texts("fn f<'a>(x: &'a str) -> &'a crate::Ty { crate::g(x) }"), [
    "crate::",
    "crate::",
  ]);
});

test("ignores a path the macro hygiene owns", () => {
  assert.deepEqual(texts("macro_rules! m { () => { $crate::a::b() } }"), []);
});

test("keeps `crate` that is not a path root", () => {
  assert.deepEqual(texts("pub(crate) fn f() {}"), []);
});

test("locates a hit by line and column", () => {
  const [hit] = findInlinePaths("fn f() {\n    let x = super::g();\n}");
  assert.deepEqual(hit, { line: 2, column: 13, text: "super::" });
});

test("blanks non-code without moving any later offset", () => {
  const source = '// a\n/* b */ let s = "c"; // d\n';
  const stripped = stripNonCode(source);
  assert.equal(stripped.length, source.length);
  assert.equal(stripped.split("\n").length, source.split("\n").length);
  assert.equal(stripped.replace(/ +/g, " ").trim(), "let s = ;");
});

test("reads a capture list however much space precedes its angle bracket", () => {
  // Rust allows any whitespace in `impl Trait + use <'a>`; read as an import,
  // its span would swallow every path up to the next `;`.
  const q = String.fromCharCode(39);
  assert.deepEqual(texts(`fn f() -> impl Sized + use<${q}a> { crate::g() }`), ["crate::"]);
  assert.deepEqual(texts(`fn f() -> impl Sized + use     <${q}a> { crate::g() }`), ["crate::"]);
});
