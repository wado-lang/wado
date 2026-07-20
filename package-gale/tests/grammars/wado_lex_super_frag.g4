// Source: Gale test fixture (superClass predicate reached through a fragment)
// License: same as the Gale package
//
// The `{this.enabled()}?` predicate sits in a `fragment`, referenced by KW.
// A non-recursive fragment is inlined into the referencing rule's match fn, so
// `try_KW` evaluates `SlBase::enabled(...)` even though the call is textually in
// the fragment, not in KW's own body — `try_KW` must therefore carry
// `with SlBase`. Exercises the `RuleRef`-following effect marking.
lexer grammar WadoLexSuperFrag;

options { superClass = SlBase; language = Wado; }

KW : GATED ;
ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;

fragment GATED : { this.enabled() }? [a-z]+ ;
