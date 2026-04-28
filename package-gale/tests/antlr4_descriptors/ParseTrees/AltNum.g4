grammar T;

options { contextSuperClass=MyRuleNode; }

<TreeNodeWithAltNumField(X="T")>


s
@init {
<BuildParseTrees()>
}
@after {
<ToStringTree("$r.ctx"):writeln()>
}
  : r=a ;

a : 'f'
  | 'g'
  | 'x' b 'z'
  ;
b : 'e' {} | 'y'
  ;
