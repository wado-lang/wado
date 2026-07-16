// Browser smoke test for the client-side playground. Serves web/ and drives a
// JSPI-capable Chromium to compile + transpile + run Wado in the page.
//
// Usage: node wado-playground/web/test-browser.mjs
// Requires playwright-core (mise run playground-web-test installs it) and a
// Chromium 137+. Set CHROME_PATH to override the browser binary.

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
  return undefined; // let playwright resolve its own download
}

const MIME = {
  ".html": "text/html",
  ".js": "text/javascript",
  ".mjs": "text/javascript",
  ".wasm": "application/wasm",
  ".json": "application/json",
};

const CASES = [
  {
    name: "hello",
    src: `use { println, Stdout } from "core:cli";\n\nexport fn run() with Stdout {\n    println("hello from browser");\n}\n`,
    expect: "hello from browser",
  },
  {
    name: "compute+float",
    src: `use { println, Stdout } from "core:cli";\n\nexport fn run() with Stdout {\n    let mut s = 0;\n    let mut i = 1;\n    while i <= 100 { s = s + i; i = i + 1; }\n    println(\`sum={s} pi={355.0 / 113.0}\`);\n}\n`,
    expect: "sum=5050 pi=3.14",
  },
  {
    // Exercises cli.js stream routing + per-stream decoders: writing to both
    // stdout and stderr must not cross channels, and a multi-byte glyph must
    // decode intact.
    name: "stdout+stderr+utf8",
    src: `use { println, eprintln, Stdout, Stderr } from "core:cli";\n\nexport fn run() with Stdout, Stderr {\n    println("out: café ☕");\n    eprintln("err: naïve");\n}\n`,
    expect: "out: café ☕",
    expectStderr: "err: naïve",
  },
];

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

const browser = await chromium.launch({ executablePath: chromePath(), args: ["--no-sandbox"] });
let failed = 0;
try {
  const page = await browser.newPage();
  page.on("pageerror", (e) => console.log("[pageerror]", e.message));
  await page.goto(`http://127.0.0.1:${port}/index.html`, { waitUntil: "load" });
  for (const c of CASES) {
    const r = await page.evaluate(async (source) => {
      try {
        return { ok: true, ...(await globalThis.__wadoRun(source)) };
      } catch (e) {
        return { ok: false, error: String(e?.stack ?? e) };
      }
    }, c.src);
    const pass =
      r.ok &&
      r.stdout.includes(c.expect) &&
      (c.expectStderr === undefined
        ? true
        : r.stderr.includes(c.expectStderr) && !r.stdout.includes(c.expectStderr));
    console.log(`${pass ? "✅" : "❌"} ${c.name}: ${JSON.stringify(r.ok ? { stdout: r.stdout, stderr: r.stderr } : r.error)}`);
    if (!pass) failed++;
  }

  // The same pipeline via runner-worker.js: streamed messages, then done.
  const w = await page.evaluate(async (source) => {
    const worker = new Worker("./runner-worker.js", { type: "module" });
    const events = [];
    const result = await new Promise((resolve, reject) => {
      setTimeout(() => reject(new Error("timeout: runner worker")), 120000);
      worker.onmessage = (e) => {
        events.push(e.data);
        if (e.data.type === "done" || e.data.type === "error") resolve(e.data);
      };
      worker.postMessage({ type: "run", source });
    }).finally(() => worker.terminate());
    return { events, result };
  }, CASES[2].src);
  const stdout = w.events.filter((e) => e.type === "stdout").map((e) => e.text).join("");
  const stderr = w.events.filter((e) => e.type === "stderr").map((e) => e.text).join("");
  const phases = w.events.filter((e) => e.type === "status").map((e) => e.phase);
  const workerPass =
    w.result.type === "done" &&
    stdout.includes(CASES[2].expect) &&
    stderr.includes(CASES[2].expectStderr) &&
    !stdout.includes(CASES[2].expectStderr) &&
    ["compiling", "transpiling", "running"].every((p) => phases.includes(p));
  console.log(`${workerPass ? "✅" : "❌"} runner-worker: ${JSON.stringify({ phases, stdout, stderr, result: w.result })}`);
  if (!workerPass) failed++;
} finally {
  await browser.close();
  server.close();
}
process.exit(failed ? 1 : 0);
