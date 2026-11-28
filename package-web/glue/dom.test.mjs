// Runs `example/web-browser`, built by `mise run test-web-glue`, on Node against
// jsdom: the glue alone stands between the component and a standard DOM.

import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { test } from "node:test";

const { JSDOM } = createRequire(new URL("../../tools/jco/package.json", import.meta.url))("jsdom");
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

// `probe/src/main.wado` writes what it found to the attributes of `<body>`.
const probe = process.env.WEB_PROBE_JS;
assert.ok(probe, "WEB_PROBE_JS names the transpiled glue/probe");

test("the host answers identity, narrowing and a listener while `run` waits", async () => {
  install('<input id="input"><div id="div"></div>');
  // jsdom implements none of the members answering a promise.
  const later = (settle) => new Promise((resolve, reject) => setTimeout(() => settle(resolve, reject), 2));
  document.hasStorageAccess = () => later((resolve) => resolve(true));
  document.exitFullscreen = () =>
    later((_, reject) => reject(new DOMException("nothing is fullscreen", "NotAllowedError")));
  document.requestStorageAccess = () => new Promise(() => {});
  let enterFullscreen;
  document.getElementById("input").requestFullscreen = () =>
    new Promise((resolve) => {
      enterFullscreen = resolve;
    });

  // Nor spatial navigation, and its `readyState` moves on its own.
  Object.defineProperty(document, "readyState", { value: "interactive" });
  let searched;
  document.getElementById("div").spatialNavigationSearch = (dir) => {
    searched = dir;
  };
  // Nor `hidden="until-found"`, whose `hidden` is a string.
  Object.defineProperty(document.getElementById("div"), "hidden", { get: () => "until-found" });

  const { run } = await import(probe);
  const found = document.body.dataset;
  const running = run.run();
  while (found.state !== "waiting") {
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
  assert.equal(found.settled, "true");
  assert.equal(found.rejected, "NotAllowedError: nothing is fullscreen");
  assert.equal(found.same, "true false");
  // An input is an `HTMLElement` two levels up; a `<div>` is no input.
  assert.equal(found.narrow, "true true false");
  // A `<div>`'s own interface is not declared, so it is named by `HTMLElement`.
  assert.equal(found.inspect, "HtmlInputElement HtmlElement");
  assert.equal(
    found.hidden,
    'Option::Some(BoolOrF64OrString::AsBool(false)) Option::Some(BoolOrF64OrString::AsString("until-found"))',
  );
  // The list the program set is the list it read back, object for object.
  assert.equal(found.labels, "2 true true");
  assert.equal(found.walk, "true true");
  assert.equal(found.overload, "Xbc");
  assert.equal(found.enum, "DocumentReadyState::Interactive true");
  assert.equal(searched, "down");
  // Of two events, the one made with `bubbles` reached `<body>`, both were
  // captured, and the listener added with `once` heard the first.
  assert.equal(found.records, "1 2 1 true");

  const div = document.getElementById("div");
  assert.deepEqual(div.ariaLabelledByElements, [document.getElementById("input"), div]);
  div.dispatchEvent(new Event("click"));
  assert.equal(found.clicks, "1");
  // `<div>`, `<body>`, `<html>`, the document and the window.
  assert.equal(found.path, "5 true");

  // The listener waits on a promise of its own, and the page goes on.
  document.getElementById("input").dispatchEvent(new Event("focus"));
  assert.equal(found.focus, "waiting");
  div.dispatchEvent(new Event("click"));
  assert.equal(found.clicks, "2");
  enterFullscreen();
  await new Promise((resolve) => setTimeout(resolve, 1));
  assert.equal(found.focus, "true");
  await running;
  assert.equal(found.state, "done");
  // The closure outlives `run`, and keeps what it captured.
  div.dispatchEvent(new Event("click"));
  assert.equal(found.clicks, "3");
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

test("a value the enum declares no case for is refused", async () => {
  const { $glue } = await import(glue);
  install("");
  const { imports } = $glue();
  // jsdom's own answer, which the specification has since dropped.
  assert.equal(document.visibilityState, "prerender");
  assert.throws(() => imports["$wado-lang:web/document#visibility-state"](document), TypeError);
});

test("two handles are the same when they are one object", async () => {
  const { $glue } = await import(glue);
  install('<div id="div"></div><p id="p"></p>');
  const { imports } = $glue();
  const div = document.getElementById("div");
  assert.equal(imports["$same"](div, document.getElementById("div")), 1);
  assert.equal(imports["$same"](div, document.getElementById("p")), 0);
});

// Whether the collector reclaims what `held` refers to, given a few turns.
async function isCollected(held) {
  for (let i = 0; i < 20 && held.deref() !== undefined; i++) {
    await new Promise((resolve) => setTimeout(resolve, 5));
    globalThis.gc();
  }
  return held.deref() === undefined;
}

test("the collector reclaims a handle, a listener and a cycle through both heaps", async () => {
  assert.equal(typeof globalThis.gc, "function", "run node with --expose-gc");
  install('<input id="input"><div id="div"></div>');
  document.hasStorageAccess = async () => true;
  document.exitFullscreen = async () => {};
  document.requestStorageAccess = async () => {};
  document.getElementById("div").spatialNavigationSearch = () => {};
  const { run } = await import(`${probe}?collect`);
  const found = document.body.dataset;
  const running = run.run();
  while (found.state !== "waiting") {
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
  const div = document.getElementById("div");
  div.dispatchEvent(new Event("make"));
  document.getElementById("cyclic").dispatchEvent(new Event("ping"));
  assert.equal(found.cyclic, "cyclic");

  // Out of the page, each element is held by the program alone, if at all.
  const leave = (id) => {
    const element = document.getElementById(id);
    element.remove();
    return new WeakRef(element);
  };
  const [dropped, cyclic, kept] = ["dropped", "cyclic", "kept"].map(leave);
  assert.ok(await isCollected(dropped), "a handle the program dropped");
  assert.ok(await isCollected(cyclic), "an element and the closure listening on it");
  assert.ok(!(await isCollected(kept)), "a handle a global of the program holds");
  div.dispatchEvent(new Event("release"));
  assert.equal(found.released, "kept");
  assert.ok(await isCollected(kept), "the handle, once the global lets go");
  await running;
});
