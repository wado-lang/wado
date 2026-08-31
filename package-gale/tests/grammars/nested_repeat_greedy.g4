// Source: hand-written regression grammar for Gale's repeat-over-repeat
// collapse.
// License: same as the Gale package.
//
// A repeat over a repeat collapses to one repeat, and the collapse has one
// greediness flag to carry two loops'. They do not compose: the outer loop
// re-enters whatever the inner one stopped short of, so the surviving loop's
// greediness is the outer one's. The jar agrees — all three of `('x'+?)+ 'x'`,
// `('x'+)+ 'x'` and the plain `'x'+ 'x'` accept the same inputs.
//
// A preference to *skip* does compose, because two skip decisions are one
// decision: the `'e'` arm keeps it, and `( X | )` mints exactly that shape.
grammar NestedRepeatGreedy;

s : 'a' ('x'+?)+ 'x' EOF
  | 'b' ('x'+)+ 'x' EOF
  | 'd' 'x'+ 'x' EOF
  | 'e' (('x' | ))? 'y' EOF
  ;

WS : [ \t\r\n]+ -> skip ;
