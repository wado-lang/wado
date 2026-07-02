grammar T;
@parser::members {
boolean pred(boolean v) {
	System.out.println("eval=" + v);
	return v;
}
}
s : e {this.pred(true)}? {System.out.println("parse");} '!' ;
t : e {this.pred(false)}? ID ;
e : ID | ; // non-LL(1) so we use ATN
ID : 'a'..'z'+ ;
INT : '0'..'9'+;
WS : (' '|'\n') -> skip ;
