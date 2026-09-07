grammar ReserveYieldsToAnotherIteration;
start : xs EOF ;
xs : (A | B)* A ;
A : 'a' ;
B : 'b' ;
WS : [ \t\r\n]+ -> skip ;
