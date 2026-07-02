grammar T;
a : (ID|ATN)* ATN? {System.out.println($text);} ;
ID : 'a'..'z'+ ;
ATN : '0'..'9'+;
WS : (' '|'\n') -> skip ;
