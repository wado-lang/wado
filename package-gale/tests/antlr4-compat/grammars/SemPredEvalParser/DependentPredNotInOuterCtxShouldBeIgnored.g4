grammar T;
s : b[2] ';' |  b[2] '.' ; // decision in s drills down to ctx-dependent pred in a;
b[int i] : a[i] ;
a[int i]
  : {$i == 1}? ID {System.out.println("alt 1");}
    | {$i == 2}? ID {System.out.println("alt 2");}
    ;
ID : 'a'..'z'+ ;
INT : '0'..'9'+;
WS : (' '|'\n') -> skip ;
