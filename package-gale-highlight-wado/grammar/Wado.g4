// ANTLR4 grammar for Wado, consumed by Gale.
// Checked against the compiler's parser by `mise run check-grammar`.

grammar Wado;

options { language = Wado; }

@lexer::members {
    raw_hashes: List<i32> = []
}

sourceFile
    : innerAttribute* item* EOF
    ;

// `export(…)` names the world export a function provides, and a re-export
// names its members, so neither form reaches every item.
item
    : attribute* itemModifiers itemKind
    | attribute* 'pub'? 'export' exportTargets 'async'? 'fn' funcSig
    | attribute* wholeModuleUse
    ;

// Only an export runs asynchronously.
itemModifiers
    : 'internal'
    | 'pub'? ('export' 'async'?)?
    ;

exportTargets
    : '(' exportTarget (',' exportTarget)* ','? ')'
    ;

exportTarget
    : identifier '::' identifier ('::' identifier)?
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
    : 'global' 'mut'? name ':' typeRef '=' expression ';'
    ;

typeAliasDecl
    : 'type' bindingName genericParams? ('=' typeRef)? ';'
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
    : '(' (attrArg (',' attrArg)* ','?)? ')'
    ;

attrArg
    : identifier ('=' attrValue)?
    | STRING_LITERAL
    | number
    ;

// An array holds strings or names, never both.
attrValue
    : STRING_LITERAL
    | '-'? number
    | identifier
<<<<<<< HEAD
    | '_'
    | '[' (attrValue (',' attrValue)*)? ']'
||||||| 861f89f6e
    | '[' (attrValue (',' attrValue)*)? ']'
=======
    | '[' ']'
    | '[' STRING_LITERAL (',' STRING_LITERAL)* ','? ']'
    | '[' identifier (',' identifier)* ','? ']'
    ;

number
    : INTEGER
    | FLOAT
    | FLOAT_EXP
>>>>>>> origin/main
    ;

useDecl
    : 'use' '{' importList? '}' useSource
    ;

// A namespace or wildcard import, which no modifier re-exports.
wholeModuleUse
    : 'use' (name | '_') useSource
    ;

useSource
    : 'from' STRING_LITERAL ('with' importAttributes)? ';'
    ;

// A tree of plain values: what the keys mean is the analyzer's and Kiln's.
importAttributes
    : '{' (importAttribute (',' importAttribute)* ','?)? '}'
    ;

importAttribute
    : importKey ':' importValue
    ;

importKey
    : identifier
    ;

importValue
    : STRING_LITERAL
    | '-'? number
    | 'true'
    | 'false'
    | '[' (importValue (',' importValue)* ','?)? ']'
    | importAttributes
    ;

importList
    : importItem (',' importItem)* ','?
    ;

importItem
    : bindingName '::' '{' importList? '}'
    | bindingName ('as' bindingName)?
    | STRING_LITERAL 'as' bindingName
    ;

funcSig
    : bindingName genericParams? '(' paramList? ')' sigTail
    ;

methodSig
    : methodHead sigTail
    ;

// Only a function in an `impl`, a trait, an interface or a resource takes `self`.
methodHead
    : bindingName genericParams? '(' (selfParam (',' paramList?)? | paramList)? ')'
    ;

// Elsewhere a contract is checked in the body, so a bodyless declaration takes none.
sigTail
    : returnType? withClause? (contractClause* block | ';')
    ;

contractClause
    : 'contract' exprNoStruct (',' exprNoStruct)?
    ;

identifier
    : IDENTIFIER
    | 'from' | 'of' | 'type' | 'matches' | 'world'
    | 'interface' | 'resource' | 'import' | 'export'
    | 'forward' | 'trap' | 'effect' | 'flags' | 'variant'
    | 'test' | 'do' | 'task' | 'extends' | 'contract'
    ;

// A declared or type name. The compiler lexes `resume` as a name and refuses it
// only once bound; it is `resume value;` only where a statement begins.
name
    : IDENTIFIER
    | 'resume'
    ;

bindingName
    : identifier
    | 'resume'
    ;

// The name a type declaration introduces.
typeName
    : name
    ;

// Defaults trail: a parameter without one follows none with one.
paramList
    : param (',' param)* (',' defaultedParam)* ','?
    | defaultedParam (',' defaultedParam)* ','?
    ;

param
    : attribute* 'mut'? ('_' | bindingName) ':' typeRef
    ;

defaultedParam
    : param '=' expression
    ;

selfParam
    : '&' 'mut'? 'self'
    | 'self'
    ;

returnType
    : '->' typeRef
    ;

withClause
    : 'with' ('(' (withItem (',' withItem)*)? ')' | withItem)
    ;

withItem
    : name
    | '_'
    ;

structDecl
    : 'affine'? 'struct' typeName genericParams? '{' fieldList? '}'
    ;

fieldList
    : fieldDecl (',' fieldDecl)* ','?
    ;

fieldDecl
    : attribute* ('pub' | 'internal')? fieldName ':' typeRef ('=' expression)?
    ;

enumDecl
    : 'enum' typeName '{' enumCaseList? '}'
    ;

enumCaseList
    : enumCase (',' enumCase)* ','?
    ;

enumCase
    : attribute* name ('=' expression)?
    ;

flagsDecl
    : 'flags' typeName '{' flagsCaseList? '}'
    ;

flagsCaseList
    : flagsCase (',' flagsCase)* ','?
    ;

flagsCase
    : attribute* name
    ;

variantDecl
    : 'affine'? 'variant' typeName genericParams? '{' variantCaseList? '}'
    ;

variantCaseList
    : variantCase (',' variantCase)* ','?
    ;

variantCase
    : attribute* name ('(' typeRef (',' typeRef)* ')')?
    ;

traitDecl
    : 'affine'? 'trait' typeName genericParams? (':' traitBounds)? withClause? '{' traitMember* '}'
    ;

interfaceDecl
    : 'interface' typeName '{' operation* '}'
    ;

worldDecl
    : 'world' name '{' worldItem* '}'
    ;

worldItem
    : 'import' name ';'
    | 'export' ('async'? 'fn' bindingName genericParams? '(' paramList? ')' returnType? | name) ';'
    ;

resourceDecl
    : 'resource' typeName genericParams? ('extends' typeRef)? ('{' operation* '}' | ';')
    ;

// A trait method's contract binds every implementation, so it needs no body.
traitMember
    : attribute* 'affine'? 'type' name genericParams? (':' traitBounds)? ';'
    | attribute* 'fn' methodHead returnType? withClause? contractClause* (block | ';')
    ;

// An interface operation or a resource method: modifiers come from the
// declaration, and a contract is not supported yet.
operation
    : attribute* 'async'? 'fn' methodHead returnType? withClause? (block | ';')
    ;

implBlock
    : 'impl' genericParams? implTarget ('for' implTarget)? ('{' implMember* '}' | ';')
    ;

// A type or one reference to it, never a reference to a reference.
implTarget
    : '&' 'mut'? typeAtom
    | typeAtom
    ;

implMember
    : attribute* implMemberBody
    ;

implMemberBody
    : 'type' name genericParams? '=' typeRef ';'
    | '..' ('trap' | 'forward')
    | 'export' 'async'? 'fn' methodSig
    | ('pub' | 'internal')? implPubMember
    ;

implPubMember
    : 'const' name ':' typeRef '=' expression ';'
    | 'fn' methodSig
    ;

// The attributes sit here rather than in `genericParam`, whose subtree the
// highlighter paints as types outright. An attribute holds plain names.
genericParams
    : '<' attribute* genericParam (',' attribute* genericParam)* '>'
    ;

// `affine` admits an affine type argument, so an effect parameter takes none.
genericParam
    : ('affine'? '..'? | 'effect') name (':' traitBounds)? ('=' typeRef)?
    ;

traitBounds
    : typeRef ('+' typeRef)*
    ;

typeRef
    : ('&' | '&&') 'mut'? typeRef
    | typeAtom
    ;

typeAtom
    : '!'
    | '_'
    | '(' (typeRef (',' typeRef)*)? ')'
    | '[' (typeElement (',' typeElement)*)? ']'
    | 'fn' 'mut'? '(' (typeRef (',' typeRef)*)? ')' returnType? withClause?
    | typePath typeArgs?
    ;

// A type's path. Only `path`, which an expression can begin with, leaves `resume` out.
typePath
    : (name | 'Self') ('::' name)*
    ;

typeElement
    : '..'? typeRef
    ;

typeArgs
    : '<' typeArg (',' typeArg)* '>'
    ;

typeArg
    : (IDENTIFIER | 'Self') '=' typeRef
    | IDENTIFIER ':' traitBounds
    | typeRef
    ;

path
    : (IDENTIFIER | 'Self') ('::' IDENTIFIER)*
    ;

// `memberName` is the token set. Each use site wraps it in a rule naming what
// the name is there, and the query captures those wrappers rather than
// `memberName`, which would fire under every use site at once.

// The name in `.name(...)`.
methodName
    : memberName
    ;

// The name in `.name`, a field declaration's, a struct literal's and a
// pattern's `name:`.
fieldName
    : memberName
    ;

memberName
    : IDENTIFIER
    | 'use' | 'from' | 'as' | 'fn' | 'with' | 'let' | 'mut' | 'return'
    | 'if' | 'else' | 'match' | 'for' | 'while' | 'loop' | 'break'
    | 'continue' | 'in' | 'of' | 'pub' | 'effect' | 'interface'
    | 'struct' | 'enum' | 'variant' | 'flags'
    | 'type' | 'impl' | 'trait' | 'resource' | 'world' | 'async'
    | 'import' | 'export' | 'assert' | 'global' | 'const' | 'matches'
    | 'true' | 'false' | 'null' | 'trap' | 'forward' | 'Self'
    | 'test' | 'do' | 'task' | 'extends' | 'internal' | 'resume' | 'self'
    | 'contract' | 'unchecked' | 'affine'
    ;

// A lone `;` is an empty statement.
block
    : '{' (';' | compoundStatement | simpleStatement ';')* simpleStatement? '}'
    ;

// Where `fragment_entries` resumes a parse; no rule refers to it.
statement
    : compoundStatement
    | simpleStatement ';'?
    ;

// A statement its own closing brace ends.
compoundStatement
    : ifStatement
    | forStatement
    | whileStatement
    | loopStatement
    | matchStatement
    | withStatement
    | labeledBlock
    | localItem
    ;

// A statement a `;` ends, or the `}` of its block.
simpleStatement
    : letStatement
    | returnStatement
    | resumeStatement
    | taskReturnStatement
    | breakStatement
    | continueStatement
    | assertStatement
    | exprStatement
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
    ;

labeledBlock
    : identifier ':' block
    ;

// Only a typed binding may wait for its value.
letStatement
    : attribute* 'let' letPattern (':' typeRef)? '=' expression ('else' block)?
    | attribute* 'let' letPattern ':' typeRef
    ;

// The annotation types the whole pattern, so no alternative carries its own.
letPattern
    : letPatternRange ('|' letPatternRange)*
    ;

letPatternRange
    : patternPrimary (('..<' | '..=') patternPrimary)?
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

resumeStatement
    : 'resume' expression
    ;

ifStatement
    : 'if' condition block ('else' (ifStatement | block))?
    ;

// A condition holding a `let` is a let chain: comparison-level operands that
// only `&&` joins. Without one it is an expression.
condition
    : exprNoStruct
    | (cmpExprNoStruct '&&')* letCondition ('&&' (letCondition | cmpExprNoStruct))*
    ;

letCondition
    : 'let' pattern '=' cmpExprNoStruct
    ;

forStatement
    : 'for' forHead block
    | 'for' '(' ('let' pattern '=' expression | expression)? ';' condition? ';' expression? ')' block
    ;

forHead
    : 'let' pattern forTail
    | exprNoStruct? ';' condition? ';' exprNoStruct?
    ;

forTail
    : 'of' exprNoStruct
    | '=' expression ';' condition? ';' exprNoStruct?
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
    : stmtExpression
    ;

// The levels the compiler's parser climbs, loosest first: assignment
// (right-associative), range (non-associative), `||`, `&&`, comparison, `!`,
// `matches`, `|`, `^`, `&`, shift, `+ -`, `* / %`, `as`, then the prefix
// operators. Each level's operand is the level below it. `notTail` and
// `logicTail` continue the levels after a left edge that a context narrows: a
// statement leaves `if`, `match`, `with` and a labeled block to the statement
// forms, and a closure or match arm body leaves a leading `{` to the block.
expression
    : notExpr logicTail
    ;

stmtExpression
    : ('!' notExpr | stmtUnary notTail) logicTail
    ;

bodyExpression
    : ('!' notExpr | bodyUnary notTail) logicTail
    ;

logicTail
    : cmpChain? ('&&' cmpExpr)* ('||' andExpr)* (rangeOp orExpr)? (assignOp expression)?
    ;

orExpr
    : andExpr ('||' andExpr)*
    ;

andExpr
    : cmpExpr ('&&' cmpExpr)*
    ;

cmpExpr
    : notExpr cmpChain?
    ;

// A chain runs one way: ascending, descending or `==`. A `!=` stands alone.
cmpChain
    : (('<' | '<=') notExpr)+
    | (('>' | '>=') notExpr)+
    | ('==' notExpr)+
    | '!=' notExpr
    ;

notExpr
    : '!' notExpr
    | unary notTail
    ;

notTail
    : ('as' typeRef)* (mulOp castExpr)* (addOp mulExpr)* (shiftOp addExpr)* ('&' shiftExpr)* ('^' bitAndExpr)* ('|' bitXorExpr)* matchesTest*
    ;

matchesTest
    : 'matches' '{' pattern ('&&' expression)? '}'
    ;

bitXorExpr
    : bitAndExpr ('^' bitAndExpr)*
    ;

bitAndExpr
    : shiftExpr ('&' shiftExpr)*
    ;

shiftExpr
    : addExpr (shiftOp addExpr)*
    ;

addExpr
    : mulExpr (addOp mulExpr)*
    ;

mulExpr
    : castExpr (mulOp castExpr)*
    ;

castExpr
    : unary ('as' typeRef)*
    ;

assignOp
    : '=' | '+=' | '-=' | '*=' | '/=' | '%=' | '&=' | '|=' | '^=' | '<<=' | '>>='
    ;

rangeOp
    : '..<' | '..='
    ;

shiftOp
    : '<<' | '>' '>'
    ;

addOp
    : '+' | '-'
    ;

mulOp
    : '*' | '/' | '%'
    ;

prefixOp
    : '-' | '~' | ('&' | '&&') 'mut'? | '*' | 'unchecked'
    ;

unary
    : prefixOp unary
    | primary postfixOp*
    ;

stmtUnary
    : prefixOp unary
    | stmtPrimary postfixOp*
    ;

bodyUnary
    : prefixOp unary
    | bodyPrimary postfixOp*
    ;

postfixOp
    : '(' argumentList? ')'
    | '::' typeArgs '(' argumentList? ')'
    // A turbofish after a `.` belongs to a call: the parser demands the `(`
    // after it, so a field read carries no type arguments.
    | '.' (methodName ('::' typeArgs)? '(' argumentList? ')' | fieldName | INTEGER | FLOAT)
    | '[' expression ']'
    | '?'
    ;

argumentList
    : expression (',' expression)* ','?
    ;

// A brace in value position is a struct literal, never a block.
primary
    : atom
    | structLiteral
    | braceLiteral
    | ifExpr
    | matchExpr
    | withExpr
    | labeledBlock
    ;

stmtPrimary
    : atom
    | structLiteral
    | braceLiteral
    ;

atom
    : literal
    | 'self'
    | compileTimeExpr
    | taggedTemplate
    | exprPath
    | tupleOrArrayLiteral
    | closure
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

// One turbofish: a second belongs to a static method call, which `postfixOp`
// spells with its `(`.
exprPath
    : (identifier | 'Self') ('::' pathSegment)* ('::' typeArgs ('::' pathSegment)*)?
    ;

// A template literal directly after a path tags it (WEP 2026-01-10). A gap
// leaves two expressions side by side, which nothing accepts.
taggedTemplate
    : (tagOwner '::')* tagName taggedTemplateString
    ;

taggedTemplateString
    : TAG_BACKTICK templatePart* BACKTICK
    | TAG_RAW_OPEN templatePart* RAW_CLOSE
    ;

// The type or module a qualified tag hangs off, uncoloured like a `::`
// segment's head; the tag itself is the function the template calls.
tagOwner
    : identifier
    ;

tagName
    : identifier
    ;

// A `::` segment: a variant case, a static method, or the middle of a module
// path. It has its own rule so no capture on `fieldName` or `methodName`
// reaches it; nothing captures this one, matching `patternPath` and `path`.
pathSegment
    : memberName
    ;

compileTimeExpr
    : '#' IDENTIFIER ('(' argumentList? ')')?
    ;

// `expression`'s levels where a brace opens the block that follows, as in a
// condition: no operand outside brackets is a struct literal.
exprNoStruct
    : notExprNoStruct cmpChainNoStruct? ('&&' cmpExprNoStruct)* ('||' andExprNoStruct)* (rangeOp orExprNoStruct)? (assignOp exprNoStruct)?
    ;

orExprNoStruct
    : andExprNoStruct ('||' andExprNoStruct)*
    ;

andExprNoStruct
    : cmpExprNoStruct ('&&' cmpExprNoStruct)*
    ;

cmpExprNoStruct
    : notExprNoStruct cmpChainNoStruct?
    ;

cmpChainNoStruct
    : (('<' | '<=') notExprNoStruct)+
    | (('>' | '>=') notExprNoStruct)+
    | ('==' notExprNoStruct)+
    | '!=' notExprNoStruct
    ;

notExprNoStruct
    : '!' notExprNoStruct
    | bitXorExprNoStruct ('|' bitXorExprNoStruct)* matchesTest*
    ;

bitXorExprNoStruct
    : bitAndExprNoStruct ('^' bitAndExprNoStruct)*
    ;

bitAndExprNoStruct
    : shiftExprNoStruct ('&' shiftExprNoStruct)*
    ;

shiftExprNoStruct
    : addExprNoStruct (shiftOp addExprNoStruct)*
    ;

addExprNoStruct
    : mulExprNoStruct (addOp mulExprNoStruct)*
    ;

mulExprNoStruct
    : castExprNoStruct (mulOp castExprNoStruct)*
    ;

castExprNoStruct
    : unaryNoStruct ('as' typeRef)*
    ;

unaryNoStruct
    : prefixOp unaryNoStruct
    | primaryNoStruct postfixOp*
    ;

// A labeled block belongs here although it ends in a brace: its `:` says the
// brace opens the block and not the `if` / `while` / `for` body that follows.
primaryNoStruct
    : atom
    | ifExpr
    | matchExpr
    | labeledBlock
    ;

structLiteral
    : path ('::' typeArgs)? '{' fieldInitList? '}'
    ;

fieldInitList
    : fieldInit (',' fieldInit)* ','?
    ;

// A shorthand reads the variable of the field's name, so a keyword no variable
// takes needs the colon.
fieldInit
    : (fieldName | STRING_LITERAL) ':' expression
    | shorthandField
    | STRING_LITERAL
    | '..' expression
    ;

// A field's name and a variable's at once, coloured as the field.
shorthandField
    : bindingName
    ;

tupleOrArrayLiteral
    : '[' 'for' 'let' pattern 'of' exprNoStruct '{' expression '}' ']'
    | '[' (arrayElement (',' arrayElement)* ','?)? ']'
    ;

arrayElement
    : '..' expression
    | expression
    ;

// A closure declares no `with` row yet, and a `with` after its head would read
// as one, so a handler reaches the body through a block or parentheses.
closure
    : ('||' | '|' closureParamList? '|') returnType? (block | closureBody)
    ;

closureBody
    : ('!' notExpr | closureUnary notTail) logicTail
    ;

closureUnary
    : prefixOp unary
    | closurePrimary postfixOp*
    ;

closurePrimary
    : atom
    | structLiteral
    | ifExpr
    | matchExpr
    | labeledBlock
    ;

bodyPrimary
    : closurePrimary
    | withExpr
    ;

closureParamList
    : closureParam (',' closureParam)* ','?
    ;

closureParam
    : attribute* 'mut'? ('_' | bindingName) closureParamType?
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
    : pattern ('&&' expression)? '=>' (block | 'return' expression? | resumeStatement | bodyExpression)
    ;

pattern
    : patternRange ('|' patternRange)*
    ;

patternRange
    : patternPrimary (('..<' | '..=') patternPrimary)? (':' typeRef)?
    ;

patternPrimary
    : '_'
    | 'mut'? bindingName
    | literal
    | '-' number
    | 'mut'? patternPath ('(' (pattern (',' pattern)*)? ')')?
    | 'mut'? path? '{' patternFieldList? '}'
    | 'mut'? '(' (pattern (',' pattern)*)? ')'
    | 'mut'? '[' patternElements? ']'
    ;

patternPath
    : (identifier | 'Self') typeArgs? ('::' (typeArgs | identifier))*
    ;

patternElements
    : '..'
    | pattern (',' pattern)* (',' '..')? ','?
    ;

patternFieldList
    : '..'
    | patternField (',' patternField)* (',' '..')? ','?
    ;

patternField
    : (fieldName | STRING_LITERAL) ':' pattern
    | shorthandField
    | STRING_LITERAL
    ;

literal
    : number
    | STRING_LITERAL
    | templateString
    | CHAR_LITERAL
    | 'true'
    | 'false'
    | 'null'
    ;

// An untagged literal may still touch a keyword, as after `return`.
templateString
    : (BACKTICK | TAG_BACKTICK) templatePart* BACKTICK
    | (RAW_OPEN | TAG_RAW_OPEN) templatePart* RAW_CLOSE
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

// An `_` stands between two digits. A float with an exponent is its own token,
// as `t.0.1` reads a FLOAT as two tuple fields and `t.1.5e3` cannot.
FLOAT
    : DIGITS '.' DIGITS
    ;

FLOAT_EXP
    : DIGITS ('.' DIGITS)? [eE] [+-]? DIGITS
    ;

INTEGER
    : '0' [xX] [0-9a-fA-F] ('_'? [0-9a-fA-F])*
    | '0' [bB] [01] ('_'? [01])*
    | '0' [oO] [0-7] ('_'? [0-7])*
    | DIGITS
    ;

fragment DIGITS
    : [0-9] ('_'? [0-9])*
    ;

// In literal text a CR is the line break of a CRLF and nothing else.
STRING_LITERAL
    : 'b'? '"' ('\\' ~[\r] | '\\'? '\r\n' | ~["\\\r])* '"'
    ;

// A raw string is one STRING_LITERAL, as a cooked one is: `more` carries the
// opener and the text into the closer, which types the whole run. Its `#`s
// share the raw template's stack.
RAW_STRING_OPEN
    : '#'+ '"' { lx.raw_hashes.push(pos - start - 1) } -> more, pushMode(RAW_STRING)
    ;

RAW_BYTES_OPEN
    : 'b' '#'+ '"' { lx.raw_hashes.push(pos - start - 2) } -> more, pushMode(RAW_STRING)
    ;

LBRACE
    : '{' -> pushMode(DEFAULT_MODE)
    ;

RBRACE
    : '}' -> popMode
    ;

// A template touching the name before it is tagged. Whitespace never reaches
// the parser, so the lexer tells the touching opener apart. Ahead of
// `BACKTICK` and `RAW_OPEN`, which it otherwise ties.
TAG_BACKTICK
    : '`' { start > 0 && (lx.src.byte_at(start - 1).is_ascii_alphanumeric() || lx.src.byte_at(start - 1) == b'_') }?
      -> pushMode(TEMPLATE)
    ;

TAG_RAW_OPEN
    : '#'+ '`' { start > 0 && (lx.src.byte_at(start - 1).is_ascii_alphanumeric() || lx.src.byte_at(start - 1) == b'_') }?
      { lx.raw_hashes.push(pos - start - 1) } -> pushMode(RAW_TEMPLATE)
    ;

BACKTICK
    : '`' -> pushMode(TEMPLATE)
    ;

// A raw template's closer repeats its opener's `#`s. The count is a stack,
// since a hole may hold a raw template of its own.
RAW_OPEN
    : '#'+ '`' { lx.raw_hashes.push(pos - start - 1) } -> pushMode(RAW_TEMPLATE)
    ;

CHAR_LITERAL
    : 'b'? '\'' (UNICODE_ESCAPE | HEX_ESCAPE | '\\' ~[\r] | ~['\\\r\n])+ '\''
    ;

fragment HEX_ESCAPE
    : '\\' 'x' [0-9a-fA-F] [0-9a-fA-F]
    ;

fragment UNICODE_ESCAPE
    : '\\' 'u' '{' [0-9a-fA-F]+ '}'
    | '\\' 'u' [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F]
    ;

// `_` alone lexes as the wildcard token '_', defined ahead of this rule.
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
    : [ \t\r\n\f]+ -> skip
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
    : ('\\' ~[\r] | '\\'? '\r\n' | ~[`$\\\r])+
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

// A backslash is text. The longest match takes `${` over `$`, and the closer
// over a lone backtick. The closer takes no more `#`s than the opener had, so
// one `#` past them is text.
mode RAW_TEMPLATE;

RAW_CLOSE
    : '`' ({ pos - start <= lx.raw_hashes.last().unwrap() }? '#')+
      { pos - start == lx.raw_hashes.last().unwrap() + 1 }?
      { lx.raw_hashes.pop() } -> popMode
    ;

RAW_INTERP_OPEN
    : '${' -> type(INTERP_OPEN), pushMode(DEFAULT_MODE)
    ;

RAW_TEXT
    : (~[`$\r] | '\r\n')+ -> type(TEMPLATE_TEXT)
    ;

RAW_LONE
    : [`$] -> type(TEMPLATE_TEXT)
    ;

mode RAW_STRING;

RAW_STRING_CLOSE
    : '"' ({ pos - start <= lx.raw_hashes.last().unwrap() }? '#')+
      { pos - start == lx.raw_hashes.last().unwrap() + 1 }?
      { lx.raw_hashes.pop() } -> type(STRING_LITERAL), popMode
    ;

RAW_STRING_TEXT
    : (~["\r] | '\r\n')+ -> more
    ;

RAW_STRING_QUOTE
    : '"' -> more
    ;
