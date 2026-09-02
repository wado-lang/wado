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
; the compiler's own classifier paints. A `${ ... }` holds code; its `:spec`
; tail is formatting metadata, so it mutes.
(TEMPLATE_TEXT) @string
(BACKTICK) @string
(INTERP_OPEN) @string
(interpolationEnd "}" @string)

; A specifier is not code: `${x:>8.2}` must not colour `>` as an operator.
; A rule-context override outranks the token's default, so listing the atoms
; that double as operators is enough to mute them.
(formatSpec ":" @comment)
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

; Identifiers the grammar can classify on its own, most specific first: an
; override matches anywhere *under* its rule and the first one declared wins,
; so a rule nested inside another has to be listed above it. `formatSpec` is
; above these for that reason; `interpolation` is below them.
;
; Only a rule whose whole subtree is of one nature can carry an override.
; `typeRef` and `genericParam` hold nothing but types, and `memberName` is a
; leaf; the call and index forms are out, because `postfixOp` contains the
; argument list and `@function` there would repaint every argument.
;
; Telling a function from a variable takes name resolution, which no
; context-free grammar has. `mise run check-highlight` reports what stays
; uncoloured, by the kind the compiler resolved it to.
(formatSpec (IDENTIFIER) @comment)
; `stores[b]` names a parameter, not a type. It sits inside the `fn(…) with
; stores[b]` type that the rule below would otherwise paint.
(storesItem (IDENTIFIER) @variable)
(typeRef (IDENTIFIER) @type)
(genericParam (IDENTIFIER) @type)
; `.method()`, and `.field` with a struct literal's and a pattern's field name.
; Each captures its use-site wrapper rather than the shared `memberName`; see
; `Wado.g4`. A `::` segment goes through `pathSegment` and nothing captures it,
; because `Option::None` and `Foo::new` are one shape that only name resolution
; splits. The call form is a fact instead: the grammar matches its `(`.
;
; Each wrapper also spells out the contextual keywords usable as a name; only
; those need listing, because they are the ones the compiler lexes as plain
; identifiers (a real keyword stays a keyword on both sides).
(methodName (IDENTIFIER) @function.method)
(methodName "test" @function.method)
(methodName "do" @function.method)
(methodName "task" @function.method)
(methodName "trap" @function.method)
(methodName "forward" @function.method)
(fieldName (IDENTIFIER) @property)
(fieldName "test" @property)
(fieldName "do" @property)
(fieldName "task" @property)
(fieldName "trap" @property)
(fieldName "forward" @property)
; An interpolation holds ordinary code, so its names take the classes the rules
; above give them and nothing more. Painting the rest `@variable` would reach
; every identifier under `${…}`, the `::` segments included, and colour inside
; a template what the same name is left plain outside one.

; The contextual keywords the `identifier` rule also accepts as names. In that
; position they are not keywords — the compiler lexes them as identifiers and
; only recognises them by where they sit — so `let test = |…|` must not colour
; `test`. (`self` is absent from `identifier`: the language reserves it.)
(identifier "test" @variable)
(identifier "do" @variable)
(identifier "task" @variable)
(identifier "trap" @variable)
(identifier "forward" @variable)

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
"extends" @keyword
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
