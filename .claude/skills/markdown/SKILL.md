---
name: markdown
description: "The rules for every Markdown file in the repository, inside docs/ or not: prose a reader understands on the first pass, MECE structure, headings rather than bold, checklists for TODOs. Read before writing or editing any Markdown (.md) file."
---

# Markdown

The goal is prose a reader understands on the first pass. Everything below
serves that.

## Prose

- Plain words. One idea per sentence. The plain statement first, the reason for
  it after.
- Three habits make a reader decode instead of read: a second clause hung off a
  dash, an abstract noun standing where a verb would do, and the clever phrasing
  of a point arriving before the obvious one. Undo each where you find it.
- Correct and fresh. Keep the facts.
- Cutting narration and redundancy is one way to get there. It is not the point.
  A passage that came out shorter and harder to follow has failed.

## Structure

- Keep a document simple and MECE.
- Do not use `**...**` (bold) for sub-sections. Use Markdown sections instead.
- Use a Markdown checklist for TODOs (`- [ ] ...`) and what's done (`- [x] ...`).

## Numbers

A number is trustworthy only where something regenerates it.

- Never write a count of code sites ("29 `mem::replace` sites"). Name the shape
  to grep for, and the grep gives the number.
- A measured figure goes in a table that a refresh re-measures, never in the
  prose around it. A figure that defines the workload, such as a dataset size
  or a toolchain version, may stay in prose.
- Commit messages and pull request bodies describe one moment and are exempt.

## Finish

Run `mise run format` after editing.
