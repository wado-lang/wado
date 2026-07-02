grammar M;
import S, T;
b : 'b'|'c' {System.out.println("M.b");}|B|A;
WS : (' '|'\n') -> skip ;
