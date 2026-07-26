// Source: Gale test fixture (Stage C: $<ruleLabel>.text)
// License: same as the Gale package
//
// `$label.text` where `label=` binds a rule reference is the text that rule
// invocation matched — a span of its own, not the enclosing rule's `$text` and
// not a value-channel field. `sub` spans two tokens so a single-token shortcut
// would not pass.
grammar JavaRuleLabelText;

start : a* EOF ;

// `myLabel` is deliberately not snake_case: the per-alternative label-kind
// disambiguation keys on the binding name codegen derives, so a label that is
// already snake_case would not exercise it.
a : myLabel=sub {System.out.println($myLabel.text);} tail=sub {System.out.println($tail.text);}
  | myLabel='y' {System.out.println($myLabel.text);}
  // Inside a group, and repeated: the capture must be declared in whatever
  // scope the call lives in, not only at an alternative's top level.
  | 'g' (inner=sub {System.out.println($inner.text);})+
  ;

sub : 'x' 'z' ;
WS : (' '|'\n') -> skip ;
