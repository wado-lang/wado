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
    : 'use' importGroup 'from' StringLiteral ';'
    ;

importGroup
    : '{' importList? '}'
    ;

importList
    : importItem (',' importItem)*
    ;

importItem
    : Identifier
    ;

functionDecl
    : 'export'? 'fn' Identifier '(' paramList? ')' withClause? block
    ;

paramList
    : param (',' param)*
    ;

param
    : Identifier ':' typeRef
    ;

typeRef
    : Identifier
    ;

withClause
    : 'with' Identifier (',' Identifier)*
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
    : Identifier '(' argumentList? ')'
    ;

argumentList
    : expression (',' expression)*
    ;

primary
    : Identifier
    | StringLiteral
    ;

// ---------------------------------------------------------------------------
// Lexer rules
// ---------------------------------------------------------------------------

Identifier
    : [a-zA-Z_] [a-zA-Z0-9_]*
    ;

StringLiteral
    : '"' (~["\\\r\n] | '\\' .)* '"'
    ;

LineComment
    : '//' ~[\r\n]* -> channel(HIDDEN)
    ;

WS
    : [ \t\r\n]+ -> skip
    ;
