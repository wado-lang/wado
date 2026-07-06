// A tiny four-function calculator grammar, fed to the Gale generator
// (wado-lang:gale) at compile time. Left-recursive with `# Label`s, which
// Gale rewrites into a precedence-climbing parser.
grammar Calc;

prog : expr EOF ;

expr : expr ('*' | '/') expr   # MulDiv
     | expr ('+' | '-') expr   # AddSub
     | '(' expr ')'            # Paren
     | INT                     # Num
     ;

INT : [0-9]+ ;
WS  : [ \t\r\n]+ -> skip ;
