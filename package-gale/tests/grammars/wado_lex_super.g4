// Source: Gale test fixture (Stage C superClass effect interface, lexer)
// License: same as the Gale package
//
// A `language = Wado` lexer grammar with `options { superClass = SlBase }`.
// The alt-initial predicate `{ this.enabled() }?` on KW calls a base method,
// which Gale lowers to the ambient effect operation `SlBase::enabled()`.
// KW and ID both match `[a-z]+`; when the handler reports `enabled() == true`
// the tie resolves to the first-declared rule (KW), otherwise KW is rejected
// and ID wins. The base state lives in a user-written `impl SlBase`, installed
// with `with SlBase => &mut base do { ... }`.
lexer grammar WadoLexSuper;

options { superClass = SlBase; language = Wado; }

KW : { this.enabled() }? [a-z]+ ;
ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;
