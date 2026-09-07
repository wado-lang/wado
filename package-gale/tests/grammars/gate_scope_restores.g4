// Source: Gale test fixture (a rule that scopes a gate and puts it back)
// License: same as the Gale package
//
// A gate a bracketing rule clears never comes back, so the ban it lifted stays
// lifted for the rest of the enclosing head: `if f(x) || a { b }` reads
// `a { b }` as the braced form and takes the block the `if` still owes. A
// members field cannot express the restore, because the value to restore to is
// per-invocation.
//
// ANTLR4 spells that with `locals` + `@init` / `@after`, and the parse gets one
// copy per invocation from its own frame. The scan has to do the same, from its
// own call stack -- `args` below is where both save and restore.
//
// This is `RustParser.g4`'s `callParams`, which is why
// `if self.functions.has_impl(&info) || served { }` did not parse.
grammar GateScopeRestores;

@parser::members {
    int noBrace = 0;
}

start
    : stmt+ EOF
    ;

stmt
    : IF {noBrace = 1;} expr {noBrace = 0;} block
    | expr SEMI
    ;

block
    : LB ID RB
    ;

args
locals [int sv]
@init { $sv = noBrace; noBrace = 0; }
@after { noBrace = $sv; }
    : expr
    ;

expr
    : expr OR expr
    | ID LP args RP
    | {noBrace == 0}? ID LB ID RB
    | ID
    ;

IF   : 'if' ;
OR   : '||' ;
LP   : '(' ;
RP   : ')' ;
LB   : '{' ;
RB   : '}' ;
SEMI : ';' ;
ID   : [a-z]+ ;
WS   : [ \t\r\n]+ -> skip ;
