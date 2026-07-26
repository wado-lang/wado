// Source: Gale test fixture (Stage C: $<ruleLabel>.text)
// License: same as the Gale package
//
// `$label.text` on a rule reference is the text that invocation matched — a
// span of its own, not the enclosing rule's `$text` and not a value-channel
// field. `sub` spans two tokens so a single-token shortcut would not pass.
grammar JavaRuleLabelText;

start : a* EOF ;

// `myLabel` is deliberately not snake_case: label disambiguation keys on the
// name codegen derives, which a snake_case label would not exercise.
a : myLabel=sub {System.out.println($myLabel.text);} tail=sub {System.out.println($tail.text);}
  | myLabel='y' {System.out.println($myLabel.text);}
  // Repeated, inside a group: the capture must reach whatever scope the call
  // lives in, not only an alternative's top level.
  | 'g' (inner=sub {System.out.println($inner.text);})+
  ;

sub : 'x' 'z' ;
WS : (' '|'\n') -> skip ;
