// The ONNX textual representation, the syntax `onnx.parser` reads and
// `onnx.printer` writes. Productions follow ONNX's own `docs/Syntax.md`; the
// lexical rules follow `onnx/defs/parser.h`, which is where the `#` comment
// and the identifier's shape come from.
//
// The element type is an `ID` rather than a keyword set. A keyword `float`
// would shadow a tensor named `float`, and an unknown type is a better
// diagnostic from the generator, which can say what it does accept, than a
// parse error here. `seq(...)` and `map(...)` are the same: they are an `ID`
// followed by `(`, which nothing else in a type position can be.
//
// Local functions are left out, and with them the `@name` attribute reference
// that only a function body can carry. A model holding one does not parse.
grammar Onnx;

model : metadata? graph EOF ;

// Model-level data: `<ir_version: 7, opset_import: ["" : 10]>`.
metadata  : LT metaEntry (COMMA metaEntry)* GT ;
metaEntry : ID COLON metaValue ;
metaValue : atom | atomList ;
atomList  : LBRACK (atomPair (COMMA atomPair)*)? RBRACK ;
atomPair  : atom (COLON atom)? ;
atom      : INT | FLOAT | STRING | ID ;

graph        : quotableId inputs ARROW outputs initializers? nodes ;
inputs       : LPAREN (valueInfo (COMMA valueInfo)*)? RPAREN ;
outputs      : LPAREN (valueInfo (COMMA valueInfo)*)? RPAREN ;
initializers : LT (valueInfo (COMMA valueInfo)*)? GT ;

// An initializer's data is inline in braces, a key-value list naming a file
// (ONNX's `external-data`), or absent, meaning the checkpoint supplies it.
valueInfo    : type quotableId (EQ constantData)? ;
constantData : tensorValues | externalData ;
externalData : LBRACK strPair (COMMA strPair)* RBRACK ;
strPair      : STRING COLON STRING ;

nodes : LBRACE node* RBRACE ;

// ONNX writes a node's attributes on one side of the input list, never both.
// The alternatives start on different tokens, so the choice stays token-led.
node        : nodeLabel? outputNames? EQ qualifiedId
              ( attrs LPAREN inputNames? RPAREN
              | LPAREN inputNames? RPAREN attrs? ) ;
nodeLabel   : LBRACK quotableId RBRACK ;
outputNames : quotableId (COMMA quotableId)* ;
inputNames  : quotableId (COMMA quotableId)* ;

attrs : LT attr (COMMA attr)* GT ;
attr  : ID (COLON ID)? EQ attrValue ;

attrValue       : singleAttrValue | attrValueList ;
attrValueList   : LBRACK (singleAttrValue (COMMA singleAttrValue)*)? RBRACK ;
singleAttrValue : INT | FLOAT | STRING | tensorConstant | graph ;

tensorConstant : type quotableId? EQ? tensorValues ;
tensorValues   : LBRACE (tensorElem (COMMA tensorElem)*)? RBRACE ;
tensorElem     : INT | FLOAT | STRING ;

// `ID (` is `seq(T)` or `map(K, V)`; the generator decides which and rejects
// the rest. Nothing else in a type position is followed by `(`.
type       : ID LPAREN type (COMMA type)? RPAREN | tensorType ;
tensorType : ID (LBRACK dims? RBRACK)? ;
dims       : dim (COMMA dim)* ;

// `?` is a dimension of unknown extent, an identifier a symbolic one.
dim : QUESTION | quotableId | INT ;

qualifiedId : ID (DOT ID)* ;

// An exporter's names carry `/` and `.`, so the printer quotes them.
quotableId : ID | STRING ;

ARROW    : '=>' ;
EQ       : '=' ;
LT       : '<' ;
GT       : '>' ;
LPAREN   : '(' ;
RPAREN   : ')' ;
LBRACK   : '[' ;
RBRACK   : ']' ;
LBRACE   : '{' ;
RBRACE   : '}' ;
COMMA    : ',' ;
COLON    : ':' ;
DOT      : '.' ;
QUESTION : '?' ;

FLOAT  : '-'? [0-9]+ ('.' [0-9]* EXPONENT? | EXPONENT) ;
INT    : '-'? [0-9]+ ;
STRING : '"' ~["]* '"' ;
ID     : [a-zA-Z_] [a-zA-Z0-9_]* ;

fragment EXPONENT : [eE] [+-]? [0-9]+ ;

COMMENT : '#' ~[\r\n]* -> skip ;
WS      : [ \t\r\n]+ -> skip ;
