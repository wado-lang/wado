grammar M;
import S,T;
s : x y ; // matches AA, which should be 'aa'
B : 'b' ; // another order: B, A, C
A : 'a' ;
C : 'c' ;
WS : (' '|'\n') -> skip ;
