grammar T;
s
@init {
setBuildParseTree(true);
}
@after {
System.out.println($r.ctx.toStringTree(this));
}
  : r=a ;
a : b 'x'
  ;
b : 'y'
  ;
