// What test262.mjs and test262-inputs.mjs share: where test262 is, and how
// its tests are walked.

import { readdirSync, statSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

export const root = fileURLToPath(new URL("..", import.meta.url));
export const corpus = join(root, "../vendor/test262");

// Each test under `dir`, in vendor/test262. The generated property tests list
// each property's code points: a Unicode version's data, which `core:icu`
// answers for Re.
export function* files(dir) {
  for (const name of readdirSync(dir).sort()) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) {
      if (name !== "generated") {
        yield* files(path);
      }
    } else if (name.endsWith(".js")) {
      yield path;
    }
  }
}

export function acceptsUnderU(pattern) {
  try {
    new RegExp(pattern, "u");
    return true;
  } catch {
    return false;
  }
}
