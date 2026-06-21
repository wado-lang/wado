// Source: hand-written for Gale's tutorial — a four-function calculator.
// License: same as the Gale package.
//
// Left-recursive with `# Label`s. ANTLR4 / Gale rewrite the direct left
// recursion into a precedence-climbing parser: alternatives listed earlier
// bind tighter, and operators grouped in one alternative share a precedence
// level and associate left. So `*` / `/` bind tighter than `+` / `-`, and
// `6 / 2 * 3` is `(6 / 2) * 3`.
//
// Because the rule is labeled, the generated parser exposes an `ExprAlt`
// enum and an `expr_alt(node)` accessor; the interpreter in `eval.wado`
// dispatches on those rather than inspecting tree shape.
grammar Arith;

prog : expr EOF ;

expr : expr ('*' | '/') expr   # MulDiv
     | expr ('+' | '-') expr   # AddSub
     | '(' expr ')'            # Paren
     | INT                     # Num
     ;

INT : [0-9]+ ;
WS  : [ \t\r\n]+ -> skip ;
