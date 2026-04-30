grammar T;
a : (assign|.)+ EOF ;
assign : ID '=' INT ';' {
<writeln("$text")>
} ;
ID : 'a'..'z'+ ;
INT : '0'..'9'+;
WS : (' '|'\n') -> skip;
