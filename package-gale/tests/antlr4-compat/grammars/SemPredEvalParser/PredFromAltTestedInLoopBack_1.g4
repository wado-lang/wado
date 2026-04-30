grammar T;
file_
@after {<ToStringTree("$ctx"):writeln()>}
  : para para EOF ;
para: paraContent NL NL ;
paraContent : ('s'|'x'|{<LANotEquals("2",{T<ParserToken("Parser", "NL")>})>}? NL)+ ;
NL : '\n' ;
s : 's' ;
X : 'x' ;
