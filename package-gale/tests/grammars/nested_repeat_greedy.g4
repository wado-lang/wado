// Source: hand-written regression grammar for Gale's repeat-over-repeat
// collapse.
// License: same as the Gale package.
//
// A repeat over a repeat collapses to one repeat, and the collapse has one
// greediness flag to carry two. The one that survives is the surviving loop's:
//
//   outer `*` / `+` loops and re-enters whatever the inner stopped short of,
//   so the outer's preference is the whole of it — `('x'+?)+ 'x'` accepts what
//   `('x'+)+ 'x'` accepts;
//   outer `?` does not loop, so the inner loop's preference carries, and the
//   outer's preference to skip points the same way and composes with it.
//
// The jar accepts every input below for every spelling.
grammar NestedRepeatGreedy;

s : 'a' ('x'+?)+ 'x' EOF
  | 'b' ('x'+)+ 'x' EOF
  | 'd' 'x'+ 'x' EOF
  | 'e' (('x' | ))? 'y' EOF
  | 'f' ('x'+?)? 'x' EOF
  | 'g' ('x'+)? 'x' EOF
  ;

WS : [ \t\r\n]+ -> skip ;
