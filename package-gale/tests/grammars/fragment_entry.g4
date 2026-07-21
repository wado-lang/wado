// Source: hand-written Gale regression fixture for the fragment_entry option.
// License: same terms as the Gale package (see package-gale/README.md).

grammar FragmentEntry;

prog : item* EOF ;
item : 'set' ID ';' | main ;
main : 'main' '{' stmt* '}' ;
stmt : 'do' ID ';' ;

ID : [a-z]+ ;

WS : [ \t\r\n]+ -> skip ;
