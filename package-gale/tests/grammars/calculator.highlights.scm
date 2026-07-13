; Syntax-highlight query for calculator.g4 (Gale highlights.scm subset).
; This grammar names its operator/paren tokens, so match them by rule name.

(COS) @function.builtin
(SIN) @function.builtin
(TAN) @function.builtin
(ACOS) @function.builtin
(ASIN) @function.builtin
(ATAN) @function.builtin
(LN) @function.builtin
(LOG) @function.builtin
(SQRT) @function.builtin

(LPAREN) @punctuation.bracket
(RPAREN) @punctuation.bracket
(COMMA) @punctuation.delimiter

(PLUS) @operator
(MINUS) @operator
(TIMES) @operator
(DIV) @operator
