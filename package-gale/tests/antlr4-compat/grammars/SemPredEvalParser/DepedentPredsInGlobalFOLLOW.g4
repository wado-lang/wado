grammar T;
@parser::members {
boolean pred(boolean v) {
	System.out.println("eval=" + v);
	return v;
}
}
s : a[99] ;
a[int i] : e {this.pred($i == 99)}? {System.out.println("parse");} '!' ;
b[int i] : e {this.pred($i == 99)}? ID ;
e : ID | ; // non-LL(1) so we use ATN
ID : 'a'..'z'+ ;
INT : '0'..'9'+;
WS : (' '|'\n') -> skip ;
