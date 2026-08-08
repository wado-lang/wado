// Source: written for Gale — the Java twin of the `impl RustLexerBase for
// RustBase` in package-gale/tests/driver_cst_rust_test.wado.
// License: same as the Gale package
//
// RustLexer.g4 declares `options { superClass = RustLexerBase; }`, so its
// tokenization is only defined together with a base class. Gale's driver test
// models one in Wado; this file is the same specification in Java, so
// `antlr4-oracle.sh --super` runs ANTLR4 against exactly the base class Gale
// runs against and a divergence is a Gale bug rather than two different
// grammars disagreeing.
//
// Written from the Wado impl, not ported from grammars-v4's RustLexerBase.java
// (see the License hygiene section in package-gale/AGENTS.md). Keep the two in
// sync: an edit to either side without the other silently re-opens the gap
// this file exists to close.
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
