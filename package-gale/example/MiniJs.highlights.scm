; Syntax-highlight query for the JS half of the composite (Gale highlights.scm
; subset). Captures name token rules, not literals — see `MiniCss.highlights.scm`.
;
; The one line a lexer-based highlighter cannot have:
;
;     (params (JS_IDENT) @variable.parameter)
;
; `arrow` and `group` both open with `(`, so whether an identifier inside the
; parentheses lands under `params` is settled by the `=>` after the closing
; paren — arbitrarily many tokens later. A lexer classifies `a` when it reads
; it, before that token exists. The parser decides first, and the query reads
; the answer off the rule stack.

(params (JS_IDENT) @variable.parameter)
(call (JS_IDENT) @function)
(JS_IDENT) @variable

(JS_NUMBER) @number
(JS_STRING) @string
(JS_COMMENT) @comment

(JS_LET) @keyword
(JS_ARROW) @operator
(JS_ASSIGN) @operator
(JS_PLUS) @operator
(JS_STAR) @operator
(JS_LPAREN) @punctuation.bracket
(JS_RPAREN) @punctuation.bracket
(JS_COMMA) @punctuation.delimiter
(JS_SEMI) @punctuation.delimiter
