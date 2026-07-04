// Source: Gale test fixture (Stage C java2wado predicate gating)
// License: same as the Gale package
//
// The `SemPredEvalParser/Simple` shape: two single-token alts gated by
// context-independent member-field predicates, with actions that mutate the
// member so the next decision flips. java2wado translates the predicates and
// the actions; prediction-time gating selects the alt.
grammar JavaPred;

@parser::members {int mode = 1;}

s : a a ;

a : {this.mode == 1}? ID {this.mode = 2; System.out.print("one ");}
  | {this.mode == 2}? ID {this.mode = 1; System.out.print("two ");}
  ;

ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;
