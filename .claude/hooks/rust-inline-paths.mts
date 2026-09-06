#!/usr/bin/env node
// PreToolUse hook for the editing tools: refuse Rust text that names an item
// through `crate::` or `super::` instead of importing it.
// `mise run check-rust-paths` reports the same thing once the edit has landed.

import { findInlinePaths } from "../../scripts/rust-inline-paths.mjs";

type ToolInput = { file_path?: string; new_string?: string; content?: string };

/** The text an edit would add, or "" when it adds no Rust. */
export function addedRust(toolInput: ToolInput): string {
  if (!toolInput?.file_path?.endsWith(".rs")) return "";
  return toolInput.new_string ?? toolInput.content ?? "";
}

/** Why the edit is denied, or null when it imports everything it names. */
export function denialReason(toolInput: ToolInput): string | null {
  const hits = findInlinePaths(addedRust(toolInput));
  if (hits.length === 0) return null;
  const where = hits.map((hit) => `line ${hit.line}: ${hit.text}`).join(", ");
  return (
    `this edit does not import everything it names (${where}). A \`crate::\` or` +
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
    // A guard that cannot read the edit stays out of the way; clippy and the CI
    // check still see what lands.
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
