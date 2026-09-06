; Syntax-highlight query for MiniHtml.g4 (Gale highlights.scm subset).
; Captures use the tree-sitter standard vocabulary; each becomes a CSS class.
;
; The bodies are captured too, but `highlight.wado` cuts them out before this
; grammar ever sees them — a body reaches MiniCss / MiniJs instead. What is
; left here is the markup around them. Uncaptured tokens (TEXT) render as
; plain escaped text.

(TAG_OPEN) @tag
(TAG_CLOSE) @tag
(STYLE_OPEN) @tag
(STYLE_CLOSE) @tag
(SCRIPT_OPEN) @tag
(SCRIPT_CLOSE) @tag
