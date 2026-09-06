; Syntax-highlight query for the host half of the composite (Gale
; highlights.scm subset). Captures use the tree-sitter standard vocabulary;
; each becomes a CSS class.
;
; Only the markup is classified here. A `<style>` / `<script>` body is lexed in
; its own mode, so `MiniCss.highlights.scm` and `MiniJs.highlights.scm` classify
; it — the three queries are concatenated into one table. Uncaptured tokens
; (TEXT) render as plain escaped text.

(TAG_OPEN) @tag
(TAG_CLOSE) @tag
(STYLE_OPEN) @tag
(STYLE_CLOSE) @tag
(SCRIPT_OPEN) @tag
(SCRIPT_CLOSE) @tag
