#!/usr/bin/env node
// SessionStart hook: restore the shared cargo caches from GCS.
//
// Two objects are restored (both produced by .github/workflows/cargo-cache.yml):
//   registry    -> $CARGO_HOME (index, .crate files, unpacked sources)
//   target-deps -> $CARGO_TARGET_DIR (the dependency half of target/)
//
// Unpacking these is what makes a session warm: cargo then compiles only the
// workspace crates. Workspace artifacts are deliberately absent — see
// scripts/pack-target-deps.sh.
//
// The manifest check below enforces the path parity cargo-cache.yml explains,
// and fails closed: artifacts built under other paths get replayed rather than
// rebuilt, so a mismatch skips the target restore and takes a cold build.
//
// Neither object contains credentials. Any failure (no key, no object yet,
// network) is logged and the session continues.
//
// Auth uses a read-only service-account key provided via the environment:
//   WADO_CACHE_SA_KEY_B64   base64 of the JSON key (for stores that reject raw JSON), or
//   WADO_CACHE_SA_KEY       inline service-account JSON, or
//   WADO_CACHE_SA_KEY_FILE  path to the JSON key file
// Optional overrides: WADO_CACHE_BUCKET, WADO_CACHE_OBJECT (registry),
//   WADO_CACHE_TARGET_OBJECT, WADO_CACHE_TARGET_MANIFEST.

import { createSign } from "node:crypto";
import { readFileSync, writeFileSync, mkdirSync, openSync, rmSync, existsSync } from "node:fs";
import { spawn, spawnSync, execFileSync } from "node:child_process";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { homedir } from "node:os";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

process.env.NODE_USE_ENV_PROXY ??= "1";

const BUCKET = process.env.WADO_CACHE_BUCKET ?? "wado-lang-cache";
const REGISTRY_OBJECT = process.env.WADO_CACHE_OBJECT ?? "cargo/registry/linux-x86_64.tar.gz";
const TARGET_OBJECT = process.env.WADO_CACHE_TARGET_OBJECT ?? "cargo/target-deps/linux-x86_64.tar.gz";
const TARGET_MANIFEST =
  process.env.WADO_CACHE_TARGET_MANIFEST ?? "cargo/target-deps/linux-x86_64.manifest.json";
const CARGO_HOME = process.env.CARGO_HOME ?? join(homedir(), ".cargo");
// CLAUDE_PROJECT_DIR is not always set, and a detached re-exec inherits whatever
// cwd it was launched with, so pin the root to this file's own location.
const REPO_ROOT =
  process.env.CLAUDE_PROJECT_DIR ?? resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const TARGET_DIR = process.env.CARGO_TARGET_DIR ?? join(REPO_ROOT, "target");
// The packer appends the manifest to the tarball last, so a copy inside
// target/ means an extraction ran to completion: the tree is already warm.
const RESTORED_MARKER = join(TARGET_DIR, "wado-cache-manifest.json");
// Held exclusively for the length of a build (the `.cargo-lock` beside it is
// only taken shared, so it says nothing about a build being under way).
const BUILD_LOCK = join(TARGET_DIR, "debug", ".cargo-build-lock");
const LOCK_BUSY_EXIT = 42;
const SCOPE = "https://www.googleapis.com/auth/devstorage.read_only";
const TIMEOUT_MS = 60_000;
// `AbortSignal.timeout` covers the streamed body, not just the response
// headers, so this bounds the whole multi-gigabyte download.
const DOWNLOAD_TIMEOUT_MS = 900_000;

type ServiceAccountKey = { client_email: string; private_key: string; token_uri: string };
type CacheManifest = {
  schema: number;
  repo_root: string;
  cargo_home: string;
  cargo_target_dir: string;
  rustc: string;
  commit: string;
};

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

const objectUrl = (object: string): string =>
  `https://storage.googleapis.com/storage/v1/b/${BUCKET}/o/${encodeURIComponent(object)}?alt=media`;

async function fetchObject(token: string, object: string, timeout: number): Promise<Response | null> {
  const res = await fetch(objectUrl(object), {
    headers: { Authorization: `Bearer ${token}` },
    signal: AbortSignal.timeout(timeout),
  });
  if (res.status === 404) {
    log(`no cache object yet (gs://${BUCKET}/${object}); skipping`);
    return null;
  }
  if (!res.ok) throw new Error(`download failed: HTTP ${res.status} ${await res.text()}`);
  return res;
}

// Unpack straight from the network into destDir. Staging the tarball first
// would double the peak draw on the session's fixed disk allowance, which is
// the resource under pressure here.
//
// `lock` is taken for the whole extraction, so a cargo that starts meanwhile
// waits for a complete tree instead of racing a half-written one, and a build
// already holding it makes the restore stand down. `keepExisting` decides the
// other half of that coexistence: whatever the container already put in
// destDir wins over the published copy.
type UntarOptions = { lock?: string; keepExisting?: boolean };

async function untar(
  body: ReadableStream<Uint8Array>,
  destDir: string,
  { lock, keepExisting }: UntarOptions = {},
): Promise<"extracted" | "lock-busy"> {
  // `--no-same-owner`: tar restores the archive's uid/gid when it runs as root,
  // which would leave a tree cargo writes into owned by the CI runner's uid.
  const tar = [
    "tar",
    "-xzf",
    "-",
    "--no-same-owner",
    ...(keepExisting ? ["--skip-old-files"] : []),
    "-C",
    destDir,
  ];
  const argv = lock
    ? ["flock", "--exclusive", "--nonblock", `--conflict-exit-code=${LOCK_BUSY_EXIT}`, lock, ...tar]
    : tar;

  const child = spawn(argv[0], argv.slice(1), { stdio: ["pipe", "ignore", "inherit"] });
  const exited = new Promise<number>((resolve, reject) => {
    child.on("error", reject);
    child.on("close", (code) => resolve(code ?? -1));
  });
  // A lock conflict exits before anything reads stdin, so the write fails with
  // EPIPE; the child's exit code is the verdict either way.
  let writeError: Error | null = null;
  const written = pipeline(Readable.fromWeb(body), child.stdin!).catch((e: Error) => {
    writeError = e;
  });

  const [code] = await Promise.all([exited, written]);
  if (code === LOCK_BUSY_EXIT) return "lock-busy";
  if (code !== 0) throw writeError ?? new Error(`${argv[0]} exited with code ${code}`);
  return "extracted";
}

async function restoreRegistry(token: string): Promise<void> {
  const res = await fetchObject(token, REGISTRY_OBJECT, DOWNLOAD_TIMEOUT_MS);
  if (!res) return;
  mkdirSync(CARGO_HOME, { recursive: true });
  await untar(res.body!, CARGO_HOME);
  log(`restored gs://${BUCKET}/${REGISTRY_OBJECT} into ${CARGO_HOME}`);
}

function rustcVersion(): string {
  const out = execFileSync("rustc", ["-vV"], { encoding: "utf8" });
  return out.split("\n").find((l) => l.startsWith("release: "))?.slice("release: ".length) ?? "";
}

function manifestMismatch(m: CacheManifest): string | null {
  if (m.schema !== 1) return `unknown manifest schema ${m.schema}`;
  if (m.repo_root !== REPO_ROOT) return `repo root ${m.repo_root} != ${REPO_ROOT}`;
  if (m.cargo_home !== CARGO_HOME) return `CARGO_HOME ${m.cargo_home} != ${CARGO_HOME}`;
  if (m.cargo_target_dir !== TARGET_DIR) return `target dir ${m.cargo_target_dir} != ${TARGET_DIR}`;
  const local = rustcVersion();
  if (m.rustc !== local) return `rustc ${m.rustc} != ${local}`;
  return null;
}

function buildInProgress(): boolean {
  const probe = spawnSync("flock", [
    "--exclusive",
    "--nonblock",
    `--conflict-exit-code=${LOCK_BUSY_EXIT}`,
    BUILD_LOCK,
    "true",
  ]);
  return probe.status === LOCK_BUSY_EXIT;
}

async function restoreTargetDeps(token: string): Promise<void> {
  // Existence says nothing: a resumed container carries whatever target/ its
  // image was snapshotted with, and restoring over it is the whole point.
  if (existsSync(RESTORED_MARKER)) {
    log(`${TARGET_DIR} already holds a restored cache; skipping target restore`);
    return;
  }

  // Probed before the download rather than only at extraction time: a build
  // that already owns target/ makes the whole transfer wasted bytes.
  mkdirSync(dirname(BUILD_LOCK), { recursive: true });
  if (buildInProgress()) {
    log(`a build already owns ${TARGET_DIR}; skipping target restore`);
    return;
  }

  const manifestRes = await fetchObject(token, TARGET_MANIFEST, TIMEOUT_MS);
  if (!manifestRes) return;
  const manifest = (await manifestRes.json()) as CacheManifest;
  const mismatch = manifestMismatch(manifest);
  if (mismatch) {
    log(`target cache does not match this container (${mismatch}); skipping — cold build`);
    return;
  }

  const res = await fetchObject(token, TARGET_OBJECT, DOWNLOAD_TIMEOUT_MS);
  if (!res) return;

  // The tarball is ordered artifacts-then-fingerprints, so an extraction cut
  // short leaves units cargo rebuilds rather than units it wrongly trusts —
  // and the next session's restore fills the gaps it left.
  const outcome = await untar(res.body!, TARGET_DIR, { lock: BUILD_LOCK, keepExisting: true });
  if (outcome === "lock-busy") {
    log(`a build already owns ${TARGET_DIR}; skipping target restore`);
    return;
  }
  log(`restored gs://${BUCKET}/${TARGET_OBJECT} into ${TARGET_DIR} (built at ${manifest.commit})`);
}

const DETACH_MARKER = "WADO_CACHE_RESTORE_DETACHED";
const LOG_FILE = join(homedir(), ".cache", "wado", "cargo-cache.log");
// Read by a second SessionStart. A pid rather than a flag keeps a killed
// restore from wedging it.
const RESTORE_MARKER = join(homedir(), ".cache", "wado", "cargo-cache-restore.running");

function restoreInFlight(): boolean {
  try {
    const pid = Number(readFileSync(RESTORE_MARKER, "utf8").trim());
    if (!Number.isInteger(pid) || pid <= 0) return false;
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

// The download outlives the harness's hook timeout, which would kill this
// script; re-exec detached so the session starts immediately instead.
function relaunchDetached(): boolean {
  if (process.env[DETACH_MARKER] === "1" || process.env.CLAUDE_CODE_REMOTE !== "true") return false;
  if (restoreInFlight()) {
    log("a cache restore is already running; leaving it to finish");
    return true;
  }
  try {
    mkdirSync(dirname(LOG_FILE), { recursive: true });
    const out = openSync(LOG_FILE, "a");
    const child = spawn(process.execPath, [fileURLToPath(import.meta.url)], {
      detached: true,
      stdio: ["ignore", out, out],
      env: { ...process.env, [DETACH_MARKER]: "1" },
    });
    child.unref();
    log(`restoring caches in the background; progress in ${LOG_FILE}`);
    return true;
  } catch (e) {
    log(`background restore launch failed, restoring inline: ${(e as Error).message}`);
    return false;
  }
}

async function main(): Promise<void> {
  if (relaunchDetached()) return;

  const key = loadKey();
  if (!key) {
    log("no service-account key in env; skipping cache restore");
    return;
  }

  const token = await accessToken(key);
  mkdirSync(dirname(RESTORE_MARKER), { recursive: true });
  writeFileSync(RESTORE_MARKER, String(process.pid));
  try {
    // Independent and best-effort: a missing target-deps object must not stop
    // the registry restore, and vice versa.
    const results = await Promise.allSettled([restoreRegistry(token), restoreTargetDeps(token)]);
    for (const r of results) {
      if (r.status === "rejected") log(`skipped: ${(r.reason as Error).message}`);
    }
  } finally {
    rmSync(RESTORE_MARKER, { force: true });
  }
}

main().catch((e: Error) => log(`skipped: ${e.message}`));
