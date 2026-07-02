grammar T;
s : a a;
a : {false}? ID INT {System.out.println("alt 1");}
  | {false}? ID INT {System.out.println("alt 2");}
  ;
ID : 'a'..'z'+ ;
INT : '0'..'9'+;
WS : (' '|'\n') -> skip ;
