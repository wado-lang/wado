// `(A B | C D)+ (A B E)?` is the shape. The loop's head and its optional
// suffix's overlap, so the loop declines an iteration that would strand the
// suffix. Never its first, though: that one is mandatory and takes what
// prediction picked. On `a b e` the loop owes an iteration the suffix cannot
// spare, so the input has no reading at all.
grammar PlusFirstIterationNeverDeclines;
start : body EOF ;
body  : (A B | C D)+ (A B E)? ;
A : 'a' ;
B : 'b' ;
C : 'c' ;
D : 'd' ;
E : 'e' ;
WS : [ \t\r\n]+ -> skip ;
