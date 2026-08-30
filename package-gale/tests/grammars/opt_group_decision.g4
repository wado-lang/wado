// Source: hand-written regression grammar for Gale's optional-group decision.
// License: same as the Gale package.
//
// An optional whose group needs a longest-match decision. The scan that
// decides whether to enter the group is the scan that picks the alternative,
// so the two cannot disagree:
//
//   * `name` and `word` tie at one token — the lowest alternative wins;
//   * `name '.' name` scans strictly further, so it beats both;
//   * a lookahead no alternative matches leaves the optional unentered;
//   * `.` overlaps every alternative, and loses to a concrete alternative
//     that scans further rather than to an iteration order (invariant 4).
grammar OptGroupDecision;

s : 'K' body ';' EOF
  | 'W' wild EOF
  ;

body : ( name | word | name '.' name )? ;

wild : ( assign | . )? ;

assign : ID '=' ID ;

name : ID ;

word : ID ;

ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;
