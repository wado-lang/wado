grammar M;
import S;
s : a {System.out.print($a.text);} ;
B : 'b' ; // defines B from inherited token space
WS : (' '|'\n') -> skip ;
