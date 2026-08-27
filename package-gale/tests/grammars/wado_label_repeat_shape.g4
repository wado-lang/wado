// Source: Gale test fixture (a label must not change a repeat's shape decisions)
// License: same as the Gale package
//
// A Repeat supplies its own `List<>` typing, so lower peels both label forms off
// the body op. Every decision taken from the surface element has to peel with
// it, or a labelled repeat is analysed as though its body were not a group at
// all: the non-greedy exit loses the second-position discriminator, and a
// shape-lookahead optional loses its enumeration. Each rule below is paired
// with its label-free twin, and the pair must parse the same inputs the same
// way.
grammar WadoLabelRepeatShape;

options { language = Wado; }

ng_labelled : xs += ( A B )*? ( A C )* EOF ;

ng_bare : ( A B )*? ( A C )* EOF ;

opt_labelled : x = ( A? B )? C EOF ;

opt_bare : ( A? B )? C EOF ;

A : 'a' ;
B : 'b' ;
C : 'c' ;
WS : ' ' -> skip ;
