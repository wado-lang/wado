// Source: hand-written for Gale's tutorial — the CSS subset a `<style>` body
// needs in `MiniHtml.g4`.
// License: same as the Gale package.
//
// Every name here is the same `IDENT` token. What each one *is* — a selector,
// a property, or a value — is where the parser put it, not anything the lexer
// saw:
//
//     color { color: color; }
//
// is a selector, a property and a value spelled identically.
// `MiniCss.highlights.scm` reads the three apart off the rule stack.
grammar MiniCss;

stylesheet  : ruleset* EOF ;
ruleset     : selector LBRACE declaration* RBRACE ;
selector    : IDENT (COMMA IDENT)* ;
declaration : property COLON value SEMI ;
property    : IDENT ;
value       : IDENT | NUMBER | HASH ;

LBRACE : '{' ;
RBRACE : '}' ;
COLON  : ':' ;
SEMI   : ';' ;
COMMA  : ',' ;
HASH   : '#' [0-9a-fA-F]+ ;
NUMBER : [0-9]+ ('px' | '%')? ;
IDENT  : [a-zA-Z-] [a-zA-Z0-9-]* ;
BLOCK_COMMENT : '/*' .*? '*/' -> channel(HIDDEN) ;
WS     : [ \t\r\n]+ -> skip ;
