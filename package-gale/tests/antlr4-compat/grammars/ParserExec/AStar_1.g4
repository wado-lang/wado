grammar T;
a : ID* {
System.out.println($text);
};
ID : 'a'..'z'+;
WS : (' '|'\n') -> skip;
