// Source: Gale test fixture (Stage C superClass effect interface, parser)
// License: same as the Gale package
//
// A combined grammar whose `superClass` is the *parser's* base, as ANTLR4
// defines it (`vendor/antlr4/doc/options.md`). `shr` accepts `>>` only for an
// adjacent pair, which the grammar cannot see, so it asks the base. `pick`'s
// alt-initial predicate takes the other emit path: a dispatch gate.
grammar WadoPrsSuper;

options { superClass = PsBase; language = Wado; }

prog : shr EOF ;

shr : GT { this.next_gt() }? GT ;

pick
    : { this.first_wins() }? ID
    | ID
    ;

GT : '>' ;
ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;
