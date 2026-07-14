// Partial ANTLR4 grammar for Wado, consumed by Gale. Covers the syntax used
// by every example under `example/*.wado` (declarations, statements,
// expressions, patterns, generics, attributes, traits/impls, globals, type
// aliases, `if let` / `while let` let-chains, `task return`, map literals,
// turbofish associated calls, and effect-handler installation via
// `with E => h do { ... }` / `resume`). Effect *declarations* (`effect E {
// ... }`) and world / resource declarations are not yet modeled.

grammar Wado;

sourceFile
    : innerAttribute* item* EOF
    ;

item
    : attribute* itemKind
    ;

itemKind
    : implBlock
    | testDecl
    | functionDecl
    | ('pub' | 'internal')? pubItem
    ;

// Item kinds that take an optional `pub` / `internal` (plus plain `fn`). The
// leading modifier is left-factored into `itemKind` so the dispatch is
// token-led. `use` lives here so `pub use` re-exports parse.
pubItem
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
    ;

globalDecl
    : 'global' 'mut'? IDENTIFIER ':' typeRef '=' expression ';'
    ;

typeAliasDecl
    : 'type' IDENTIFIER genericParams? '=' typeRef ';'
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
    : 'use' importGroup 'from' STRING_LITERAL ';'
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
    : IDENTIFIER ('as' IDENTIFIER)?
    ;

functionDecl
    : ('pub' | 'internal' | 'export')? 'async'? 'fn' funcSig
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
    | 'stores' '[' (storesItem (',' storesItem)*)? ']'
    ;

storesItem
    : IDENTIFIER
    | 'self'
    ;

structDecl
    : 'struct' IDENTIFIER genericParams? '{' fieldList? '}'
    ;

fieldList
    : fieldDecl (',' fieldDecl)* ','?
    ;

fieldDecl
    : attribute* 'pub'? identifier ':' typeRef ('=' expression)?
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
    : 'trait' IDENTIFIER genericParams? '{' traitMember* '}'
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
    : ('import' | 'export') IDENTIFIER ';'
    ;

// A WIT-style resource: an opaque handle with method / static-function
// signatures (or a bodyless unit resource `resource X;`).
resourceDecl
    : 'resource' IDENTIFIER ('{' resourceMember* '}' | ';')
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
    | '[' ('..' typeRef | (typeRef (',' typeRef)*)?) ']'
    | 'fn' 'mut'? '(' (typeRef (',' typeRef)*)? ')' returnType? fnTypeWithClause?
    | path typeArgs?
    ;

// A fn-*type*'s effect clause takes a single effect: a comma there would be
// ambiguous with an enclosing list (e.g. the next parameter), and real fn
// types never carry a comma-separated effect row (those appear only on
// function *declarations*, where a trailing block/`;` disambiguates).
fnTypeWithClause
    : 'with' withItem
    ;

typeArgs
    : '<' typeArg (',' typeArg)* '>'
    ;

// A type argument, optionally an associated-type binding (`Iterator<Item = T>`).
typeArg
    : IDENTIFIER '=' typeRef
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
    ;

// Optional trailing expression with no `;` is the block's value (`{ 1 }`).
block
    : '{' statement* expression? '}'
    ;

statement
    : letStatement
    | returnStatement
    | taskReturnStatement
    | resumeStatement
    | ifStatement
    | forStatement
    | whileStatement
    | loopStatement
    | breakStatement
    | continueStatement
    | assertStatement
    | matchStatement
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
    : 'let' pattern (':' typeRef)? '=' expression ';'
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
resumeStatement
    : 'resume' expression? ';'
    ;

ifStatement
    : 'if' condition block ('else' (ifStatement | block))?
    ;

// `if`/`while` headers admit an optional `let` binding (and let-chains via
// `&&`, folded into the trailing expression). `exprNoStruct` keeps a bare `{`
// reserved for the body.
condition
    : 'let' pattern '=' exprNoStruct
    | exprNoStruct
    ;

forStatement
    : 'for' 'let' pattern forTail block
    ;

forTail
    : 'of' exprNoStruct
    | (':' typeRef)? '=' expression ';' expression? ';' exprNoStruct?
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

exprStatement
    : expression ';'
    ;

// Precedence by left recursion, lowest to highest.
expression
    : expression ('=' | '+=' | '-=' | '*=' | '/=' | '%=' | '&=' | '|=' | '^=' | '<<=' | '>>=') expression
    | expression ('..<' | '..=') expression
    | expression '||' expression
    | expression '&&' expression
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
    : ('-' | '!' | '~' | '&' '&'? 'mut'? | '*') unary
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
    | 'matches' '{' pattern ('&&' expression)? '}'
    | '?'
    ;

argumentList
    : expression (',' expression)* ','?
    ;

primary
    : literal
    | 'self'
    | compileTimeExpr
    | structLiteral
    | mapLiteral
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

// Key-value (map) literal: `{}` or `{ key: value, ... }`. Inferred to
// `TreeMap<String, V>` by context. Excluded from `primaryNoStruct` because a
// `{` after an `if`/`while`/`for` header opens the body.
mapLiteral
    : '{' (mapEntry (',' mapEntry)* ','?)? '}'
    ;

mapEntry
    : (IDENTIFIER | STRING_LITERAL) ':' expression
    ;

// Expression-position path, supporting interspersed turbofish segments:
// `Stream::<u8>::new`, `Future::<Result<(), E>>::new`, `JsonValue::Bool`.
// A `::` segment may be any `memberName` (not just `IDENTIFIER`) so that a
// keyword method name resolves there, e.g. `Instant::from(x)` — mirroring how
// `.from` is already accepted after `.`.
exprPath
    : IDENTIFIER ('::' (typeArgs | memberName))*
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
    : ('-' | '!' | '~' | '&' '&'? 'mut'? | '*') unaryNoStruct
    | postfixNoStruct
    ;

postfixNoStruct
    : primaryNoStruct postfixOp*
    ;

primaryNoStruct
    : literal
    | 'self'
    | compileTimeExpr
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

fieldInit
    : IDENTIFIER (':' expression)?
    | '..' expression
    ;

tupleOrArrayLiteral
    : '[' (arrayElement (',' arrayElement)* ','?)? ']'
    ;

arrayElement
    : '..' expression
    | expression
    ;

closure
    : ('||' | '|' closureParamList? '|') (block | expression)
    ;

closureParamList
    : closureParam (',' closureParam)* ','?
    ;

closureParam
    : 'mut'? IDENTIFIER (':' typeRef)?
    ;

ifExpr
    : 'if' condition block 'else' (ifExpr | block)
    ;

matchExpr
    : 'match' exprNoStruct '{' (matchArm (',' matchArm)* ','?)? '}'
    ;

matchArm
    : pattern ('&&' expression)? '=>' (block | expression)
    ;

// Or-patterns: `A | B | C`, as used in `match` arms and `matches { ... }`.
pattern
    : patternPrimary ('|' patternPrimary)*
    ;

patternPrimary
    : '_'
    | 'mut'? IDENTIFIER
    | literal
    | '-' INTEGER
    | path ('(' (pattern (',' pattern)*)? ')')?
    | 'mut'? path? '{' patternFieldList? '}'
    | '(' (pattern (',' pattern)*)? ')'
    | '[' patternElements? ']'
    ;

patternElements
    : '..'
    | pattern (',' pattern)* (',' '..')? ','?
    ;

patternFieldList
    : patternField (',' patternField)* (',' '..')? ','?
    ;

patternField
    : IDENTIFIER (':' pattern)?
    ;

literal
    : INTEGER
    | FLOAT
    | STRING_LITERAL
    | TEMPLATE_STRING
    | CHAR_LITERAL
    | 'true'
    | 'false'
    | 'null'
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

// A `{ ... }` interpolation holds Wado code, which may nest braces, strings,
// and further templates. Matching it recursively lets a template contain a
// nested template (e.g. a `match` arm inside `{ ... }`). The whole template,
// interpolations included, is one token (highlighted as one string span).
TEMPLATE_STRING
    : '`' ('\\' . | TEMPLATE_INTERP | ~[`\\{])* '`'
    ;

fragment TEMPLATE_INTERP
    : '{' ('\\' . | STRING_LITERAL | CHAR_LITERAL | TEMPLATE_STRING | TEMPLATE_INTERP | ~[{}"'`\\])* '}'
    ;

CHAR_LITERAL
    : '\'' ('\\' . | ~['\\\r\n]) '\''
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
