// Runs `example/web-browser`, transpiled by `mise run test-web-glue`, against the
// DOM stub: the glue alone stands between the component and the DOM.

import assert from "node:assert/strict";
import { test } from "node:test";
import { install, HTMLElement } from "./dom-stub.mjs";

const component = process.env.WEB_BROWSER_JS;
assert.ok(component, "WEB_BROWSER_JS names the transpiled example/web-browser");
const glue = new URL("./dom.js", import.meta.url);

async function runExample(document, instance) {
  const { run } = await import(`${component}?${instance}`);
  await run.run();
  return document.getElementById("greeting");
}

test("the program reads the input it narrows to", async () => {
  const document = install();
  const input = document.body.appendChild(document.createElement("input"));
  input.id = "name";
  input.value = "Ada";
  const greeting = await runExample(document, "input");
  assert.equal(greeting.textContent, "Hello, Ada!");
  assert.equal(greeting.parentNode, document.body);
});

test("an element that is no input does not narrow to one", async () => {
  const document = install();
  document.body.appendChild(document.createElement("p")).id = "name";
  const greeting = await runExample(document, "paragraph");
  assert.equal(greeting.textContent, "Hello, stranger!");
});

test("one object crosses as one handle, tagged with its nearest class", async () => {
  const { global, document: documentGlue } = await import(glue);
  const document = install();
  assert.equal(global.document(), global.document());

  class HTMLDivElement extends HTMLElement {}
  const handleOf = (object, id) => {
    document.body.appendChild(object).id = id;
    return documentGlue.getElementById(global.document(), id);
  };
  const div = handleOf(new HTMLDivElement("div"), "div");
  const p = handleOf(document.createElement("p"), "p");
  const classOf = (h) => Math.floor(h / 2 ** 37);
  assert.equal(classOf(div), classOf(p));
  assert.notEqual(div, p);
  assert.equal(handleOf(document.getElementById("div"), "div"), div);
});
