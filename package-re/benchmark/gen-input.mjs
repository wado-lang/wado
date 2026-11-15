// Writes `input.txt`, the text every arm searches: prose with a name in three
// casings, and log lines carrying a timestamp, a URI and an email address.
// Seeded, so a rerun writes the same bytes.
//
// How to run:
//   node gen-input.mjs

import { writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

// One search of it stays well inside a phase's time budget on the slowest arm,
// so every arm averages several iterations.
const TARGET_BYTES = 64 * 1024;

// xorshift32: the input must not depend on the engine's `Math.random`.
let state = 0x2545f491;
function next(bound) {
  state ^= state << 13;
  state ^= state >>> 17;
  state ^= state << 5;
  state >>>= 0;
  return state % bound;
}
const pick = (list) => list[next(list.length)];

const WORDS = [
  "the", "a", "of", "and", "to", "in", "that", "was", "his", "with", "upon", "which", "had", "there",
  "study", "window", "letter", "morning", "carriage", "detective", "evidence", "street", "fog", "violin",
  "remarked", "observed", "answered", "glanced", "silence", "problem", "singular", "curious", "doctor",
  "café", "naïve", "東京", "手紙", "探偵",
];
const NAMES = ["Sherlock Holmes", "Sherlock Holmes", "SHERLOCK HOLMES", "sherlock holmes", "Dr. Watson"];
const USERS = ["alice", "bob.smith", "carol+news", "dave_99", "eve-admin", "frank.o.brien"];
const HOSTS = ["example.com", "mail.example.org", "wado-lang.dev", "sub.domain.co.uk", "localhost.test"];
const SCHEMES = ["https", "http", "ftp", "wss"];
const PATHS = ["", "/", "/index.html", "/api/v1/users/42", "/docs/spec-overview.md", "/a/b/c.tar.gz"];
const QUERIES = ["", "", "?q=regex", "?page=2&sort=asc", "?id=10#section-3", "#top"];
const METHODS = ["GET", "POST", "PUT", "DELETE"];
const STATUSES = ["200", "201", "301", "404", "500"];

const two = (n) => String(n).padStart(2, "0");

function prose() {
  const words = [];
  const count = 8 + next(12);
  for (let i = 0; i < count; i++) {
    words.push(next(12) === 0 ? pick(NAMES) : pick(WORDS));
  }
  return words.join(" ") + ".";
}

function log() {
  const stamp = `2026-${two(1 + next(12))}-${two(1 + next(28))}T${two(next(24))}:${two(next(60))}:${two(next(60))}Z`;
  const uri = `${pick(SCHEMES)}://${pick(HOSTS)}${pick(PATHS)}${pick(QUERIES)}`;
  return `${stamp} ${pick(METHODS)} ${uri} ${pick(STATUSES)} ${pick(USERS)}@${pick(HOSTS)}`;
}

const lines = [];
let bytes = 0;
while (bytes < TARGET_BYTES) {
  const line = next(4) === 0 ? log() : prose();
  lines.push(line);
  bytes += Buffer.byteLength(line) + 1;
}

const out = join(dirname(fileURLToPath(import.meta.url)), "input.txt");
writeFileSync(out, lines.join("\n") + "\n");
console.log(`${out}: ${bytes} bytes, ${lines.length} lines`);
