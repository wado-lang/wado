grammar M;
import S;
a : A {System.out.println("M.a: " + $A);};
A : 'abc' {System.out.println("M.A");};
WS : (' '|'\n') -> skip ;
