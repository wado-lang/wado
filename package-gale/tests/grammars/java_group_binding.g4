// An inlined group body binds its own elements: `$B` inside the group is the
// group's `B`, not the outer alternative's same-named one.
grammar JavaGroupBinding;

a : B ( B {System.out.println($B.text);} ) ;

B : [bc] ;
WS : ' ' -> skip ;
