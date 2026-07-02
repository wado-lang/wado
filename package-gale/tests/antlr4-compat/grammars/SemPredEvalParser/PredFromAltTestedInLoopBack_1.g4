grammar T;
file_
@after {System.out.println($ctx.toStringTree(this));}
  : para para EOF ;
para: paraContent NL NL ;
paraContent : ('s'|'x'|{_input.LA(2) != TParser.NL}? NL)+ ;
NL : '\n' ;
s : 's' ;
X : 'x' ;
