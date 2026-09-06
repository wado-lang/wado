; Syntax-highlight query for the JS half of the composite (Gale highlights.scm
; subset). Captures name token rules, not literals — see `MiniCss.highlights.scm`.
;
; The one line a lexer-based highlighter cannot have:
;
;     (params (JS_IDENT) @variable.parameter)
;
; `arrow` and `group` both open with `(`, so whether an identifier inside the
; parentheses lands under `params` is settled by the `=>` after the closing
; paren. Two things stand between a highlighter and that answer.
;
; A lexer classifies an identifier when it reads it, before the deciding token
; exists, and no mode-stack state brings it closer. That rules out colouring
; from a token stream.
;
; A regex highlighter does look ahead, and `\(([^)]*)\)\s*=>` would settle a
; flat parameter list. But a default value nests parentheses, so finding the
; closing paren means matching brackets, which no regular expression does:
;
;     let add = (a, b = (1 + 2)) => a + b;   ; a, b are parameters
;     let one = ((a));                       ; a is a variable
;
; `[^)]*` stops at the inner `)` in the first line and misses the `=>`. The
; parser matches the brackets, and the query reads the answer off the rule
; stack.

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
