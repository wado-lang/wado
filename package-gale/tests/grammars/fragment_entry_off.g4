// Source: hand-written Gale regression fixture for the fragment_entry option
// (off-variant copy for the on/off identity test).
// License: same terms as the Gale package (see package-gale/README.md).

grammar FragmentEntryOff;

prog : item* EOF ;
item : 'set' ID ';' | main ;
main : 'main' '{' stmt* '}' ;
stmt : 'do' ID ';' ;

ID : [a-z]+ ;

WS : [ \t\r\n]+ -> skip ;
