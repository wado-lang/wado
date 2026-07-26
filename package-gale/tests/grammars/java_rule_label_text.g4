// Source: Gale test fixture (Stage C: $<ruleLabel>.text)
// License: same as the Gale package
//
// `$label.text` where `label=` binds a rule reference is the text that rule
// invocation matched — a span of its own, not the enclosing rule's `$text` and
// not a value-channel field. `sub` spans two tokens so a single-token shortcut
// would not pass.
grammar JavaRuleLabelText;

start : a* EOF ;

a : label=sub {System.out.println($label.text);} tail=sub {System.out.println($tail.text);}
  | label='y' {System.out.println($label.text);}
  ;

sub : 'x' 'z' ;
WS : (' '|'\n') -> skip ;
