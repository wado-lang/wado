grammar T;
start : statement+ ;
statement : 'x' | ifStatement;
ifStatement : 'if' 'y' statement ('else' statement)? {
System.out.println($text);
};
ID : 'a'..'z'+ ;
WS : (' '|'\n') -> channel(HIDDEN);
