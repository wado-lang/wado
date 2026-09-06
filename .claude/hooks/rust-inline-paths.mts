#!/usr/bin/env node
// PreToolUse hook for the editing tools: refuse Rust text that names an item
// through `crate::` or `super::` instead of importing it. Clippy reports the
// `crate::` half and `mise run check-rust-paths` the `super::` half, but both
// speak after the edit lands; this speaks before it.

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
    `this edit names an item through ${hits.length === 1 ? "a path" : "paths"} it does not import` +
    ` (${where}). A \`crate::\` or \`super::\` path belongs in a \`use\` item at the top of the` +
    " module (AGENTS.md > General Rules); name the item itself where it is read."
  );
}

if (import.meta.main) {
  let input = "";
  for await (const chunk of process.stdin) input += chunk;
  let reason: string | null = null;
  try {
    reason = denialReason(JSON.parse(input)?.tool_input);
  } catch {
    // A guard that cannot read the edit stays out of the way: the CI check and
    // clippy still see whatever lands.
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
