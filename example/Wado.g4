// Partial ANTLR4 grammar for Wado, consumed by Gale. Covers the syntax in
// the CLI examples (hello, fizzbuzz, romu, tree). Effects/handlers, world /
// interface / resource / flags, and let-chains are not yet modeled.

grammar Wado;

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
    : IDENTIFIER '=' literal
    | IDENTIFIER
    | literal
    ;

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

// Any keyword may be a field/method name after `.` (e.g. `entry.type`).
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

// Optional trailing expression with no `;` is the block's value (`{ 1 }`).
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

// Shebang line; the negated `[` keeps `#![...]` inner attributes separate.
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
