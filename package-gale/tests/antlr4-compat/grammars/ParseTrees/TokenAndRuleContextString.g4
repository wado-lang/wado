grammar T;
<ImportRuleInvocationStack()>

s
@init {
<BuildParseTrees()>
}
@after {
<ToStringTree("$r.ctx"):writeln()>
}
  : r=a ;
a : 'x' {
<RuleInvocationStack():writeln()>
} ;
