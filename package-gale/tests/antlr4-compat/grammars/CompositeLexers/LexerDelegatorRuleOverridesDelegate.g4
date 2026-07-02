lexer grammar M;
import S;
A : 'a' B {System.out.println("M.A");} ;
WS : (' '|'\n') -> skip ;
