// Main-thread client for the wado-lsp worker: JSON-RPC id bookkeeping plus
// document-lifecycle helpers. Editor bindings (Monaco providers) live with
// the page; this module only speaks LSP.

export class WadoLsp {
  constructor(workerUrl = new URL("./lsp-worker.js", import.meta.url)) {
    this.worker = new Worker(workerUrl, { type: "module" });
    this.nextId = 1;
    this.pending = new Map();
    this.notificationHandlers = new Map();
    this.versions = new Map();
    this.exited = false;

    this.ready = new Promise((resolve, reject) => {
      this.worker.onmessage = (e) => {
        const { type, msg, text, code } = e.data;
        if (type === "ready") resolve();
        else if (type === "message") this.dispatch(msg);
        else if (type === "stderr") console.warn("[wado-lsp]", text);
        else if (type === "exit") this.fail(`wado-lsp exited with code ${code}`);
        else if (type === "error") {
          reject(new Error(text));
          this.fail(text);
        }
      };
    });
  }

  dispatch(msg) {
    if (msg.id !== undefined && this.pending.has(msg.id)) {
      const { resolve, reject } = this.pending.get(msg.id);
      this.pending.delete(msg.id);
      if ("error" in msg) reject(new Error(`${msg.error.code}: ${msg.error.message}`));
      else resolve(msg.result);
    } else if (msg.method) {
      this.notificationHandlers.get(msg.method)?.(msg.params);
    }
  }

  fail(reason) {
    this.exited = true;
    for (const { reject } of this.pending.values()) reject(new Error(reason));
    this.pending.clear();
  }

  send(msg) {
    if (this.exited) throw new Error("wado-lsp worker has exited");
    this.worker.postMessage({ type: "send", msg });
  }

  request(method, params) {
    const id = this.nextId++;
    const result = new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
    this.send({ jsonrpc: "2.0", id, method, params });
    return result;
  }

  notify(method, params) {
    this.send({ jsonrpc: "2.0", method, params });
  }

  onNotification(method, handler) {
    this.notificationHandlers.set(method, handler);
  }

  async initialize() {
    await this.ready;
    const result = await this.request("initialize", {
      processId: null,
      rootUri: null,
      capabilities: {},
    });
    this.notify("initialized", {});
    return result;
  }

  didOpen(uri, text) {
    this.versions.set(uri, 1);
    this.notify("textDocument/didOpen", {
      textDocument: { uri, languageId: "wado", version: 1, text },
    });
  }

  didChange(uri, text) {
    const version = (this.versions.get(uri) ?? 0) + 1;
    this.versions.set(uri, version);
    this.notify("textDocument/didChange", {
      textDocument: { uri, version },
      contentChanges: [{ text }],
    });
  }

  dispose() {
    this.exited = true;
    this.worker.terminate();
    this.fail("client disposed");
  }
}
