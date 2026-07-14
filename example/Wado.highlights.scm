; Syntax-highlight query for Wado.g4 (Gale highlights.scm subset).
; Captures use the tree-sitter standard vocabulary; each becomes a CSS class.

; Comments, strings, numbers (lexer-rule names)
(LINE_COMMENT) @comment
(BLOCK_COMMENT) @comment

; The `__DATA__` data section: raw embedded text, muted like a comment.
(DATA_SECTION) @comment
(STRING_LITERAL) @string
(CHAR_LITERAL) @string
(FLOAT) @number
(INTEGER) @number

; Template strings: the literal chunks and the backtick delimiters are string;
; a `{ ... }` interpolation holds real code. Its identifiers read as variables,
; while keywords / numbers / nested strings keep their own colors. The trailing
; `:spec` format specifier is muted (listed before the interpolation rule so it
; wins over the identifier override for a spec like `{x:width}`).
(TEMPLATE_TEXT) @string
(BACKTICK) @string
(formatSpec (IDENTIFIER) @comment)
(formatSpec (INTEGER) @comment)
(formatSpec (FLOAT) @comment)
(interpolation (IDENTIFIER) @variable)

; Boolean / null constants
"true" @constant.builtin
"false" @constant.builtin
"null" @constant.builtin

; Keywords (inline literals)
"as" @keyword
"assert" @keyword
"async" @keyword
"break" @keyword
"const" @keyword
"continue" @keyword
"do" @keyword
"effect" @keyword
"else" @keyword
"enum" @keyword
"export" @keyword
"flags" @keyword
"fn" @keyword
"for" @keyword
"forward" @keyword
"from" @keyword
"global" @keyword
"if" @keyword
"impl" @keyword
"import" @keyword
"in" @keyword
"interface" @keyword
"let" @keyword
"loop" @keyword
"match" @keyword
"matches" @keyword
"mut" @keyword
"of" @keyword
"pub" @keyword
"reactive" @keyword
"resource" @keyword
"resume" @keyword
"return" @keyword
"self" @keyword
"stores" @keyword
"struct" @keyword
"task" @keyword
"test" @keyword
"trait" @keyword
"trap" @keyword
"type" @keyword
"unique" @keyword
"use" @keyword
"variant" @keyword
"while" @keyword
"with" @keyword
"world" @keyword
