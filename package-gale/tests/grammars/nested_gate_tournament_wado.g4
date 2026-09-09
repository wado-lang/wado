// `nested_gate_tournament.g4`'s grammar with its actions written in Wado.
//
// The gate a scan simulates has to be the gate the parse evaluates, and one
// collector writes both. It used to read the grammar text and snake_case the
// member name to guess what the translator would call it — right for Java by
// construction, and wrong for every Wado grammar, whose members keep the name
// they were written with. No `language = Wado` grammar had a simulable gate,
// so this one mis-parsed where the Java original above did not.
//
// Both languages name a member the one way once translated, which is the form
// the gate analysis reads now. This is the only grammar in the corpus that asks
// the question in Wado, so it is what says the two languages still agree.
grammar NestedGateTournamentWado;

options {
    language = Wado;
}

@parser::members {
    noBrace: i32 = 0
}

start
    : block EOF
    ;

block
    : LB stmts RB
    ;

stmts
    : stmt+ expr?
    | expr
    ;

stmt
    : expr SEMI
    ;

blk
    : LB ID RB
    ;

expr
    : expr EQ expr
    | IF {p.noBrace = 1} expr {p.noBrace = 0} blk
    | {p.noBrace == 0}? ID LB ID RB
    | ID
    ;

IF   : 'if' ;
EQ   : '=' ;
ID   : [a-z]+ ;
LB   : '{' ;
RB   : '}' ;
SEMI : ';' ;
WS   : [ \t\r\n]+ -> skip ;
