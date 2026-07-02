grammar T;
a : (ID|ID)+ {
System.out.println($text);
};
ID : 'a'..'z'+;
WS : (' '|'\n') -> skip;
