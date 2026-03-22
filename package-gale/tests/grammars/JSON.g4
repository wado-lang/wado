/**
 * Taken from "The Definitive ANTLR 4 Reference" by Terence Parr.
 * Derived from https://json.org
 *
 * Original source: https://github.com/antlr/grammars-v4/blob/master/json/JSON.g4
 * ANTLR4 is licensed under the BSD 3-Clause License.
 * Copyright (c) 2012-2022, The ANTLR Project. All rights reserved.
 */

grammar JSON;

json
    : value EOF
    ;

obj
    : '{' pair (',' pair)* '}'
    | '{' '}'
    ;

pair
    : STRING ':' value
    ;

arr
    : '[' value (',' value)* ']'
    | '[' ']'
    ;

value
    : STRING
    | NUMBER
    | obj
    | arr
    | 'true'
    | 'false'
    | 'null'
    ;

STRING
    : '"' (ESC | SAFECODEPOINT)* '"'
    ;

fragment ESC
    : '\\' (["\\/bfnrt] | UNICODE)
    ;

fragment UNICODE
    : 'u' HEX HEX HEX HEX
    ;

fragment HEX
    : [0-9a-fA-F]
    ;

fragment SAFECODEPOINT
    : ~ ["\\\u0000-\u001F]
    ;

NUMBER
    : '-'? INT ('.' [0-9]+)? EXP?
    ;

fragment INT
    : '0'
    | [1-9] [0-9]*
    ;

fragment EXP
    : [Ee] [+-]? [0-9]+
    ;

WS
    : [ \t\n\r]+ -> skip
    ;
