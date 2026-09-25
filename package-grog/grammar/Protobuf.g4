// The `.proto` schema language: proto2, proto3, and editions 2023 and 2024 in
// one grammar. Productions follow `vendor/protobuf-spec/content/reference/
// protobuf/`, one spec per syntax; this is their union, and the `syntax` or
// `edition` statement decides which rules the generator applies.
//
// Every keyword is contextual: `message message = 1;` declares a field named
// `message` of type `message`, so `ident` admits each keyword token.
//
// An option's value may be a text-format message (`{ a: 1 }`), which the specs
// name through `MessageValue` without spelling out. `textMessage` covers it.
grammar Protobuf;

proto : (syntaxDecl | editionDecl)? topLevel* EOF ;

syntaxDecl  : SYNTAX EQ strLit SEMI ;
editionDecl : EDITION EQ strLit SEMI ;

topLevel : importDecl | packageDecl | optionDecl | messageDef | enumDef
         | extendDef | serviceDef | SEMI ;

importDecl  : IMPORT (WEAK | PUBLIC | OPTION)? strLit SEMI ;
packageDecl : PACKAGE fullIdent SEMI ;

optionDecl : OPTION optionName EQ constant SEMI ;
optionName : optionPart (DOT optionPart)* ;
optionPart : ident | LPAREN typeRef RPAREN ;

visibility : EXPORT | LOCAL ;

messageDef  : visibility? MESSAGE ident messageBody ;
messageBody : LBRACE messageElement* RBRACE ;
messageElement : field | group | mapField | oneof | messageDef | enumDef
               | extendDef | extensions | reserved | optionDecl | SEMI ;

field        : label? typeRef ident EQ INT fieldOptions? SEMI ;
label        : OPTIONAL | REQUIRED | REPEATED ;
group        : label? GROUP ident EQ INT fieldOptions? messageBody ;
mapField     : MAP LT typeRef COMMA typeRef GT ident EQ INT fieldOptions? SEMI ;
oneof        : ONEOF ident LBRACE oneofElement* RBRACE ;
oneofElement : optionDecl | oneofField | group | SEMI ;
oneofField   : typeRef ident EQ INT fieldOptions? SEMI ;

fieldOptions : LBRACK fieldOption (COMMA fieldOption)* RBRACK ;
fieldOption  : optionName EQ constant ;

extensions    : EXTENSIONS ranges fieldOptions? SEMI ;
reserved      : RESERVED (ranges | reservedNames) SEMI ;
ranges        : range (COMMA range)* ;
range         : signedInt (TO (signedInt | MAX))? ;
reservedNames : reservedName (COMMA reservedName)* ;
reservedName  : strLit | ident ;

enumDef     : visibility? ENUM ident enumBody ;
enumBody    : LBRACE enumElement* RBRACE ;
enumElement : optionDecl | enumValue | reserved | SEMI ;
enumValue   : ident EQ signedInt fieldOptions? SEMI ;

extendDef     : EXTEND typeRef LBRACE extendElement* RBRACE ;
extendElement : field | group | SEMI ;

serviceDef     : SERVICE ident LBRACE serviceElement* RBRACE ;
serviceElement : optionDecl | rpc | SEMI ;
rpc            : RPC ident LPAREN STREAM? typeRef RPAREN
                 RETURNS LPAREN STREAM? typeRef RPAREN (rpcBody | SEMI) ;
rpcBody        : LBRACE (optionDecl | SEMI)* RBRACE ;

// A leading `.` resolves from the root package instead of the enclosing scope.
typeRef   : DOT? fullIdent ;
fullIdent : ident (DOT ident)* ;

signedInt : MINUS? INT ;

// `inf`, `nan`, `true` and `false` are identifiers the generator reads by
// their text, as protoc's tokenizer does.
constant : (MINUS | PLUS)? (INT | FLOAT | fullIdent) | strLit | textMessage ;

textMessage : LBRACE textField* RBRACE | LT textField* GT ;
textField   : textFieldName (COLON textValue | textMessage) (SEMI | COMMA)? ;
// `[pkg.ext]` names an extension, `[type.googleapis.com/pkg.Msg]` an `Any`.
textFieldName : ident | LBRACK DOT? ident ((DOT | SLASH) ident)* RBRACK ;
textValue     : constant | LBRACK (constant (COMMA constant)*)? RBRACK ;

strLit : STRING+ ;

ident : IDENT | SYNTAX | EDITION | IMPORT | WEAK | PUBLIC | PACKAGE | OPTION
      | EXPORT | LOCAL | MESSAGE | OPTIONAL | REQUIRED | REPEATED | GROUP | MAP
      | ONEOF | EXTENSIONS | RESERVED | TO | MAX | ENUM | EXTEND | SERVICE
      | RPC | STREAM | RETURNS ;

SYNTAX     : 'syntax' ;
EDITION    : 'edition' ;
IMPORT     : 'import' ;
WEAK       : 'weak' ;
PUBLIC     : 'public' ;
PACKAGE    : 'package' ;
OPTION     : 'option' ;
EXPORT     : 'export' ;
LOCAL      : 'local' ;
MESSAGE    : 'message' ;
OPTIONAL   : 'optional' ;
REQUIRED   : 'required' ;
REPEATED   : 'repeated' ;
GROUP      : 'group' ;
MAP        : 'map' ;
ONEOF      : 'oneof' ;
EXTENSIONS : 'extensions' ;
RESERVED   : 'reserved' ;
TO         : 'to' ;
MAX        : 'max' ;
ENUM       : 'enum' ;
EXTEND     : 'extend' ;
SERVICE    : 'service' ;
RPC        : 'rpc' ;
STREAM     : 'stream' ;
RETURNS    : 'returns' ;

SEMI   : ';' ;
EQ     : '=' ;
COMMA  : ',' ;
DOT    : '.' ;
COLON  : ':' ;
SLASH  : '/' ;
MINUS  : '-' ;
PLUS   : '+' ;
LPAREN : '(' ;
RPAREN : ')' ;
LBRACE : '{' ;
RBRACE : '}' ;
LBRACK : '[' ;
RBRACK : ']' ;
LT     : '<' ;
GT     : '>' ;

FLOAT : DECIMALS '.' DECIMALS? EXPONENT?
      | DECIMALS EXPONENT
      | '.' DECIMALS EXPONENT?
      ;
INT : '0' [xX] [0-9a-fA-F]+
    | '0' [0-7]*
    | [1-9] [0-9]*
    ;
IDENT  : [a-zA-Z_] [a-zA-Z0-9_]* ;
STRING : '"' (ESCAPE | ~["\\\n])* '"'
       | '\'' (ESCAPE | ~['\\\n])* '\''
       ;

fragment DECIMALS : [0-9]+ ;
fragment EXPONENT : [eE] [+-]? DECIMALS ;
fragment ESCAPE   : '\\' ~[\n] ;

LINE_COMMENT  : '//' ~[\n]* -> channel(HIDDEN) ;
BLOCK_COMMENT : '/*' .*? '*/' -> channel(HIDDEN) ;
WS            : [ \t\r\n\f]+ -> skip ;
