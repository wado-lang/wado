#!/usr/bin/env node
// SessionStart hook: restore the shared cargo caches from GCS.
//
// Two objects are restored (both produced by .github/workflows/cargo-cache.yml):
//   registry  -> $CARGO_HOME (registry/index + registry/cache; saves the download)
//   sccache   -> $SCCACHE_DIR (content-addressed compile cache; the `warm-cache`
//                mise task turns it into a warm target/ in ~80s instead of ~8min)
// Neither object contains credentials.
//
// Best-effort: any failure (no key, no object yet, network) is logged and the
// session continues.
//
// Auth uses a read-only service-account key provided via the environment:
//   WADO_CACHE_SA_KEY_B64   base64 of the JSON key (for stores that reject raw JSON), or
//   WADO_CACHE_SA_KEY       inline service-account JSON, or
//   WADO_CACHE_SA_KEY_FILE  path to the JSON key file
// Optional overrides: WADO_CACHE_BUCKET, WADO_CACHE_OBJECT (registry),
//   WADO_CACHE_SCCACHE_OBJECT, SCCACHE_DIR.

import { createSign } from "node:crypto";
import { createWriteStream, readFileSync, mkdirSync } from "node:fs";
import { execFileSync, spawn } from "node:child_process";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { tmpdir, homedir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

process.env.NODE_USE_ENV_PROXY ??= "1";

const BUCKET = process.env.WADO_CACHE_BUCKET ?? "wado-lang-cache";
const REGISTRY_OBJECT = process.env.WADO_CACHE_OBJECT ?? "cargo/registry/linux-x86_64.tar.gz";
const SCCACHE_OBJECT = process.env.WADO_CACHE_SCCACHE_OBJECT ?? "cargo/sccache/linux-x86_64.tar.gz";
const CARGO_HOME = process.env.CARGO_HOME ?? join(homedir(), ".cargo");
const SCCACHE_DIR = process.env.SCCACHE_DIR ?? join(CARGO_HOME, "sccache");
const SCOPE = "https://www.googleapis.com/auth/devstorage.read_only";
const TIMEOUT_MS = 60_000;

type ServiceAccountKey = { client_email: string; private_key: string; token_uri: string };

const log = (m: string): void => console.error(`[cargo-cache] ${m}`);

function loadKey(): ServiceAccountKey | null {
  const file = process.env.WADO_CACHE_SA_KEY_FILE;
  const b64 = process.env.WADO_CACHE_SA_KEY_B64;
  const inline = process.env.WADO_CACHE_SA_KEY;
  if (file) return JSON.parse(readFileSync(file, "utf8"));
  if (b64) return JSON.parse(Buffer.from(b64, "base64").toString("utf8"));
  if (inline) return JSON.parse(inline);
  return null;
}

const b64url = (v: string | Buffer): string => Buffer.from(v).toString("base64url");

async function accessToken({ client_email, private_key, token_uri }: ServiceAccountKey): Promise<string> {
  const now = Math.floor(Date.now() / 1000);
  const header = b64url(JSON.stringify({ alg: "RS256", typ: "JWT" }));
  const claim = b64url(
    JSON.stringify({ iss: client_email, scope: SCOPE, aud: token_uri, iat: now, exp: now + 3600 }),
  );
  const input = `${header}.${claim}`;
  const sig = createSign("RSA-SHA256").update(input).end().sign(private_key);
  const jwt = `${input}.${b64url(sig)}`;

  const res = await fetch(token_uri, {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      grant_type: "urn:ietf:params:oauth:grant-type:jwt-bearer",
      assertion: jwt,
    }),
    signal: AbortSignal.timeout(TIMEOUT_MS),
  });
  if (!res.ok) throw new Error(`token exchange failed: HTTP ${res.status} ${await res.text()}`);
  return (await res.json()).access_token;
}

async function restore(token: string, object: string, destDir: string, tarName: string): Promise<boolean> {
  const url =
    `https://storage.googleapis.com/storage/v1/b/${BUCKET}` +
    `/o/${encodeURIComponent(object)}?alt=media`;
  const res = await fetch(url, {
    headers: { Authorization: `Bearer ${token}` },
    signal: AbortSignal.timeout(TIMEOUT_MS),
  });
  if (res.status === 404) {
    log(`no cache object yet (gs://${BUCKET}/${object}); skipping`);
    return false;
  }
  if (!res.ok) throw new Error(`download failed: HTTP ${res.status} ${await res.text()}`);

  mkdirSync(destDir, { recursive: true });
  const tarPath = join(tmpdir(), tarName);
  await pipeline(Readable.fromWeb(res.body!), createWriteStream(tarPath));
  execFileSync("tar", ["-xzf", tarPath, "-C", destDir]);
  log(`restored gs://${BUCKET}/${object} into ${destDir}`);
  return true;
}

// Reconstruct target/ from the just-restored sccache cache, in the background,
// so the session is warm without anyone needing to run on-task-started first.
// Detached and non-blocking; warm-cache-bg.sh waits for the parallel mise-setup
// hook to install mise + sccache before it builds.
function launchBackgroundWarm(): void {
  if (process.env.CLAUDE_CODE_REMOTE !== "true") return;
  try {
    const script = join(dirname(fileURLToPath(import.meta.url)), "warm-cache-bg.sh");
    const child = spawn("bash", [script], { detached: true, stdio: "ignore" });
    child.unref();
    log("launched warm-cache in the background");
  } catch (e) {
    log(`warm-cache launch skipped: ${(e as Error).message}`);
  }
}

async function main(): Promise<void> {
  const key = loadKey();
  if (!key) {
    log("no service-account key in env; skipping cache restore");
    return;
  }

  const token = await accessToken(key);
  // Independent and best-effort: a missing sccache object must not stop the
  // registry restore, and vice versa.
  const [registry, sccache] = await Promise.allSettled([
    restore(token, REGISTRY_OBJECT, CARGO_HOME, "cargo-registry-cache.tar.gz"),
    restore(token, SCCACHE_OBJECT, SCCACHE_DIR, "cargo-sccache-cache.tar.gz"),
  ]);
  for (const r of [registry, sccache]) {
    if (r.status === "rejected") log(`skipped: ${(r.reason as Error).message}`);
  }
  if (sccache.status === "fulfilled" && sccache.value) launchBackgroundWarm();
}

main().catch((e: Error) => log(`skipped: ${e.message}`));
