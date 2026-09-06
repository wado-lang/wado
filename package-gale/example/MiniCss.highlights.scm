; Syntax-highlight query for MiniCss.g4 (Gale highlights.scm subset).
;
; One `IDENT` token, three classes. The rule-context form `(rule (TOKEN) @cap)`
; fires while `rule` is on the parse's rule stack, so the class each name gets
; is the position the parser gave it — not anything visible in the token.

(selector (IDENT) @type)
(property (IDENT) @property)
(value (IDENT) @constant)

(NUMBER) @number
(HASH) @constant
(BLOCK_COMMENT) @comment

"{" @punctuation.bracket
"}" @punctuation.bracket
":" @punctuation.delimiter
";" @punctuation.delimiter
"," @punctuation.delimiter
