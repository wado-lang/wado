// A label inside a repeat's body retypes what it binds and nothing else. Every
// decision taken from the surface element has to read through it, or the body
// is analysed as though it were not a group at all: the non-greedy exit loses
// the second-position discriminator, and the shape-lookahead optional loses its
// enumeration. Each rule below is paired with its label-free twin, and the pair
// must parse the same inputs the same way.
//
// The label sits on an element rather than on the block, because ANTLR4 labels
// a block only when it is a set: `xs += ( A B )*?` is `error(130)`.
grammar WadoLabelRepeatShape;

options { language = Wado; }

ng_labelled : ( xs += A B )*? ( A C )* EOF ;

ng_bare : ( A B )*? ( A C )* EOF ;

opt_labelled : ( A? x = B )? C EOF ;

opt_bare : ( A? B )? C EOF ;

A : 'a' ;
B : 'b' ;
C : 'c' ;
WS : ' ' -> skip ;
