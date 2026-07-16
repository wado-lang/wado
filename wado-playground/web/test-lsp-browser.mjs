// Browser test for the wado-lsp worker: serves web/ and drives a JSPI-capable
// Chromium through an initialize → didOpen → diagnostics → hover round-trip.
//
// Usage: node wado-playground/web/test-lsp-browser.mjs
// Requires playwright-core and a Chromium 137+ (CHROME_PATH overrides), plus
// wado-lsp.wasm staged in web/ (mise run playground-web-build).

import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { join, extname, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { existsSync } from "node:fs";

const WEB = dirname(fileURLToPath(import.meta.url));
const require = createRequire(join(WEB, "..", "..", "scripts", "jco") + "/");
const { chromium } = require("playwright-core");

function chromePath() {
  if (process.env.CHROME_PATH) return process.env.CHROME_PATH;
  const glob = "/opt/pw-browsers";
  if (existsSync(glob)) {
    const dir = require("node:fs")
      .readdirSync(glob)
      .find((d) => d.startsWith("chromium-") && existsSync(join(glob, d, "chrome-linux", "chrome")));
    if (dir) return join(glob, dir, "chrome-linux", "chrome");
  }
  return undefined;
}

const MIME = {
  ".html": "text/html",
  ".js": "text/javascript",
  ".mjs": "text/javascript",
  ".wasm": "application/wasm",
  ".json": "application/json",
};

const server = createServer(async (req, res) => {
  try {
    const p = join(WEB, decodeURIComponent(new URL(req.url, "http://x").pathname));
    const body = await readFile(p);
    res.writeHead(200, { "content-type": MIME[extname(p)] ?? "application/octet-stream" });
    res.end(body);
  } catch {
    res.writeHead(404);
    res.end("not found");
  }
});
await new Promise((r) => server.listen(0, r));
const port = server.address().port;

const BROKEN = `use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    let x: u32 = "not a number";
    println("hi");
}
`;

const FIXED = `use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println("hi");
}
`;

const browser = await chromium.launch({ executablePath: chromePath(), args: ["--no-sandbox"] });
let failed = 0;
const check = (name, pass, detail) => {
  console.log(`${pass ? "✅" : "❌"} ${name}: ${detail}`);
  if (!pass) failed++;
};

try {
  const page = await browser.newPage();
  page.on("pageerror", (e) => console.log("[pageerror]", e.message));
  page.on("console", (m) => {
    if (m.type() === "warning" || m.type() === "error") console.log(`[console.${m.type()}]`, m.text());
  });
  await page.goto(`http://127.0.0.1:${port}/index.html`, { waitUntil: "load" });

  const r = await page.evaluate(
    async ({ broken, fixed }) => {
      const timeout = (ms, what) =>
        new Promise((_, rej) => setTimeout(() => rej(new Error(`timeout: ${what}`)), ms));
      const { WadoLsp } = await import("./lsp-client.js");
      const lsp = new WadoLsp();

      let onDiags = null;
      lsp.onNotification("textDocument/publishDiagnostics", (p) => onDiags?.(p));
      const nextDiags = (what) =>
        Promise.race([new Promise((res) => (onDiags = res)), timeout(60000, what)]);

      const init = await Promise.race([lsp.initialize(), timeout(60000, "initialize")]);

      const uri = "file:///playground.wado";
      let diagsPromise = nextDiags("diagnostics after didOpen");
      lsp.didOpen(uri, broken);
      const brokenDiags = await diagsPromise;

      const hover = await Promise.race([
        lsp.request("textDocument/hover", {
          textDocument: { uri },
          position: { line: 4, character: 6 },
        }),
        timeout(60000, "hover"),
      ]);

      diagsPromise = nextDiags("diagnostics after didChange");
      lsp.didChange(uri, fixed);
      const fixedDiags = await diagsPromise;

      lsp.dispose();
      return {
        serverName: init?.serverInfo?.name,
        brokenCount: brokenDiags.diagnostics.length,
        firstMessage: brokenDiags.diagnostics[0]?.message ?? "",
        hoverText: JSON.stringify(hover?.contents ?? null),
        fixedCount: fixedDiags.diagnostics.length,
      };
    },
    { broken: BROKEN, fixed: FIXED },
  );

  check("initialize", r.serverName === "wado-lsp", `serverInfo.name=${r.serverName}`);
  check(
    "diagnostics on type error",
    r.brokenCount >= 1,
    `${r.brokenCount} diagnostic(s): ${r.firstMessage}`,
  );
  check("hover on println", r.hoverText.includes("println"), r.hoverText);
  check("diagnostics cleared after fix", r.fixedCount === 0, `${r.fixedCount} diagnostic(s)`);
} catch (e) {
  check("lsp round-trip", false, String(e?.stack ?? e));
} finally {
  await browser.close();
  server.close();
}
process.exit(failed ? 1 : 0);
