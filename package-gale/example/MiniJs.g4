// Source: hand-written for Gale's tutorial — the JavaScript subset a
// `<script>` body needs in `MiniHtml.g4`.
// License: same as the Gale package.
//
// The grammar exists for one classification a lexer cannot make. `arrow` and
// `group` both open with `(`, so what an identifier inside the parentheses is
// — a parameter or a variable — is settled by the `=>` that follows the
// closing paren, arbitrarily many tokens later:
//
//     let add = (a, b) => a + b;   // a, b are parameters
//     let one = (a);               // a is a variable
//
// No lexer can answer that when it reads `a`; it has not seen the deciding
// token yet, and no amount of mode-stack state brings it closer. The parser
// resolves it, and `MiniJs.highlights.scm` reads the answer off the rule
// stack: an `IDENT` under `params` is `@variable.parameter`, elsewhere
// `@variable`.
//
// `term` therefore builds with a shared-lookahead warning: `arrow` and `group`
// are not token-led, so the generated parser settles them with a longest-match
// scan tournament. That warning is the demo working, not a defect.
grammar MiniJs;

program : statement* EOF ;

statement
    : LET IDENT ASSIGN expr SEMI
    | expr SEMI
    ;

expr : term ((PLUS | STAR) term)* ;

term
    : arrow
    | group
    | call
    | IDENT
    | NUMBER
    | STRING
    ;

arrow  : LPAREN params? RPAREN ARROW expr ;
params : IDENT (COMMA IDENT)* ;
group  : LPAREN expr RPAREN ;
call   : IDENT LPAREN (expr (COMMA expr)*)? RPAREN ;

LET    : 'let' ;
ARROW  : '=>' ;
ASSIGN : '=' ;
LPAREN : '(' ;
RPAREN : ')' ;
COMMA  : ',' ;
SEMI   : ';' ;
PLUS   : '+' ;
STAR   : '*' ;
NUMBER : [0-9]+ ;
STRING : '"' ~["]* '"' ;
IDENT  : [a-zA-Z_] [a-zA-Z0-9_]* ;
LINE_COMMENT : '//' ~[\r\n]* -> channel(HIDDEN) ;
WS     : [ \t\r\n]+ -> skip ;
