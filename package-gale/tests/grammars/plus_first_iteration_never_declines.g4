grammar PlusFirstIterationNeverDeclines;
start : body EOF ;
body  : (A B | C D)+ (A B E)? ;
A : 'a' ;
B : 'b' ;
C : 'c' ;
D : 'd' ;
E : 'e' ;
WS : [ \t\r\n]+ -> skip ;
