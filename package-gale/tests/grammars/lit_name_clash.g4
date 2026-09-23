// Distinct literals whose `TK_LIT_*` spellings coincide: case folding maps
// 'self' and 'Self' onto one name, and 'lt' spells the word '<' is named by.
// Every site naming a literal must agree on which one it means.
grammar LitNameClash;
prog : atom+ EOF ;
atom : 'self' | 'Self' | '<' | 'lt' ;
WS   : [ \t\r\n]+ -> skip ;
