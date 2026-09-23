// Distinct literals whose `TK_LIT_*` spellings coincide: '<' is named by the
// word 'LT' spells. 'self' / 'Self' differ only in case, which the name keeps.
// Every site naming a literal must agree on which one it means.
grammar LitNameClash;
prog : atom+ EOF ;
atom : 'self' | 'Self' | '<' | 'LT' ;
WS   : [ \t\r\n]+ -> skip ;
