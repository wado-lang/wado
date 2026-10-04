---
name: markdown
description: "The rules for every Markdown file in the repository, inside docs/ or not: prose a reader understands on the first pass, MECE structure, headings rather than bold, checklists for TODOs. Read before writing or editing any Markdown (.md) file."
---

# Markdown

The goal is prose a reader understands on the first pass.

## Prose

- Plain words, one idea per sentence, the plain statement before its reason.
- Undo what makes a reader decode: a clause hung off a dash, an abstract noun
  where a verb would do, the clever phrasing before the obvious one.
- Shorter is not the goal. A passage that came out shorter and harder to follow
  has failed.

## Structure

- Simple and MECE.
- Sub-sections are headings, never `**bold**`.
- TODOs are a checklist (`- [ ]`, `- [x]`).

## Numbers

A number is trustworthy only where something regenerates it. Name the shape to
grep for rather than a count of sites, and keep measured figures in a table a
refresh re-measures. Figures that define a workload, commit messages, and pull
request bodies are exempt.

## Finish

Run `mise run format`.
