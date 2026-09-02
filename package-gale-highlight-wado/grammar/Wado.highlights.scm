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

; Identifiers the grammar can classify on its own, most specific first. An
; override matches anywhere under its rule and the first one declared wins, so
; a rule nested inside another is listed above it.
;
; Only a rule whose whole subtree is of one nature can carry an override.
; `typeRef` and `genericParam` hold nothing but types. For a member name the
; subtree is whatever `postfixOp` holds beside it, so `Wado.g4` wraps each use
; site in a one-token rule and those carry the captures instead.
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
; A member name is a name whichever word it is, and `memberName` accepts ~47 of
; them, keywords included. The whole rule carries the capture: the compiler
; reads every one of those words through its ordinary name path. `self` is the
; exception it reads lexically, wherever it stands.
(fieldName "self" @constant.builtin)
(methodName "self" @constant.builtin)
(methodName) @function.method
(fieldName) @property
; A `::` segment's IDENTIFIER stays uncoloured: `Option::None` and `Foo::new`
; are one shape that only name resolution splits. Its keywords are names all
; the same, and `Instant::from(x)` is the shape every `From` impl is called
; through. Listed are the words the compiler accepts as a segment: `identifier`
; holds the ones that lex as keywords, and the rest lex as identifiers there.
(pathSegment "from" @variable)
(pathSegment "of" @variable)
(pathSegment "type" @variable)
(pathSegment "flags" @variable)
(pathSegment "extends" @variable)
(pathSegment "test" @variable)
(pathSegment "do" @variable)
(pathSegment "task" @variable)
(pathSegment "trap" @variable)
(pathSegment "forward" @variable)
(pathSegment "resume" @variable)
; An interpolation holds ordinary code, so its names take the classes the rules
; above give them and nothing more. Painting the rest `@variable` would colour
; inside a template what the same name is left plain outside one.

; Every contextual keyword the `identifier` rule accepts as a name. None of them
; is a keyword there: `let type = 1` binds a variable and `fn from(…)` declares
; a function. The compiler agrees, colouring these words by the position the
; parse read them in rather than by how they lex. (`self` is absent from
; `identifier`: the language reserves it.)
;
; Listed word by word rather than as `(identifier) @variable`, which would also
; claim the IDENTIFIER token and outrank `typeRef` / `pathSegment` from further
; in, painting every type name and enum case a variable.
(identifier "from" @variable)
(identifier "of" @variable)
(identifier "type" @variable)
(identifier "matches" @variable)
(identifier "stores" @variable)
(identifier "world" @variable)
(identifier "interface" @variable)
(identifier "resource" @variable)
(identifier "import" @variable)
(identifier "export" @variable)
(identifier "reactive" @variable)
(identifier "unique" @variable)
(identifier "forward" @variable)
(identifier "trap" @variable)
(identifier "effect" @variable)
(identifier "flags" @variable)
(identifier "variant" @variable)
(identifier "test" @variable)
(identifier "do" @variable)
(identifier "task" @variable)
(identifier "extends" @variable)

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
