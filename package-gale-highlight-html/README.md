# gale-highlight-html

Syntax highlighter for HTML that highlights what is embedded in a page as the
language it is written in. A `<script>` body is highlighted as JavaScript and a
`<style>` body as CSS. The parsers are generated at build time by
[Gale](../package-gale) from the [grammars-v4](https://github.com/antlr/grammars-v4)
grammars in `assets/grammar/`.
[WEP: Gale Highlight Query](../docs/wep-2026-07-12-gale-highlight-query.md)
holds the design of the highlight queries.

## Library

```wado
use { highlight } from "wado-lang:gale-highlight-html";

let fragment = highlight(src); // <span class="..."> HTML
```

`highlight(source: String) -> String` renders a page to an HTML fragment. It
never fails: an unparsable region is highlighted best-effort.

Classes use the tree-sitter capture vocabulary, so any tree-sitter theme
applies. A dotted capture like `string.regexp` becomes
`class="string regexp"`. Bring your own CSS and page shell, or use
`gale-highlight`'s `Theme` and `stylesheet`.

## How a Region Gets Its Language

The HTML grammar lexes a `<script>` or `<style>` body as one token, as a
browser's tokenizer does. `src/lib.wado` hands that text to the parser of its
language, shifts the captures to where the text sits in the page, and merges
them with the markup's own.

| Region                                                         | Language                      |
| -------------------------------------------------------------- | ----------------------------- |
| `<style>` body                                                 | CSS stylesheet                |
| `<script>` body, no `type`                                     | JavaScript                    |
| `type` of `module` or a JS MIME type                           | JavaScript                    |
| `type` of `importmap`, `speculationrules`, or a JSON MIME type | JSON                          |
| any other `type`                                               | plain text, as for a template |
| `style="..."` attribute                                        | CSS declarations              |
| `on*="..."` attribute                                          | JavaScript                    |

The opening `<script ...>` and `<style ...>` tags are one token to the grammar
too. They are lexed again with the element name masked, so their attributes
are captured like any other tag's.

## The JavaScript Base

`JavaScriptLexer.g4` and `JavaScriptParser.g4` name a `superClass`.
`src/javascript.wado` is the Wado twin of the upstream Java base classes. It
tells a division from a regular expression, closes a template interpolation at
its own brace, tracks `"use strict"`, and finds line breaks for automatic
semicolon insertion.

## Limits

An override in a highlight query fires while its rule is anywhere on the rule
stack. So only a rule whose whole subtree is a name can colour a name:

- A JavaScript property after `.` and an object key are coloured. A
  declaration's name is not, because its body sits under the same rule.
- A JSON key stays a string, because a nested object's key has the enclosing
  value on its rule stack.

## CLI

```sh
wado run package-gale-highlight-html \
    --output-dir build/highlight example/page.html
```

Writes the HTML fragment for each input to `build/highlight/<path>.html`. Add
`--standalone` (with `--theme light|dark`) for full themed pages.
