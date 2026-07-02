grammar T;
s : stmt EOF ;
stmt : ifStmt | ID;
ifStmt : 'if' ID stmt ('else' stmt | { _input.LA(1) != TParser.ELSE }?);
ELSE : 'else';
ID : [a-zA-Z]+;
WS : [ \n\t]+ -> skip;
