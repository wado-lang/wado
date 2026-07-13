; Syntax-highlight query for JSON.g4 (Gale highlights.scm subset).
; Captures use the tree-sitter standard vocabulary; each becomes a CSS class.

(STRING) @string
(NUMBER) @number

"true" @constant.builtin
"false" @constant.builtin
"null" @constant.builtin

"{" @punctuation.bracket
"}" @punctuation.bracket
"[" @punctuation.bracket
"]" @punctuation.bracket
"," @punctuation.delimiter
":" @punctuation.delimiter
