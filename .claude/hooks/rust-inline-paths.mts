#!/usr/bin/env node
// PreToolUse hook for the editing tools: refuse a Rust edit that adds an item
// named through `crate::` or `super::` instead of imported.
// `mise run check-rust-paths` ratchets the same count once the edit has landed.

import { readFileSync } from "node:fs";

import { findInlinePaths } from "../../scripts/rust-inline-paths.mjs";

type ToolInput = {
  file_path?: string;
  old_string?: string;
  new_string?: string;
  replace_all?: boolean;
  content?: string;
};

/** The file as it stands, or "" when it is new or unreadable. */
function currentRust(path: string): string {
  try {
    return readFileSync(path, "utf8");
  } catch {
    return "";
  }
}

/**
 * The Rust before and after the edit. An Edit supplies a replacement rather
 * than a file, so the guard reads what it would land in: judging `new_string`
 * alone puts every path outside a `use` item it cannot see.
 */
export function editedRust(toolInput: ToolInput): { before: string; after: string } {
  const path = toolInput?.file_path;
  if (!path?.endsWith(".rs")) return { before: "", after: "" };
  if (toolInput.content !== undefined) return { before: currentRust(path), after: toolInput.content };
  if (toolInput.old_string === undefined || toolInput.new_string === undefined) {
    return { before: "", after: "" };
  }
  const before = currentRust(path);
  const after = toolInput.replace_all
    ? before.split(toolInput.old_string).join(toolInput.new_string)
    : before.replace(toolInput.old_string, toolInput.new_string);
  return { before, after };
}

/** Why the edit is denied, or null when it imports everything it adds. */
export function denialReason(toolInput: ToolInput): string | null {
  const { before, after } = editedRust(toolInput);
  if (after === before) return null;
  // The corpus predates the rule, so what the file already carries is the
  // baseline `--check` ratchets against; only what this edit adds is refused.
  const had = findInlinePaths(before).length;
  const hits = findInlinePaths(after);
  if (hits.length <= had) return null;
  const where = hits.map((hit) => `line ${hit.line}: ${hit.text}`).join(", ");
  return (
    `this edit adds a path it does not import (${where}). A \`crate::\` or` +
    " `super::` path belongs in a `use` item at the top of the module" +
    " (AGENTS.md > General Rules); where the item is read, write its name."
  );
}

if (import.meta.main) {
  let input = "";
  for await (const chunk of process.stdin) input += chunk;
  let reason: string | null = null;
  try {
    reason = denialReason(JSON.parse(input)?.tool_input);
  } catch {
    // A guard that cannot read the edit stays out of the way; the CI check
    // still sees what lands.
  }
  if (reason) {
    process.stdout.write(
      JSON.stringify({
        hookSpecificOutput: {
          hookEventName: "PreToolUse",
          permissionDecision: "deny",
          permissionDecisionReason: reason,
        },
      }),
    );
  }
}
