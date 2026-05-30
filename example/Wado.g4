/*
 * Wado grammar (ANTLR4 syntax), consumed by Gale (package-gale).
 *
 * Scope: a precise-but-partial grammar for the Wado language, tracking the
 * token vocabulary in `wado-compiler/src/token.rs` and the lexical forms in
 * `wado-compiler/src/lexer.rs`. It currently covers the surface exercised by
 * `example/hello.wado` and `example/fizzbuzz.wado`:
 *   - `use` imports (with optional `as` aliases)
 *   - function / struct / enum / variant declarations
 *   - `let` / `return` / `if` / `for` / `while` / `loop` / `break` /
 *     `continue` / expression statements
 *   - an expression grammar with C-like operator precedence, `::` paths,
 *     call and field postfixes, and `match` expressions
 *   - integer / float / string / template / char literals and the
 *     `true` / `false` / `null` constants
 *
 * It is meant to grow toward full coverage of the e2e fixtures in
 * `wado-compiler/tests/` and the `wasi/*` stdlib. Generics, attributes,
 * traits/impls, effects, and the full pattern grammar are intentionally
 * left for later increments.
 *
 * Original work, written for the Wado language.
 */

grammar Wado;

// ===========================================================================
// Parser rules
// ===========================================================================

sourceFile
    : item* EOF
    ;

item
    : useDecl
    | functionDecl
    | structDecl
    | enumDecl
    | variantDecl
    ;

// --- Imports ---------------------------------------------------------------

useDecl
    : 'use' importGroup 'from' STRING_LITERAL ';'
    ;

importGroup
    : '{' importList? '}'
    | IDENTIFIER
    ;

importList
    : importItem (',' importItem)* ','?
    ;

importItem
    : IDENTIFIER ('as' IDENTIFIER)?
    ;

// --- Declarations ----------------------------------------------------------

functionDecl
    : ('export' | 'pub')? 'fn' IDENTIFIER '(' paramList? ')' returnType? withClause? block
    ;

paramList
    : param (',' param)* ','?
    ;

param
    : 'mut'? IDENTIFIER ':' typeRef
    ;

returnType
    : '->' typeRef
    ;

withClause
    : 'with' IDENTIFIER (',' IDENTIFIER)*
    ;

structDecl
    : 'pub'? 'struct' IDENTIFIER '{' fieldList? '}'
    ;

fieldList
    : fieldDecl (',' fieldDecl)* ','?
    ;

fieldDecl
    : 'pub'? IDENTIFIER ':' typeRef
    ;

enumDecl
    : 'pub'? 'enum' IDENTIFIER '{' enumCaseList? '}'
    ;

enumCaseList
    : IDENTIFIER (',' IDENTIFIER)* ','?
    ;

variantDecl
    : 'pub'? 'variant' IDENTIFIER '{' variantCaseList? '}'
    ;

variantCaseList
    : variantCase (',' variantCase)* ','?
    ;

variantCase
    : IDENTIFIER ('(' typeRef (',' typeRef)* ')')?
    ;

// --- Types -----------------------------------------------------------------

typeRef
    : '&'? path
    ;

path
    : IDENTIFIER ('::' IDENTIFIER)*
    ;

// --- Statements ------------------------------------------------------------

block
    : '{' statement* '}'
    ;

statement
    : letStatement
    | returnStatement
    | ifStatement
    | forStatement
    | whileStatement
    | loopStatement
    | breakStatement
    | continueStatement
    | exprStatement
    ;

letStatement
    : 'let' 'mut'? IDENTIFIER (':' typeRef)? '=' expression ';'
    ;

returnStatement
    : 'return' expression? ';'
    ;

ifStatement
    : 'if' expression block ('else' (ifStatement | block))?
    ;

forStatement
    : 'for' 'let' 'mut'? IDENTIFIER forTail block
    ;

forTail
    : 'of' expression
    | (':' typeRef)? '=' expression ';' expression? ';' assignment?
    ;

whileStatement
    : 'while' expression block
    ;

loopStatement
    : 'loop' block
    ;

breakStatement
    : 'break' ';'
    ;

continueStatement
    : 'continue' ';'
    ;

exprStatement
    : assignment ';'
    ;

assignment
    : expression (assignOp expression)?
    ;

assignOp
    : '=' | '+=' | '-=' | '*=' | '/=' | '%='
    ;

// --- Expressions -----------------------------------------------------------

expression
    : expression ('*' | '/' | '%') expression
    | expression ('+' | '-') expression
    | expression ('<' | '<=' | '>' | '>=') expression
    | expression ('==' | '!=') expression
    | expression ('&&' | '||') expression
    | unary
    ;

unary
    : ('-' | '!') unary
    | postfix
    ;

postfix
    : primary postfixOp*
    ;

postfixOp
    : '(' argumentList? ')'
    | '.' IDENTIFIER
    ;

argumentList
    : expression (',' expression)* ','?
    ;

primary
    : matchExpr
    | path
    | literal
    | '(' expression ')'
    ;

matchExpr
    : 'match' expression '{' matchArm (',' matchArm)* ','? '}'
    ;

matchArm
    : pattern '=>' expression
    ;

pattern
    : path ('(' patternList? ')')?
    | literal
    ;

patternList
    : pattern (',' pattern)* ','?
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

// ===========================================================================
// Lexer rules
// ===========================================================================

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
    : '"' ('\\' . | ~["\\\r\n])* '"'
    ;

TEMPLATE_STRING
    : '`' ('\\' . | ~[`\\])* '`'
    ;

CHAR_LITERAL
    : '\'' ('\\' . | ~['\\\r\n]) '\''
    ;

IDENTIFIER
    : [a-zA-Z_] [a-zA-Z0-9_]*
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
