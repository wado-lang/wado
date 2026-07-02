grammar T;
a : (A {System.out.println($A.text);})+ ;
A : [AaBb] ;
WS : (' '|'\n')+ -> skip ;
