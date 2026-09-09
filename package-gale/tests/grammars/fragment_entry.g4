grammar FragmentEntry;

prog : item* EOF ;
item : 'set' ID ';' | main ;
main : 'main' '{' stmt* '}' ;
stmt : 'do' ID ';' ;

ID : [a-z]+ ;

WS : [ \t\r\n]+ -> skip ;
