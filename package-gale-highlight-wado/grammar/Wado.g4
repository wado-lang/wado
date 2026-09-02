// ANTLR4 grammar for Wado, consumed by Gale.
// Checked against the compiler's parser by `mise run check-grammar`.

grammar Wado;

sourceFile
    : innerAttribute* item* EOF
    ;

item
    : attribute* itemModifiers itemKind
    ;

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

typeAliasDecl
    : 'type' identifier genericParams? ('=' typeRef)? ';'
    | 'type' typeRef ';'
    ;

testDecl
    : 'test' STRING_LITERAL? block
    ;

attribute
    : '#' '[' identifier attrArgs? ']'
    ;

innerAttribute
    : '#' '!' '[' identifier attrArgs? ']'
    ;

attrArgs
    : '(' (attrArg (',' attrArg)*)? ')'
    ;

attrArg
    : identifier ('=' attrValue)?
    | attrValue
    ;

attrValue
    : literal
    | identifier
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

functionDecl
    : itemModifiers 'fn' funcSig
    ;

funcSig
    : identifier genericParams? '(' paramList? ')' returnType? withClause? (block | ';')
    ;

identifier
    : IDENTIFIER
    | 'from' | 'of' | 'type' | 'matches' | 'stores' | 'world'
    | 'interface' | 'resource' | 'import' | 'export' | 'reactive'
    | 'unique' | 'forward' | 'trap' | 'effect' | 'flags' | 'variant'
    | 'test' | 'do' | 'task' | 'extends'
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
    : 'with' ('(' (withItem (',' withItem)*)? ')' | withItem)
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

interfaceDecl
    : 'interface' IDENTIFIER genericParams? '{' traitMember* '}'
    ;

worldDecl
    : 'world' IDENTIFIER '{' worldItem* '}'
    ;

worldItem
    : 'import' IDENTIFIER ';'
    | 'export' ('async'? 'fn' identifier genericParams? '(' paramList? ')' returnType? | IDENTIFIER) ';'
    ;

resourceDecl
    : 'resource' IDENTIFIER genericParams? ('extends' typeRef)? ('{' resourceMember* '}' | ';')
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
    | 'fn' 'mut'? '(' (typeRef (',' typeRef)*)? ')' returnType? withClause?
    | path typeArgs?
    ;

typeElement
    : '..'? typeRef
    ;

typeArgs
    : '<' typeArg (',' typeArg)* '>'
    ;

typeArg
    : IDENTIFIER '=' typeRef
    | IDENTIFIER ':' traitBounds
    | typeRef
    ;

path
    : IDENTIFIER ('::' IDENTIFIER)*
    ;

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

block
    : '{' (statement | ';')* '}'
    ;

statement
    : ifStatement
    | forStatement
    | whileStatement
    | loopStatement
    | matchStatement
    | withStatement
    | labeledBlock
    | localItem
    | letStatement ';'?
    | returnStatement ';'?
    | taskReturnStatement ';'?
    | breakStatement ';'?
    | continueStatement ';'?
    | assertStatement ';'?
    | exprStatement ';'?
    ;

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

labeledBlock
    : identifier ':' block
    ;

letStatement
    : 'reactive'? 'let' pattern (':' typeRef)? ('=' expression ('else' block)?)?
    ;

assertStatement
    : 'assert' expression (',' expression)?
    ;

returnStatement
    : 'return' expression?
    ;

taskReturnStatement
    : 'task' 'return' expression?
    ;

resumeExpr
    : 'resume' expression
    ;

ifStatement
    : 'if' condition block ('else' (ifStatement | block))?
    ;

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
    : 'break' (identifier (':' expression)? | '(' ')')?
    ;

continueStatement
    : 'continue'
    ;

matchStatement
    : matchExpr
    ;

withStatement
    : withExpr
    ;

exprStatement
    : expression
    ;

expression
    : expression ('=' | '+=' | '-=' | '*=' | '/=' | '%=' | '&=' | '|=' | '^=' | '<<=' | '>>=') expression
    | expression ('..<' | '..=') expression
    | expression '||' expression
    | expression '&&' expression
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

withExpr
    : 'with' withBinding (',' withBinding)* 'do' block
    ;

withBinding
    : typeRef '=>' expression
    | expression
    ;

braceLiteral
    : '{' fieldInitList? '}'
    ;

exprPath
    : identifier ('::' (typeArgs | memberName))*
    ;

compileTimeExpr
    : '#' IDENTIFIER ('(' argumentList? ')')?
    ;

exprNoStruct
    : exprNoStruct ('=' | '+=' | '-=' | '*=' | '/=' | '%=' | '&=' | '|=' | '^=' | '<<=' | '>>=') exprNoStruct
    | exprNoStruct ('..<' | '..=') exprNoStruct
    | exprNoStruct '||' exprNoStruct
    | exprNoStruct '&&' exprNoStruct
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

closureParamType
    : ':' typeRef ('=' closureDefault)?
    ;

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

ifExpr
    : 'if' condition block ('else' (ifExpr | block))?
    ;

matchExpr
    : 'match' exprNoStruct '{' (matchArm ','?)* '}'
    ;

matchArm
    : pattern ('&&' expression)? '=>' (block | ifStatement | 'return' expression? | expression)
    ;

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

templateString
    : BACKTICK templatePart* BACKTICK
    ;

templatePart
    : TEMPLATE_TEXT
    | interpolation
    ;

// The `:` and the closing `}` get their own rules so the highlight query can
// name them: an override matches anywhere under a rule, so `(interpolation ":"
// …)` would also catch the `:` in `${ xs.all(|p: P| …) }`.
interpolation
    : INTERP_OPEN expression formatSpec? interpolationEnd
    ;

interpolationEnd
    : '}'
    ;

formatSpec
    : ':' formatSpecAtom*
    ;

// The fill is any character the interpolation scanner does not read as
// structure, so this lists the punctuation the lexer already tokenizes,
// minus the quotes, braces and `/`. NON_ASCII covers a multi-byte fill.
formatSpecAtom
    : IDENTIFIER | INTEGER | FLOAT | NON_ASCII
    | '.' | '<' | '>' | '^' | '+' | '-' | '#' | '?' | '_' | '*'
    | '!' | '%' | '&' | '(' | ')' | ',' | ';' | '=' | '[' | ']' | '|' | '~' | '$'
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

SHEBANG
    : '#!' ~[[\r\n] ~[\r\n]* -> channel(HIDDEN)
    ;

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

// A format specifier's fill character when it is not ASCII: `${x:あ>8}`.
// Every other token is ASCII-only and a bare non-ASCII character is legal
// nowhere else, so this steals nothing: a string, char literal or comment
// containing one matches its own rule from an ASCII delimiter.
NON_ASCII
    : ~[\u0000-\u007F]
    ;

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
