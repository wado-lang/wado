grammar T;
a : (assign|.)+ EOF ;
assign : ID '=' INT ';' {
System.out.println($text);
} ;
ID : 'a'..'z'+ ;
INT : '0'..'9'+;
WS : (' '|'\n') -> skip;
