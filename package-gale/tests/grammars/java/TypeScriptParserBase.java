// The Java twin of `impl TypeScriptParserBase for TsParserBase` in
// package-gale/tests/driver_cst_typescript_test.wado, so `antlr4-oracle.sh --super`
// runs ANTLR4 against the base class Gale runs against. Edit both or neither.
import org.antlr.v4.runtime.Parser;
import org.antlr.v4.runtime.TokenStream;

public abstract class TypeScriptParserBase extends Parser {
    public TypeScriptParserBase(TokenStream input) {
        super(input);
    }

    // No line break between the token just consumed and the next one — what
    // JavaScript's automatic semicolon insertion turns on.
    protected boolean notLineTerminator() {
        return _input.LT(1).getLine() == _input.LT(-1).getLine();
    }

    protected boolean lineTerminatorAhead() {
        return _input.LT(1).getLine() != _input.LT(-1).getLine();
    }

    protected boolean closeBrace() {
        return _input.LA(1) == TypeScriptParser.CloseBrace;
    }

    // An expression statement may not start with `{`, `function` or
    // `interface` — those open a block, a declaration and a type instead.
    protected boolean notOpenBraceAndNotFunctionAndNotInterface() {
        int next = _input.LA(1);
        return next != TypeScriptParser.OpenBrace
            && next != TypeScriptParser.Function_
            && next != TypeScriptParser.Interface;
    }

    // The token just consumed is `s` — `for (x of xs)` asks it of `of`.
    protected boolean p(String s) {
        return _input.LT(-1).getText().equals(s);
    }

    // The next token is `s` — a getter / setter asks it of `get` / `set`.
    protected boolean n(String s) {
        return _input.LT(1).getText().equals(s);
    }
}
