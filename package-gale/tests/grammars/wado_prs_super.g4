// Source: Gale test fixture (Stage C superClass effect interface, parser)
// License: same as the Gale package
//
// A combined grammar whose `superClass` is the *parser's* base, as ANTLR4
// defines it (`vendor/antlr4/doc/options.md`). `shr` accepts `>>` only when the
// two `GT` tokens are adjacent in the source, which the grammar cannot see:
// `{ this.next_gt() }?` asks the base, and Gale lowers it to the ambient
// operation `PsBase::next_gt(p)` reading the recognizer. `pick`'s alt-initial
// predicate takes the other emit path — a dispatch gate between interchangeable
// alternatives rather than an inline guard.
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
