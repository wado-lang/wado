// ANTLR4 grammar for Wado, consumed by Gale.
//
// It is checked against the compiler's own parser: `mise run check-grammar`
// parses the stdlib, the e2e fixtures, the format fixtures, and the examples
// with both and reports every file they disagree about, against the committed
// `grammar/divergences.tsv`. The compiler is the specification; this file
// follows it.
//
// Two kinds of disagreement are expected and recorded there: rules no
// context-free grammar can state (a chained `!=`, `internal` next to `pub`,
// `#[serde]` spelled out in a diagnostic), which this grammar accepts and the
// parser rejects; and the one construct it cannot reach, a fn type's bare
// multi-effect row (see `fnTypeWithClause`).
//
// Effect *declarations* (`effect E { ... }`) are not yet modeled.

grammar Wado;

sourceFile
    : innerAttribute* item* EOF
    ;

item
    : attribute* itemModifiers itemKind
    ;

// Every item reads the same modifier prefix before its keyword decides which
// item it is, so the dispatch stays token-led. Which combinations mean
// something is a later question: `internal` with `pub` / `export`, or `async`
// on anything but a `fn`, is rejected past the grammar.
itemModifiers
    : 'internal'? 'pub'? 'export'? 'async'?
    ;

itemKind
    : useDecl
    | globalDecl
    | typeAliasDecl
    | structDecl
    | enumDecl
    | variantDecl
    | flagsDecl
    | traitDecl
    | interfaceDecl
    | worldDecl
    | resourceDecl
    | implBlock
    | testDecl
    | 'fn' funcSig
    ;

globalDecl
    : 'global' 'mut'? IDENTIFIER ':' typeRef '=' expression ';'
    ;

// A named alias / builtin declaration (`type Name<T> = T;`, `type Name;`), or
// a declaration whose head is a type the name grammar cannot spell —
// `type ();`, `type !;`, `type [..T];`.
typeAliasDecl
    : 'type' identifier genericParams? ('=' typeRef)? ';'
    | 'type' typeRef ';'
    ;

// `test` is a contextual keyword; modeled as a literal here for simplicity.
testDecl
    : 'test' STRING_LITERAL? block
    ;

attribute
    : '#' '[' IDENTIFIER attrArgs? ']'
    ;

innerAttribute
    : '#' '!' '[' IDENTIFIER attrArgs? ']'
    ;

attrArgs
    : '(' (attrArg (',' attrArg)*)? ')'
    ;

attrArg
    : IDENTIFIER ('=' attrValue)?
    | attrValue
    ;

attrValue
    : literal
    | IDENTIFIER
    | '[' (attrValue (',' attrValue)*)? ']'
    ;

useDecl
    : 'use' importGroup 'from' STRING_LITERAL ('with' braceLiteral)? ';'
    ;

importGroup
    : '{' importList? '}'
    | IDENTIFIER
    | '_'
    ;

importList
    : importItem (',' importItem)* ','?
    ;

importItem
    : IDENTIFIER '::' '{' importList? '}'
    | IDENTIFIER ('as' IDENTIFIER)?
    ;

// A function in member position (trait / interface / resource body), where the
// modifier prefix is part of the member rather than of an enclosing `item`.
// Spelled out rather than referring to `itemModifiers`: an all-optional rule
// reference at the head of an alternative defeats the member dispatch.
functionDecl
    : 'internal'? 'pub'? 'export'? 'async'? 'fn' funcSig
    ;

funcSig
    : identifier genericParams? '(' paramList? ')' returnType? withClause? (block | ';')
    ;

// A binding name: a plain identifier or one of the contextual keywords that
// may also name a function / field / parameter (`fn from`, `type: T`).
identifier
    : IDENTIFIER
    | 'from' | 'of' | 'type' | 'matches' | 'stores' | 'world'
    | 'interface' | 'resource' | 'import' | 'export' | 'reactive'
    | 'unique' | 'forward' | 'trap' | 'effect' | 'flags' | 'variant'
    // Words this grammar spells as literals but the lexer never reserves:
    // they are contextual keywords, so `fn test(...)` names a function.
    | 'test' | 'do' | 'task'
    ;

paramList
    : param (',' param)* ','?
    ;

param
    : selfParam
    | 'mut'? identifier ':' typeRef ('=' expression)?
    ;

selfParam
    : '&' 'mut'? 'self'
    | 'self' (':' typeRef)?
    ;

returnType
    : '->' typeRef
    ;

withClause
    : 'with' withItem (',' withItem)*
    ;

withItem
    : IDENTIFIER
    | 'stores' '[' (storesItem (',' storesItem)* ','?)? ']'
    ;

storesItem
    : IDENTIFIER
    | INTEGER
    | 'self'
    ;

structDecl
    : 'struct' IDENTIFIER genericParams? '{' fieldList? '}'
    ;

fieldList
    : fieldDecl (',' fieldDecl)* ','?
    ;

fieldDecl
    : attribute* ('pub' | 'internal')? identifier ':' typeRef ('=' expression)?
    ;

enumDecl
    : 'enum' IDENTIFIER '{' enumCaseList? '}'
    ;

enumCaseList
    : enumCase (',' enumCase)* ','?
    ;

enumCase
    : attribute* IDENTIFIER ('=' expression)?
    ;

flagsDecl
    : 'flags' IDENTIFIER '{' flagsCaseList? '}'
    ;

flagsCaseList
    : flagsCase (',' flagsCase)* ','?
    ;

flagsCase
    : attribute* IDENTIFIER
    ;

variantDecl
    : 'variant' IDENTIFIER genericParams? '{' variantCaseList? '}'
    ;

variantCaseList
    : variantCase (',' variantCase)* ','?
    ;

variantCase
    : attribute* IDENTIFIER ('(' typeRef (',' typeRef)* ')')?
    ;

traitDecl
    : 'trait' IDENTIFIER genericParams? (':' traitBounds)? '{' traitMember* '}'
    ;

// A WIT-style interface: a named set of function signatures (and associated
// types), modeled with the same members as a trait.
interfaceDecl
    : 'interface' IDENTIFIER genericParams? '{' traitMember* '}'
    ;

// A WIT-style world: a set of `import` / `export` items naming interfaces.
worldDecl
    : 'world' IDENTIFIER '{' worldItem* '}'
    ;

worldItem
    : 'import' IDENTIFIER ';'
    | 'export' ('async'? 'fn' identifier genericParams? '(' paramList? ')' returnType? | IDENTIFIER) ';'
    ;

// A WIT-style resource: an opaque handle with method / static-function
// signatures (or a bodyless unit resource `resource X;`).
resourceDecl
    : 'resource' IDENTIFIER genericParams? ('{' resourceMember* '}' | ';')
    ;

resourceMember
    : attribute* functionDecl
    ;

traitMember
    : attribute* traitMemberBody
    ;

traitMemberBody
    : 'type' IDENTIFIER (':' traitBounds)? ';'
    | functionDecl
    ;

implBlock
    : 'impl' genericParams? typeRef ('for' typeRef)? ('{' implMember* '}' | ';')
    ;

implMember
    : attribute* implMemberBody
    ;

implMemberBody
    : 'type' IDENTIFIER '=' typeRef ';'
    | '..' ('trap' | 'forward')
    | 'export' 'async'? 'fn' funcSig
    | ('pub' | 'internal')? implPubMember
    ;

// `const` / `fn` members share an optional `pub`, left-factored here so the
// dispatch is token-led (`const` vs `fn`) instead of a tournament.
implPubMember
    : 'const' IDENTIFIER ':' typeRef '=' expression ';'
    | 'fn' funcSig
    ;

genericParams
    : '<' genericParam (',' genericParam)* '>'
    ;

genericParam
    : '..'? 'effect'? IDENTIFIER (':' traitBounds)? ('=' typeRef)?
    ;

traitBounds
    : typeRef ('+' typeRef)*
    ;

typeRef
    : '&' 'mut'? typeRef
    | '!'
    | '_'
    | '(' (typeRef (',' typeRef)*)? ')'
    | '[' (typeElement (',' typeElement)*)? ']'
    | 'fn' 'mut'? '(' (typeRef (',' typeRef)*)? ')' returnType? fnTypeWithClause?
    | path typeArgs?
    ;

typeElement
    : '..'? typeRef
    ;

// A fn-*type*'s effect clause takes a parenthesized row, or a single bare
// effect. The parser also continues a bare row across a comma when the next
// item is an effect name rather than a parameter — it decides by looking three
// tokens ahead for the `ident:` that starts a parameter. A grammar cannot
// state that negative lookahead, so the bare multi-effect row is the one
// construct this file knowingly rejects (see `grammar/divergences.tsv`).
fnTypeWithClause
    : 'with' ('(' withItem (',' withItem)* ')' | withItem)
    ;

typeArgs
    : '<' typeArg (',' typeArg)* '>'
    ;

// A type argument, optionally an associated-type binding (`Iterator<Item = T>`)
// or — in an `impl` head only — a bound the parser lifts into the block's
// generic parameters (`impl List<T: Ord>`).
typeArg
    : IDENTIFIER '=' typeRef
    | IDENTIFIER ':' traitBounds
    | typeRef
    ;

path
    : IDENTIFIER ('::' IDENTIFIER)*
    ;

// Any keyword may be a field/method name after `.` (e.g. `entry.type`).
memberName
    : IDENTIFIER
    | 'use' | 'from' | 'as' | 'fn' | 'with' | 'let' | 'mut' | 'return'
    | 'if' | 'else' | 'match' | 'for' | 'while' | 'loop' | 'break'
    | 'continue' | 'in' | 'of' | 'pub' | 'effect' | 'interface'
    | 'reactive' | 'unique' | 'struct' | 'enum' | 'variant' | 'flags'
    | 'type' | 'impl' | 'trait' | 'resource' | 'world' | 'async'
    | 'import' | 'export' | 'assert' | 'global' | 'const' | 'matches'
    | 'stores' | 'true' | 'false' | 'null' | 'trap' | 'forward'
    | 'test' | 'do' | 'task'
    ;

// Semicolons separate statements, so the last one in a block may drop its
// `;`: a bare expression is the block's value (`{ 1 }`), and a jump
// (`return` / `break` / `continue` / `task return`) simply ends it.
block
    : '{' statement* blockTail? '}'
    ;

blockTail
    : 'return' expression?
    | 'task' 'return' expression?
    | 'break' (IDENTIFIER (':' expression)?)?
    | 'continue'
    | expression
    ;

statement
    : letStatement
    | returnStatement
    | taskReturnStatement
    | ifStatement
    | forStatement
    | whileStatement
    | loopStatement
    | breakStatement
    | continueStatement
    | assertStatement
    | matchStatement
    | withStatement
    | labeledBlock
    | localItem
    | exprStatement
    ;

// Item declarations nested in a block (e.g. a helper `struct` / `fn` inside a
// `test` block or function body).
localItem
    : attribute* localItemKind
    ;

localItemKind
    : structDecl
    | enumDecl
    | variantDecl
    | flagsDecl
    | traitDecl
    | implBlock
    | typeAliasDecl
    | 'fn' funcSig
    ;

// A labeled block `label: { ... }`, exited with `break label` (optionally
// yielding a value: `break label: expr`). Also usable in expression position.
labeledBlock
    : IDENTIFIER ':' block
    ;

letStatement
    : 'let' letBinding
    | 'reactive' 'let' letBinding
    ;

// The initializer may be omitted (`let x: i32;`), in which case the type
// annotation carries the type — a rule stated past the grammar.
letBinding
    : pattern (':' typeRef)? '=' expression ('else' block)? ';'
    | pattern ':' typeRef ';'
    ;

assertStatement
    : 'assert' expression (',' expression)? ';'
    ;

returnStatement
    : 'return' expression? ';'
    ;

// `task return` yields from a Wasm async function.
taskReturnStatement
    : 'task' 'return' expression? ';'
    ;

// `resume` yields a value from an effect handler back to the suspended call.
// It is an expression, so it is also a handler body's tail value (`resume x`
// with no `;`); `resume x;` reaches it through `exprStatement`.
resumeExpr
    : 'resume' expression
    ;

ifStatement
    : 'if' condition block ('else' (ifStatement | block))?
    ;

// `if`/`while` headers admit an optional `let` binding (and let-chains via
// `&&`, folded into the trailing expression). `exprNoStruct` keeps a bare `{`
// reserved for the body.
condition
    : conditionTerm ('&&' conditionTerm)*
    ;

conditionTerm
    : 'let' pattern '=' exprNoStruct
    | exprNoStruct
    ;

forStatement
    : 'for' forHead block
    ;

// `for let x of expr`, or a C-style `for [init]; [cond]; [step]` whose init
// (`let ...`), condition, and step are each optional (`for ; cond; step`).
forHead
    : 'let' pattern forTail
    | ';' condition? ';' exprNoStruct?
    ;

forTail
    : 'of' exprNoStruct
    | (':' typeRef)? '=' expression ';' condition? ';' exprNoStruct?
    ;

whileStatement
    : 'while' condition block
    ;

loopStatement
    : 'loop' block
    ;

breakStatement
    : 'break' (IDENTIFIER (':' expression)?)? ';'
    ;

continueStatement
    : 'continue' ';'
    ;

matchStatement
    : matchExpr
    ;

withStatement
    : withExpr
    ;

exprStatement
    : expression ';'
    ;

// Precedence by left recursion, lowest to highest.
expression
    : expression ('=' | '+=' | '-=' | '*=' | '/=' | '%=' | '&=' | '|=' | '^=' | '<<=' | '>>=') expression
    | expression ('..<' | '..=') expression
    | expression '||' expression
    | expression '&&' expression
    // Logical `!` binds looser than `matches`, so `!x matches { P }` reads as
    // "does not match"; the value-producing unaries live down in `unary`.
    | '!' expression
    | expression 'matches' '{' pattern ('&&' expression)? '}'
    | expression '|' expression
    | expression '^' expression
    | expression '&' expression
    | expression ('==' | '!=') expression
    | expression ('<' | '<=' | '>' | '>=') expression
    | expression ('<<' | '>' '>') expression
    | expression ('+' | '-') expression
    | expression 'as' typeRef
    | expression ('*' | '/' | '%') expression
    | unary
    ;

unary
    : ('-' | '~' | '&' '&'? 'mut'? | '*') unary
    | postfix
    ;

postfix
    : primary postfixOp*
    ;

postfixOp
    : '(' argumentList? ')'
    | '::' typeArgs '(' argumentList? ')'
    | '.' (memberName ('::' typeArgs)? ('(' argumentList? ')')? | INTEGER | FLOAT)
    | '[' expression ']'
    | '?'
    ;

argumentList
    : expression (',' expression)* ','?
    ;

primary
    : literal
    | 'self'
    | compileTimeExpr
    | resumeExpr
    | structLiteral
    | braceLiteral
    | block
    | exprPath
    | tupleOrArrayLiteral
    | closure
    | ifExpr
    | matchExpr
    | withExpr
    | labeledBlock
    | '(' expression? ')'
    ;

// Effect-handler installation: `with Effect => handler do { ... }` (one or
// more bindings). The `do` block is the handled scope, and the whole form is
// an expression whose value is the block's value.
withExpr
    : 'with' withBinding (',' withBinding)* 'do' block
    ;

withBinding
    : typeRef '=>' expression
    | expression
    ;

// Brace literal: `{}`, `{ x, y }` (field shorthand), `{ key: value, ..base }`.
// Read as an implicit struct literal, or as a map by context; `use ... with
// { ... }` configures a generator with the same shape. Excluded from
// `primaryNoStruct` because a `{` after an `if`/`while`/`for` header opens the
// body.
braceLiteral
    : '{' fieldInitList? '}'
    ;

// Expression-position path, supporting interspersed turbofish segments:
// `Stream::<u8>::new`, `Future::<Result<(), E>>::new`, `JsonValue::Bool`.
// A `::` segment may be any `memberName` (not just `IDENTIFIER`) so that a
// keyword method name resolves there, e.g. `Instant::from(x)` — mirroring how
// `.from` is already accepted after `.`.
exprPath
    : identifier ('::' (typeArgs | memberName))*
    ;

// Compile-time literals and macros: `#file`, `#include_str("...")`.
compileTimeExpr
    : '#' IDENTIFIER ('(' argumentList? ')')?
    ;

// `expression` minus struct literals, for `if` / `while` / `for` headers
// where a `{` opens the body. Mirrors the chain with a struct-free leaf.
exprNoStruct
    : exprNoStruct ('=' | '+=' | '-=' | '*=' | '/=' | '%=' | '&=' | '|=' | '^=' | '<<=' | '>>=') exprNoStruct
    | exprNoStruct ('..<' | '..=') exprNoStruct
    | exprNoStruct '||' exprNoStruct
    | exprNoStruct '&&' exprNoStruct
    // Logical `!` binds looser than `matches`, so `!x matches { P }` reads as
    // "does not match"; the value-producing unaries live down in `unaryNoStruct`.
    | '!' exprNoStruct
    | exprNoStruct 'matches' '{' pattern ('&&' expression)? '}'
    | exprNoStruct '|' exprNoStruct
    | exprNoStruct '^' exprNoStruct
    | exprNoStruct '&' exprNoStruct
    | exprNoStruct ('==' | '!=') exprNoStruct
    | exprNoStruct ('<' | '<=' | '>' | '>=') exprNoStruct
    | exprNoStruct ('<<' | '>' '>') exprNoStruct
    | exprNoStruct ('+' | '-') exprNoStruct
    | exprNoStruct 'as' typeRef
    | exprNoStruct ('*' | '/' | '%') exprNoStruct
    | unaryNoStruct
    ;

unaryNoStruct
    : ('-' | '~' | '&' '&'? 'mut'? | '*') unaryNoStruct
    | postfixNoStruct
    ;

postfixNoStruct
    : primaryNoStruct postfixOp*
    ;

primaryNoStruct
    : literal
    | 'self'
    | compileTimeExpr
    | resumeExpr
    | exprPath
    | tupleOrArrayLiteral
    | closure
    | ifExpr
    | matchExpr
    | '(' expression? ')'
    ;

structLiteral
    : path '{' fieldInitList? '}'
    ;

fieldInitList
    : fieldInit (',' fieldInit)* ','?
    ;

// A string literal names a field too, for JSON-shaped literals.
fieldInit
    : (memberName | STRING_LITERAL) (':' expression)?
    | '..' expression
    ;

tupleOrArrayLiteral
    : '[' 'for' 'let' pattern 'of' exprNoStruct '{' expression '}' ']'
    | '[' (arrayElement (',' arrayElement)* ','?)? ']'
    ;

arrayElement
    : '..' expression
    | expression
    ;

closure
    : ('||' | '|' closureParamList? '|') returnType? (block | expression)
    ;

closureParamList
    : closureParam (',' closureParam)* ','?
    ;

closureParam
    : 'mut'? ('_' | IDENTIFIER) closureParamType?
    ;

// The default is only reachable behind an annotation, so the two optionals
// nest rather than sit side by side at the end of one alternative.
closureParamType
    : ':' typeRef ('=' closureDefault)?
    ;

// A closure parameter's default binds no looser than `^`, so the closing `|`
// terminates it instead of reading as bitwise-or. Parenthesize a default that
// needs `|`.
closureDefault
    : closureDefault '^' closureDefault
    | closureDefault '&' closureDefault
    | closureDefault ('==' | '!=') closureDefault
    | closureDefault ('<' | '<=' | '>' | '>=') closureDefault
    | closureDefault ('<<' | '>' '>') closureDefault
    | closureDefault ('+' | '-') closureDefault
    | closureDefault 'as' typeRef
    | closureDefault ('*' | '/' | '%') closureDefault
    | unary
    ;

// An `if` in expression position may drop its `else` (`let x: i32 = if c { 1 };`).
// Spelled as two alternatives rather than an optional `else` group: the
// optional form stops at the first block and leaves a trailing `else` stranded.
ifExpr
    : 'if' condition block 'else' (ifExpr | block)
    | 'if' condition block
    ;

// The comma between arms is optional, whatever shape the arm body has.
matchExpr
    : 'match' exprNoStruct '{' (matchArm ','?)* '}'
    ;

matchArm
    : pattern ('&&' expression)? '=>' (block | ifStatement | 'return' expression? | expression)
    ;

// Or-patterns: `A | B | C`, as used in `match` arms and `matches { ... }`.
// Each alternative may be a range (`0..=9`, `'a'..<'z'`).
pattern
    : patternRange ('|' patternRange)*
    ;

patternRange
    : patternPrimary (('..<' | '..=') patternPrimary)?
    ;

patternPrimary
    : '_'
    | 'mut'? identifier
    | literal
    | '-' INTEGER
    | 'mut'? patternPath ('(' (pattern (',' pattern)*)? ')')?
    | 'mut'? path? '{' patternFieldList? '}'
    | 'mut'? '(' (pattern (',' pattern)*)? ')'
    | 'mut'? '[' patternElements? ']'
    ;

// A case path whose qualifier may be a generic type: `Maybe<i32>::Some(x)`,
// `Result<i32, E>::Ok(v)`.
patternPath
    : identifier typeArgs? ('::' (typeArgs | identifier))*
    ;

patternElements
    : '..'
    | pattern (',' pattern)* (',' '..')? ','?
    ;

patternFieldList
    : patternField (',' patternField)* (',' '..')? ','?
    ;

patternField
    : (memberName | STRING_LITERAL) (':' pattern)?
    ;

literal
    : INTEGER
    | FLOAT
    | STRING_LITERAL
    | templateString
    | CHAR_LITERAL
    | 'true'
    | 'false'
    | 'null'
    ;

// A template string alternates literal text with `${ ... }` interpolations. An
// interpolation's expression is lexed in the default mode (see mode TEMPLATE),
// so it highlights as real code, not string text.
templateString
    : BACKTICK templatePart* BACKTICK
    ;

templatePart
    : TEMPLATE_TEXT
    | interpolation
    ;

interpolation
    : INTERP_OPEN expression (':' formatSpec)? '}'
    ;

// A format specifier follows Rust's mini-language; its pieces are lexed as
// ordinary tokens and muted by the highlight query.
formatSpec
    : formatSpecAtom*
    ;

formatSpecAtom
    : IDENTIFIER | INTEGER | FLOAT
    | '.' | '<' | '>' | '^' | '+' | '-' | '#' | '?' | '_' | '*'
    ;

FLOAT
    : [0-9] [0-9_]* '.' [0-9] [0-9_]* ([eE] [+-]? [0-9]+)?
    | [0-9] [0-9_]* [eE] [+-]? [0-9]+
    ;

INTEGER
    : '0' [xX] [0-9a-fA-F] [0-9a-fA-F_]*
    | '0' [bB] [01] [01_]*
    | '0' [oO] [0-7] [0-7_]*
    | [0-9] [0-9_]*
    ;

STRING_LITERAL
    : 'b'? '"' ('\\' . | ~["\\])* '"'
    ;

// Brace tokens carry the mode commands that inline `'{'` / `'}'` inherit, so a
// template interpolation's nested braces balance via the mode stack.
LBRACE
    : '{' -> pushMode(DEFAULT_MODE)
    ;

RBRACE
    : '}' -> popMode
    ;

BACKTICK
    : '`' -> pushMode(TEMPLATE)
    ;

CHAR_LITERAL
    : 'b'? '\'' (UNICODE_ESCAPE | HEX_ESCAPE | '\\' . | ~['\\\r\n]) '\''
    ;

fragment HEX_ESCAPE
    : '\\' 'x' [0-9a-fA-F] [0-9a-fA-F]
    ;

fragment UNICODE_ESCAPE
    : '\\' 'u' '{' [0-9a-fA-F]+ '}'
    | '\\' 'u' [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F]
    ;

IDENTIFIER
    : [a-zA-Z_] [a-zA-Z0-9_]*
    ;

// Shebang line; the negated `[` keeps `#![...]` inner attributes separate.
SHEBANG
    : '#!' ~[[\r\n] ~[\r\n]* -> channel(HIDDEN)
    ;

// The `__DATA__` marker ends the source code; everything after it is the
// module's raw data section (spec.md: "Data Sections"), reachable at runtime
// via the `#data` literal. It is not Wado code, so it is lexed as a single
// hidden-channel token rather than parsed — hidden (not skipped) so tooling
// such as the highlighter can still see it and render it muted, like a comment.
DATA_SECTION
    : '__DATA__' .*? EOF -> channel(HIDDEN)
    ;

LINE_COMMENT
    : '//' ~[\r\n]* -> channel(HIDDEN)
    ;

BLOCK_COMMENT
    : '/*' .*? '*/' -> channel(HIDDEN)
    ;

WS
    : [ \t\r\n]+ -> skip
    ;

// Template-string body; whitespace is significant, so this mode skips nothing.
// Only `${` opens an interpolation, so bare `{` / `}` are literal text; a `$`
// not followed by `{` is literal too.
mode TEMPLATE;

TEMPLATE_TEXT
    : ('\\' . | ~[`$\\])+
    ;

INTERP_OPEN
    : '${' -> pushMode(DEFAULT_MODE)
    ;

DOLLAR_TEXT
    : '$' -> type(TEMPLATE_TEXT)
    ;

TEMPLATE_END
    : '`' -> type(BACKTICK), popMode
    ;
