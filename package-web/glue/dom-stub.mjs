// A DOM stub with the members `example/web-browser` reaches, so Node runs the
// glue with no browser. Installing it makes `globalThis` a `Window`, as a page's is.

export class EventTarget {}

export class Node extends EventTarget {
  parentNode = null;
  childNodes = [];

  appendChild(child) {
    child.parentNode = this;
    this.childNodes.push(child);
    return child;
  }
}

export class Element extends Node {
  id = "";
  textContent = null;
}

export class HTMLElement extends Element {}

export class HTMLInputElement extends HTMLElement {
  value = "";
}

export class Document extends Node {
  body = this.appendChild(new HTMLElement());

  createElement(localName) {
    return localName === "input" ? new HTMLInputElement() : new HTMLElement();
  }

  getElementById(id) {
    const find = (node) => {
      for (const child of node.childNodes) {
        if (child.id === id) return child;
        const found = find(child);
        if (found !== null) return found;
      }
      return null;
    };
    return find(this);
  }
}

export class Window extends EventTarget {}

export class Event {}

export function install() {
  Object.assign(globalThis, {
    EventTarget,
    Node,
    Element,
    HTMLElement,
    HTMLInputElement,
    Document,
    Window,
    Event,
  });
  Object.setPrototypeOf(globalThis, Window.prototype);
  globalThis.document = new Document();
  return globalThis.document;
}
