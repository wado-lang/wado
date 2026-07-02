grammar T;
@parser::members{
void foo() {
	SContext s = null;
	s.a();
	s.b();
}
}
s : (a | b)+;
a : 'a' {System.out.print("a");};
b : 'b' {System.out.print("b");};
