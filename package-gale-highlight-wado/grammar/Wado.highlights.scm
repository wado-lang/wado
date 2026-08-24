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

; Template strings. Everything that is not code is string — backticks, text
; chunks, and the `${` / `}` that bracket an interpolation — matching what
; the compiler's own classifier paints. A `${ ... }` holds code (identifiers
; -> variable); its `:spec` tail is formatting metadata, so it mutes.
(TEMPLATE_TEXT) @string
(BACKTICK) @string
(INTERP_OPEN) @string
(interpolation "}" @string)
(interpolation (IDENTIFIER) @variable)

; A specifier is not code: `${x:>8.2}` must not colour `>` as an operator.
; A rule-context override outranks the token's default, so listing the atoms
; that double as operators is enough to mute them.
(interpolation ":" @comment)
(formatSpec (IDENTIFIER) @comment)
(formatSpec (INTEGER) @comment)
(formatSpec (FLOAT) @comment)
(formatSpec "<" @comment)
(formatSpec ">" @comment)
(formatSpec "^" @comment)
(formatSpec "+" @comment)
(formatSpec "-" @comment)
(formatSpec "*" @comment)

; Constants: `true` / `false` / `null` / `self` are `KeywordCategory::Constant`
; in the compiler's registry, not keywords.
"true" @constant.builtin
"false" @constant.builtin
"null" @constant.builtin
"self" @constant.builtin

; `matches` lexes as a keyword but is a binary pattern-test operator.
"matches" @operator

; Operators, matching the compiler's `is_highlight_operator` set. `&` / `|`
; (references, unions, closure params) and `::` / `?` / `..` / `...` double as
; punctuation and stay uncoloured on both sides. `>>` is absent because the
; grammar spells a shift `'>' '>'` so `List<Box<i32>>` can close.
; comparison
"==" @operator
"!=" @operator
"<=" @operator
">=" @operator
"<" @operator
">" @operator
; logical
"&&" @operator
"||" @operator
"!" @operator
; arithmetic
"+" @operator
"-" @operator
"*" @operator
"/" @operator
"%" @operator
; bitwise
"^" @operator
"~" @operator
"<<" @operator
; assignment
"+=" @operator
"-=" @operator
"*=" @operator
"/=" @operator
"%=" @operator
"&=" @operator
"|=" @operator
"^=" @operator
"<<=" @operator
">>=" @operator
"=" @operator
; arrows and bounded ranges
"->" @operator
"=>" @operator
"..<" @operator
"..=" @operator

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
"internal" @keyword
"let" @keyword
"loop" @keyword
"match" @keyword
"mut" @keyword
"of" @keyword
"pub" @keyword
"reactive" @keyword
"resource" @keyword
"resume" @keyword
"return" @keyword
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
