# gale-highlight-html Development Guide

A syntax highlighter for HTML that highlights embedded CSS and JavaScript as
those languages, built with Gale on the grammars-v4 grammars. The
`wado-package-standard` skill says where each kind of file goes.
[README.md](./README.md) is the user manual; Gale's own rules are in
[`package-gale/AGENTS.md`](../package-gale/AGENTS.md).

## Architecture

Gale generates one parser per grammar in `assets/grammar/` at build time: HTML,
CSS, JavaScript and JSON, each with its highlight query. The HTML parser lexes
a `<script>` or `<style>` body as one token; the library hands that text to the
parser of its language and merges the shifted captures with the markup's own.
`src/javascript.wado` is the Wado twin of the JavaScript grammar's Java
`superClass` bases. Each grammar is copied from grammars-v4 and keeps its
`// Source:` and `// License:` header.
