// Source: Gale test fixture (Stage C superClass action ops, lexer)
// License: same as the Gale package
//
// A `superClass` base with **action** ops, not only predicates: `{ this.mark() }`
// and `{ this.done() }` lower to unit-returning `SaBase` operations, alongside
// the `{ this.enabled() }?` predicate op. Actions run through the winner replay,
// so `A`'s `mark()` stays silent when `AB` wins the longer match, and the
// mid-element `mark()` sees the cursor it was written at.
lexer grammar WadoLexSuperAction;

options { superClass = SaBase; language = Wado; }

AB : 'a' { this.mark() } 'b' { this.done() } ;
A  : 'a' { this.mark() } ;
KW : { this.enabled() }? [c-z]+ ;
ID : [c-z]+ ;
WS : [ \t\r\n]+ -> skip ;
