// Source: hand-written Gale regression fixture for the fragment_entry option (off-variant copy for the on/off identity test).
// License: same terms as the Gale package (see package-gale/README.md).
//
// The start rule derives only `set`-items and `main` blocks. `stmt` is reached
// inside a `main` block, never at the top level — so a bare `stmt` fragment the
// start rule cannot derive still builds real `stmt` subtrees when `stmt` is a
// configured `fragment_entry`.

grammar FragmentEntryOff;

prog : item* EOF ;
item : 'set' ID ';' | main ;
main : 'main' '{' stmt* '}' ;
stmt : 'do' ID ';' ;

ID : [a-z]+ ;

WS : [ \t\r\n]+ -> skip ;
