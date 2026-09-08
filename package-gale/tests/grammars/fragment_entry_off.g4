// The off-variant copy of `fragment_entry.g4`, for the on/off identity test.

grammar FragmentEntryOff;

prog : item* EOF ;
item : 'set' ID ';' | main ;
main : 'main' '{' stmt* '}' ;
stmt : 'do' ID ';' ;

ID : [a-z]+ ;

WS : [ \t\r\n]+ -> skip ;
