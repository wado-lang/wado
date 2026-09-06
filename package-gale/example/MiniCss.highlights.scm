; Syntax-highlight query for the CSS half of the composite (Gale highlights.scm
; subset). One of three queries riding into the same invocation; the generator
; concatenates them, so each language keeps its own file.
;
; Every capture names a token rule rather than a literal. A composite has one
; token space, so `";"` would be ambiguous across the languages sharing it —
; `CSS_SEMI` and `JS_SEMI` are both spelled `;`.
;
; One `CSS_IDENT` token, three classes. The rule-context form
; `(rule (TOKEN) @cap)` fires while `rule` is on the parse's rule stack, so the
; class each name gets is the position the parser gave it — not anything
; visible in the token.

(selector (CSS_IDENT) @type)
(property (CSS_IDENT) @property)
(value (CSS_IDENT) @constant)

(CSS_NUMBER) @number
(CSS_HASH) @constant
(CSS_COMMENT) @comment

(CSS_LBRACE) @punctuation.bracket
(CSS_RBRACE) @punctuation.bracket
(CSS_COLON) @punctuation.delimiter
(CSS_SEMI) @punctuation.delimiter
(CSS_COMMA) @punctuation.delimiter
