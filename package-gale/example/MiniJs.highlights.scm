; Syntax-highlight query for MiniJs.g4 (Gale highlights.scm subset).
;
; The one line a lexer-based highlighter cannot have:
;
;     (params (IDENT) @variable.parameter)
;
; `arrow` and `group` both open with `(`, so whether an identifier inside the
; parentheses lands under `params` is settled by the `=>` after the closing
; paren — arbitrarily many tokens later. A lexer classifies `a` when it reads
; it, before that token exists. The parser decides first, and the query reads
; the answer off the rule stack.

(params (IDENT) @variable.parameter)
(call (IDENT) @function)
(IDENT) @variable

(NUMBER) @number
(STRING) @string
(LINE_COMMENT) @comment

"let" @keyword
"=>" @operator
"=" @operator
"+" @operator
"*" @operator
"(" @punctuation.bracket
")" @punctuation.bracket
"," @punctuation.delimiter
";" @punctuation.delimiter
