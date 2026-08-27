// Source: Gale test fixture (Stage C rule-invocation stack and `$x.ctx`)
// License: same as the Gale package
//
// `p.rule_stack()` is ANTLR's `getRuleInvocationStack()`: the rules entered and
// not yet finished, innermost first. `$r.ctx` renders the node one labeled rule
// call built — the tree of `a`, not of the enclosing `s`.
grammar WadoRuleStack;

options { language = Wado; }

s @after { p.emit($r.ctx) } : r=a ;

a : A { p.emit(`${p.rule_stack()}\n`) } ;

A : 'x' ;
WS : ' ' -> skip ;
