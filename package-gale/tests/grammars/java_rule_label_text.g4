// `$label.text` on a rule reference is the text that invocation matched — a
// span of its own, not the enclosing rule's `$text` and not a value-channel
// field. `sub` spans two tokens so a single-token shortcut would not pass.
grammar JavaRuleLabelText;

start : a* EOF ;

// `myLabel` is deliberately not snake_case: label disambiguation keys on the
// name codegen derives, which a snake_case label would not exercise.
// The alternatives are labelled, so each has a context of its own and
// `myLabel` names a call in one and a token in the other.
a : myLabel=sub {System.out.println($myLabel.text);} tail=sub {System.out.println($tail.text);} # Pair
  | myLabel='y' {System.out.println($myLabel.text);} # Y
  // Repeated, inside a group: the capture must reach whatever scope the call
  // lives in, not only an alternative's top level.
  | 'g' (inner=sub {System.out.println($inner.text);})+ # Group
  ;

sub : 'x' 'z' ;
WS : (' '|'\n') -> skip ;
