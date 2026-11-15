// Runs `example/web-browser`, built by `mise run test-web-glue`, on Node against
// jsdom: the glue alone stands between the component and a standard DOM.

import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { test } from "node:test";

const { JSDOM } = createRequire(new URL("../../scripts/jco/package.json", import.meta.url))("jsdom");
const component = process.env.WEB_BROWSER_JS;
assert.ok(component, "WEB_BROWSER_JS names the transpiled example/web-browser");
const glue = new URL("./dom.js", import.meta.url);

// Makes `globalThis` the page's `Window`, with its interface objects, as a
// browser's is.
function install(body) {
  const { window } = new JSDOM(`<!doctype html><body>${body}`);
  for (const name of Object.getOwnPropertyNames(window)) {
    if (/^[A-Z]/.test(name) && typeof window[name] === "function") {
      globalThis[name] = window[name];
    }
  }
  Object.setPrototypeOf(globalThis, window.Window.prototype);
  globalThis.document = window.document;
  return window.document;
}

async function runExample(instance) {
  const { run } = await import(`${component}?${instance}`);
  await run.run();
  return document.getElementById("greeting");
}

test("the program reads the input it narrows to", async () => {
  install('<input id="name" value="Ada">');
  const greeting = await runExample("input");
  assert.equal(greeting.outerHTML, '<p id="greeting">Hello, Ada!</p>');
  assert.equal(greeting.parentNode, document.body);
});

test("typing a name greets again, through the listener the program added", async () => {
  install('<input id="name" value="Ada">');
  const greeting = await runExample("typing");
  const input = document.getElementById("name");
  input.value = "Grace";
  input.dispatchEvent(new Event("input"));
  assert.equal(greeting.textContent, "Hello, Grace!");
  assert.equal(document.querySelectorAll("#greeting").length, 1);
});

test("an element that is no input does not narrow to one", async () => {
  install('<p id="name">Ada</p>');
  const greeting = await runExample("paragraph");
  assert.equal(greeting.textContent, "Hello, stranger!");
});

test("an object's class is its nearest declared ancestor's", async () => {
  const { $glue } = await import(glue);
  install('<div id="div"></div><p id="p"></p><input id="input">');
  const { imports } = $glue();
  const classOf = (id) => imports["$class-of"](document.getElementById(id));
  // Neither `HTMLDivElement` nor `HTMLParagraphElement` is declared, so both
  // answer `HTMLElement`'s class.
  assert.equal(classOf("div"), classOf("p"));
  assert.notEqual(classOf("input"), classOf("div"));
  assert.notEqual(imports["$class-of"](document), classOf("div"));
  assert.throws(() => imports["$class-of"]({}), TypeError);
});

test("two handles are the same when they are one object", async () => {
  const { $glue } = await import(glue);
  install('<div id="div"></div><p id="p"></p>');
  const { imports } = $glue();
  const div = document.getElementById("div");
  assert.equal(imports["$same"](div, document.getElementById("div")), 1);
  assert.equal(imports["$same"](div, document.getElementById("p")), 0);
});
