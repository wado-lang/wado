// Source: hand-written for Gale's tutorial — the JavaScript half of a
// composite page grammar. Composed into `MiniHtml.g4` by its `import`.
// License: same as the Gale package.
//
// Mode-hosted and prefixed for the same two reasons as `MiniCss.g4`.
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
// stack: a `JS_IDENT` under `params` is `@variable.parameter`, elsewhere
// `@variable`.
//
// `term` therefore builds with a shared-lookahead warning: `arrow` and `group`
// are not token-led, so the generated parser settles them with a longest-match
// scan tournament. That warning is the demo working, not a defect.
grammar MiniJs;

// No `EOF`: a script is a fragment of the host document, not a file.
program : statement* ;

statement
    : JS_LET JS_IDENT JS_ASSIGN expr JS_SEMI
    | expr JS_SEMI
    ;

expr : term ((JS_PLUS | JS_STAR) term)* ;

term
    : arrow
    | group
    | call
    | JS_IDENT
    | JS_NUMBER
    | JS_STRING
    ;

arrow  : JS_LPAREN params? JS_RPAREN JS_ARROW expr ;
params : JS_IDENT (JS_COMMA JS_IDENT)* ;
group  : JS_LPAREN expr JS_RPAREN ;
call   : JS_IDENT JS_LPAREN (expr (JS_COMMA expr)*)? JS_RPAREN ;

mode JS;
JS_LET     : 'let' ;
JS_ARROW   : '=>' ;
JS_ASSIGN  : '=' ;
JS_LPAREN  : '(' ;
JS_RPAREN  : ')' ;
JS_COMMA   : ',' ;
JS_SEMI    : ';' ;
JS_PLUS    : '+' ;
JS_STAR    : '*' ;
JS_NUMBER  : [0-9]+ ;
JS_STRING  : '"' ~["]* '"' ;
JS_IDENT   : [a-zA-Z_] [a-zA-Z0-9_]* ;
JS_COMMENT : '//' ~[\r\n]* -> channel(HIDDEN) ;
JS_WS      : [ \t\r\n]+ -> skip ;
