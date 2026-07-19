#!/usr/bin/env node
// SessionStart hook: restore the shared cargo registry cache from GCS.
//
// Best-effort: any failure (no key, no object yet, network) is logged and the
// session continues. The object is produced by .github/workflows/cargo-cache.yml
// and contains only registry/index + registry/cache (never credentials).
//
// Auth uses a read-only service-account key provided via the environment:
//   WADO_CACHE_SA_KEY       inline service-account JSON, or
//   WADO_CACHE_SA_KEY_FILE  path to the JSON key file
// Optional overrides: WADO_CACHE_BUCKET, WADO_CACHE_OBJECT.

import { createSign } from "node:crypto";
import { createWriteStream, readFileSync, mkdirSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { tmpdir, homedir } from "node:os";
import { join } from "node:path";

process.env.NODE_USE_ENV_PROXY ??= "1";

const BUCKET = process.env.WADO_CACHE_BUCKET ?? "wado-lang-cache";
const OBJECT = process.env.WADO_CACHE_OBJECT ?? "cargo-registry/linux-x86_64/registry.tar.gz";
const SCOPE = "https://www.googleapis.com/auth/devstorage.read_only";
const TIMEOUT_MS = 60_000;

const log = (m) => console.error(`[cargo-cache] ${m}`);

function loadKey() {
  const file = process.env.WADO_CACHE_SA_KEY_FILE;
  const inline = process.env.WADO_CACHE_SA_KEY;
  if (file) return JSON.parse(readFileSync(file, "utf8"));
  if (inline) return JSON.parse(inline);
  return null;
}

const b64url = (v) => Buffer.from(v).toString("base64url");

async function accessToken({ client_email, private_key, token_uri }) {
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

async function main() {
  const key = loadKey();
  if (!key) {
    log("no service-account key in env; skipping cache restore");
    return;
  }

  const token = await accessToken(key);
  const url =
    `https://storage.googleapis.com/storage/v1/b/${BUCKET}` +
    `/o/${encodeURIComponent(OBJECT)}?alt=media`;
  const res = await fetch(url, {
    headers: { Authorization: `Bearer ${token}` },
    signal: AbortSignal.timeout(TIMEOUT_MS),
  });
  if (res.status === 404) {
    log(`no cache object yet (gs://${BUCKET}/${OBJECT}); skipping`);
    return;
  }
  if (!res.ok) throw new Error(`download failed: HTTP ${res.status} ${await res.text()}`);

  const cargoHome = process.env.CARGO_HOME ?? join(homedir(), ".cargo");
  mkdirSync(cargoHome, { recursive: true });
  const tarPath = join(tmpdir(), "cargo-registry-cache.tar.gz");
  await pipeline(Readable.fromWeb(res.body), createWriteStream(tarPath));
  execFileSync("tar", ["-xzf", tarPath, "-C", cargoHome]);
  log(`restored gs://${BUCKET}/${OBJECT} into ${cargoHome}`);
}

main().catch((e) => log(`skipped: ${e.message}`));
