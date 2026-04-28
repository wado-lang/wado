grammar T;

s
@init {
<BuildParseTrees()>
}
@after {
<ToStringTree("$ctx"):writeln()>
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
