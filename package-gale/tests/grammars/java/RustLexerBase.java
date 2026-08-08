// Source: written for Gale from the `impl RustLexerBase for RustBase` in
// package-gale/tests/driver_cst_rust_test.wado — not ported from
// grammars-v4's RustLexerBase.java (see License hygiene in AGENTS.md).
// License: same as the Gale package
//
// The Java twin of that Wado impl, so `antlr4-oracle.sh --super` runs ANTLR4
// against the base class Gale runs against. Edit both or neither: a lone edit
// silently re-opens the gap this file closes.
import org.antlr.v4.runtime.CharStream;
import org.antlr.v4.runtime.Lexer;

public abstract class RustLexerBase extends Lexer {
    public RustLexerBase(CharStream input) {
        super(input);
    }

    public boolean SOF() {
        return _input.LA(-1) == -1;
    }

    public boolean FloatLiteralPossible() {
        return _input.LA(-1) != '.';
    }

    public boolean FloatDotPossible() {
        int c = _input.LA(1);
        if (c == '.' || c == '_') {
            return false;
        }
        return !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z');
    }
}
