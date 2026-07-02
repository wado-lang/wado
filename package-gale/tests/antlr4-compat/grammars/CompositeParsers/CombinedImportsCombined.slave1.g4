parser grammar S;
tokens { A, B, C }
x : 'x' INT {System.out.println("S.x");};
INT : '0'..'9'+ ;
WS : (' '|'\n') -> skip ;
