grammar T;

s
@init {
setBuildParseTree(true);
}
@after {
System.out.println($ctx.toStringTree(this));
}
  : '${' v '}'
  ;

v : A #altA
  | B #altB
  ;

A : 'a' ;
B : 'b' ;

WHITESPACE : [ \n\t\r]+ -> channel(HIDDEN) ;

ERROR : . ;
