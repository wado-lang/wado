grammar T;
file_ @init{
setErrorHandler(new BailErrorStrategy());
}
@after {
System.out.println($ctx.toStringTree(this));
}
  :   item (SEMICOLON item)* SEMICOLON? EOF ;
item : A B?;
SEMICOLON: ';';
A : 'a'|'A';
B : 'b'|'B';
WS      : [ \r\t\n]+ -> skip;
