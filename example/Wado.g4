/*
 * Wado grammar (ANTLR4 syntax), consumed by Gale (package-gale).
 *
 * This is a deliberately small first cut: it covers exactly the surface
 * exercised by `example/hello.wado` — `use` imports, an `export fn` with a
 * `with` effect clause, a block, and call expressions over identifiers and
 * string literals. It is meant to grow toward full coverage of the e2e
 * fixtures in `wado-compiler/tests/` and the `wasi/*` stdlib.
 *
 * Original work, written for the Wado language.
 */

grammar Wado;

// ---------------------------------------------------------------------------
// Parser rules
// ---------------------------------------------------------------------------

sourceFile
    : item* EOF
    ;

item
    : useDecl
    | functionDecl
    ;

useDecl
    : 'use' importGroup 'from' STRING_LITERAL ';'
    ;

importGroup
    : '{' importList? '}'
    ;

importList
    : importItem (',' importItem)*
    ;

importItem
    : IDENTIFIER
    ;

functionDecl
    : 'export'? 'fn' IDENTIFIER '(' paramList? ')' withClause? block
    ;

paramList
    : param (',' param)*
    ;

param
    : IDENTIFIER ':' typeRef
    ;

typeRef
    : IDENTIFIER
    ;

withClause
    : 'with' IDENTIFIER (',' IDENTIFIER)*
    ;

block
    : '{' statement* '}'
    ;

statement
    : expression ';'
    ;

expression
    : callExpression
    | primary
    ;

callExpression
    : IDENTIFIER '(' argumentList? ')'
    ;

argumentList
    : expression (',' expression)*
    ;

primary
    : IDENTIFIER
    | STRING_LITERAL
    ;

// ---------------------------------------------------------------------------
// Lexer rules
// ---------------------------------------------------------------------------

IDENTIFIER
    : [a-zA-Z_] [a-zA-Z0-9_]*
    ;

STRING_LITERAL
    : '"' (~["\\\r\n] | '\\' .)* '"'
    ;

LINE_COMMENT
    : '//' ~[\r\n]* -> channel(HIDDEN)
    ;

WS
    : [ \t\r\n]+ -> skip
    ;
