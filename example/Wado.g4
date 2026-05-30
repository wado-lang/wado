/*
 * Wado grammar (ANTLR4 syntax), consumed by Gale (package-gale).
 *
 * Scope: a precise-but-partial grammar for the Wado language, tracking the
 * token vocabulary in `wado-compiler/src/token.rs` and the lexical forms in
 * `wado-compiler/src/lexer.rs`. It covers the syntax exercised by the CLI
 * examples (`example/hello.wado`, `fizzbuzz.wado`, `romu.wado`, `tree.wado`):
 *
 *   - attributes (`#[...]`) and inner attributes (`#![...]`)
 *   - declarations: function, struct, enum, variant, trait, impl, with
 *     generic parameters and trait bounds; `use` imports with `as` aliases
 *   - statements: let, return, if/else, C-style and for-of `for`, while,
 *     loop, break, continue, bare `match`, expression statements, assignment
 *     / compound assignment
 *   - an expression grammar with C-like operator precedence, `as` casts,
 *     `..<` / `..=` ranges, `::` paths with turbofish, call / method / field
 *     / index postfixes, the `?` try operator, `matches`, closures, tuple /
 *     array / struct literals, and `if` / `match` expressions
 *   - the pattern grammar: bindings, wildcards, literals, tuple / struct /
 *     variant patterns with rest (`..`), and match guards (`&&`)
 *   - integer / float / string / template / char literals and the
 *     `true` / `false` / `null` constants
 *
 * It is meant to grow toward full coverage of the e2e fixtures in
 * `wado-compiler/tests/` and the `wasi/*` stdlib. Effects/handlers, the
 * `world` / `interface` / `resource` / `flags` declarations, let-chains, and
 * template-string interpolation internals are intentionally left for later.
 *
 * Original work, written for the Wado language.
 */

grammar Wado;

// ===========================================================================
// Parser rules
// ===========================================================================

sourceFile
    : innerAttribute* item* EOF
    ;

item
    : attribute* itemKind
    ;

itemKind
    : useDecl
    | functionDecl
    | structDecl
    | enumDecl
    | variantDecl
    | traitDecl
    | implBlock
    | testDecl
    ;

// `test "name" { ... }` / `test { ... }` blocks. `test` is a contextual
// keyword in Wado (a valid identifier elsewhere); none of the bundled
// examples use it as an identifier, so the example grammar models it as
// a literal keyword for simplicity.
testDecl
    : 'test' STRING_LITERAL? block
    ;

// --- Attributes ------------------------------------------------------------

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
    : IDENTIFIER '=' literal
    | IDENTIFIER
    | literal
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
    : ('pub' | 'export' 'async'?)? 'fn' IDENTIFIER genericParams? '(' paramList? ')'
        returnType? withClause? (block | ';')
    ;

paramList
    : param (',' param)* ','?
    ;

param
    : selfParam
    | 'mut'? IDENTIFIER ':' typeRef ('=' expression)?
    ;

selfParam
    : '&' 'mut'? 'self'
    ;

returnType
    : '->' typeRef
    ;

withClause
    : 'with' withItem (',' withItem)*
    ;

withItem
    : IDENTIFIER
    | 'stores' '[' (IDENTIFIER (',' IDENTIFIER)*)? ']'
    ;

structDecl
    : 'pub'? 'struct' IDENTIFIER genericParams? '{' fieldList? '}'
    ;

fieldList
    : fieldDecl (',' fieldDecl)* ','?
    ;

fieldDecl
    : attribute* 'pub'? IDENTIFIER ':' typeRef
    ;

enumDecl
    : 'pub'? 'enum' IDENTIFIER '{' enumCaseList? '}'
    ;

enumCaseList
    : IDENTIFIER (',' IDENTIFIER)* ','?
    ;

variantDecl
    : 'pub'? 'variant' IDENTIFIER genericParams? '{' variantCaseList? '}'
    ;

variantCaseList
    : variantCase (',' variantCase)* ','?
    ;

variantCase
    : IDENTIFIER ('(' typeRef (',' typeRef)* ')')?
    ;

traitDecl
    : 'pub'? 'trait' IDENTIFIER genericParams? '{' traitMember* '}'
    ;

traitMember
    : 'type' IDENTIFIER (':' traitBounds)? ';'
    | functionDecl
    ;

implBlock
    : 'impl' genericParams? typeRef ('for' typeRef)? '{' implMember* '}'
    ;

implMember
    : 'type' IDENTIFIER '=' typeRef ';'
    | 'pub'? 'const' IDENTIFIER ':' typeRef '=' expression ';'
    | 'pub'? functionDecl
    ;

// --- Generics & types ------------------------------------------------------

genericParams
    : '<' genericParam (',' genericParam)* '>'
    ;

genericParam
    : 'effect'? IDENTIFIER (':' traitBounds)? ('=' typeRef)?
    ;

traitBounds
    : typeRef ('+' typeRef)*
    ;

typeRef
    : '&' 'mut'? typeRef
    | '!'
    | '(' ')'
    | '(' typeRef ')'
    | '[' (typeRef (',' typeRef)*)? ']'
    | 'fn' 'mut'? '(' (typeRef (',' typeRef)*)? ')' returnType? withClause?
    | path typeArgs?
    ;

typeArgs
    : '<' typeRef (',' typeRef)* '>'
    ;

path
    : IDENTIFIER ('::' IDENTIFIER)*
    ;

// Member name after `.`: Wado allows any keyword as a field or method name
// (see `consume_field_name` in wado-compiler/src/parser.rs), so `entry.type`
// and the like must lex the keyword and still be accepted here.
memberName
    : IDENTIFIER
    | 'use' | 'from' | 'as' | 'fn' | 'with' | 'let' | 'mut' | 'return'
    | 'if' | 'else' | 'match' | 'for' | 'while' | 'loop' | 'break'
    | 'continue' | 'in' | 'of' | 'pub' | 'effect' | 'interface'
    | 'reactive' | 'unique' | 'struct' | 'enum' | 'variant' | 'flags'
    | 'type' | 'impl' | 'trait' | 'resource' | 'world' | 'async'
    | 'import' | 'export' | 'assert' | 'global' | 'const' | 'matches'
    | 'stores' | 'true' | 'false' | 'null'
    ;

// --- Statements ------------------------------------------------------------

// A block is a sequence of statements, optionally followed by a final
// expression with no trailing `;` — the block's value (`{ 1 }`,
// `if c { a } else { b }`). The trailing expression is struct-literal-
// unrestricted because a `}` (not a `{`) closes the block.
block
    : '{' statement* expression? '}'
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
    | assertStatement
    | matchStatement
    | exprStatement
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

ifStatement
    : 'if' exprNoStruct block ('else' (ifStatement | block))?
    ;

forStatement
    : 'for' 'let' 'mut'? IDENTIFIER forTail block
    ;

forTail
    : 'of' exprNoStruct
    | (':' typeRef)? '=' expression ';' expression? ';' exprNoStruct?
    ;

whileStatement
    : 'while' exprNoStruct block
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

// A bare `match` used as a statement carries no trailing `;`.
matchStatement
    : matchExpr
    ;

exprStatement
    : expression ';'
    ;

// --- Expressions -----------------------------------------------------------
//
// Precedence is encoded by left recursion, lowest to highest. Gale rewrites
// the direct left recursion the same way ANTLR4 does.

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
    | expression ('<<' | '>>') expression
    | expression ('+' | '-') expression
    | expression 'as' typeRef
    | expression ('*' | '/' | '%') expression
    | unary
    ;

unary
    : ('-' | '!' | '&' '&'? 'mut'? | '*') unary
    | postfix
    ;

postfix
    : primary postfixOp*
    ;

postfixOp
    : '(' argumentList? ')'
    | '::' typeArgs '(' argumentList? ')'
    | '.' memberName ('::' typeArgs)? '(' argumentList? ')'
    | '.' memberName
    | '.' INTEGER
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
    | structLiteral
    | path ('::' typeArgs)?
    | tupleOrArrayLiteral
    | closure
    | ifExpr
    | matchExpr
    | '(' expression ')'
    ;

// `exprNoStruct` is the struct-literal-restricted expression used in the
// header position of `if` / `while` / `for`, where a `{` opens the body.
// It mirrors the full `expression` precedence chain but its leaf
// `primaryNoStruct` drops the `structLiteral` alternative, so `while a {`
// parses `a` as a path and leaves `{` to open the loop body instead of
// being misread as the empty struct literal `a {}`. Sub-expressions
// reachable through `( )`, `[ ]`, or call arguments reset to the full
// `expression`, so a struct literal is still allowed when explicitly
// parenthesised (`while (Foo { x: 1 }).flag {`).
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
    | exprNoStruct ('<<' | '>>') exprNoStruct
    | exprNoStruct ('+' | '-') exprNoStruct
    | exprNoStruct 'as' typeRef
    | exprNoStruct ('*' | '/' | '%') exprNoStruct
    | unaryNoStruct
    ;

unaryNoStruct
    : ('-' | '!' | '&' '&'? 'mut'? | '*') unaryNoStruct
    | postfixNoStruct
    ;

postfixNoStruct
    : primaryNoStruct postfixOp*
    ;

primaryNoStruct
    : literal
    | 'self'
    | path ('::' typeArgs)?
    | tupleOrArrayLiteral
    | closure
    | ifExpr
    | matchExpr
    | '(' expression ')'
    ;

structLiteral
    : path '{' fieldInitList? '}'
    ;

fieldInitList
    : fieldInit (',' fieldInit)* ','?
    ;

fieldInit
    : IDENTIFIER ':' expression
    | IDENTIFIER
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
    : 'if' exprNoStruct block 'else' (ifExpr | block)
    ;

matchExpr
    : 'match' exprNoStruct '{' (matchArm (',' matchArm)* ','?)? '}'
    ;

matchArm
    : pattern ('&&' expression)? '=>' (block | expression)
    ;

// --- Patterns --------------------------------------------------------------

pattern
    : '_'
    | 'mut'? IDENTIFIER
    | literal
    | '-' INTEGER
    | path ('(' (pattern (',' pattern)*)? ')')?
    | path? '{' patternFieldList? '}'
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
    : IDENTIFIER ':' pattern
    | IDENTIFIER
    ;

// --- Literals --------------------------------------------------------------

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

// `#!` at the start of a line is a shebang (e.g. `#!/usr/bin/env wado`).
// The negated `[` after `#!` keeps inner attributes (`#![...]`) lexing as
// `#` `!` `[`, so only a real shebang is captured here.
SHEBANG
    : '#!' ~[[\r\n] ~[\r\n]* -> channel(HIDDEN)
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
