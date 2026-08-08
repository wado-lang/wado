#!/usr/bin/env bash
# Exercise antlr4-oracle.sh against the corpus grammars. Not a CI job: it
# needs java, javac and the cached jar, the same as regen-oracle.sh. Run it
# after touching the oracle — what a generated fixture pins comes from here,
# and nothing else checks it.
#
# Usage: scripts/antlr4-oracle-selftest.sh
set -uo pipefail
cd "$(dirname "$0")/.."

export ANTLR4_VERSION="${ANTLR4_VERSION:-4.13.2}"
ORACLE="scripts/antlr4-oracle.sh"
RUST_BASE="tests/grammars/java/RustLexerBase.java"
FAILED=0

# check <name> <input> <rc> <stdout-substring> <stderr-substring> -- <oracle args...>
# An empty stdout-substring asserts stdout is empty: a refused run must not
# leave a caller anything to pin.
check() {
    local name="$1" input="$2" want_rc="$3" want_out="$4" want_err="$5"
    shift 6  # name, input, rc, out, err, --
    local out rc err
    err=$(mktemp)
    out=$(printf '%s' "$input" | bash "$ORACLE" "$@" 2>"$err")
    rc=$?
    if [ "$rc" != "$want_rc" ]; then
        echo "FAIL $name: exit $rc, want $want_rc" >&2
        FAILED=$((FAILED + 1))
        rm -f "$err"
        return
    fi
    if [ -n "$want_err" ] && ! grep -qF "$want_err" "$err"; then
        echo "FAIL $name: stderr lacks '$want_err'" >&2
        sed 's/^/  /' "$err" >&2
        FAILED=$((FAILED + 1))
        rm -f "$err"
        return
    fi
    rm -f "$err"
    if [ -n "$want_out" ] && [[ "$out" != *"$want_out"* ]]; then
        echo "FAIL $name: stdout lacks '$want_out'" >&2
        echo "  got: $out" >&2
        FAILED=$((FAILED + 1))
        return
    fi
    if [ -z "$want_out" ] && [ -n "$out" ]; then
        echo "FAIL $name: expected no stdout, got: $out" >&2
        FAILED=$((FAILED + 1))
        return
    fi
    echo "ok   $name"
}

# Grammars with no superClass keep working unchanged.
check "json tree" '{"a":[1,2]}' 0 '(json (value (obj {' '' \
    -- tests/grammars/JSON.g4 json
check "json parse error" '{"a":' 2 '(json' '' \
    -- tests/grammars/JSON.g4 json
check "json tokens" '{"a":1}' 0 "[@0,0:0='{'" '' \
    -- --tokens tests/grammars/JSON.g4

# A superClass grammar with no base class is refused, not compiled and failed.
check "superClass unsupplied" '0x1f' 1 '' \
    "declares 'options { superClass = RustLexerBase; }'" \
    -- --tokens tests/grammars/RustLexer.g4
check "probe-super without superClass" '{"a":1}' 1 '' 'declares no superClass' \
    -- --tokens --probe-super tests/grammars/JSON.g4
check "super and probe-super conflict" '0x1f' 1 '' 'mutually exclusive' \
    -- --tokens --probe-super --super "$RUST_BASE" tests/grammars/RustLexer.g4

# --super is the only path that answers. `1.` is the case the base class
# decides — FLOAT_LITERAL only because FloatDotPossible() says so.
check "super 0x1f" '0x1f' 0 "'0x1f',<INTEGER_LITERAL>" '' \
    -- --tokens --super "$RUST_BASE" tests/grammars/RustLexer.g4
check "super float dot" '1.' 0 "'1.',<FLOAT_LITERAL>" '' \
    -- --tokens --super "$RUST_BASE" tests/grammars/RustLexer.g4

# --probe-super reports, never answers: exit 3 and empty stdout every time,
# so no caller can pin it. It still says which members the input reached and
# whether the predicates changed the outcome.
check "probe reports reached members" '1.' 3 '' 'FloatDotPossible' \
    -- --tokens --probe-super tests/grammars/RustLexer.g4
check "probe reports polarity split" '1.' 3 '' 'with every predicate false' \
    -- --tokens --probe-super tests/grammars/RustLexer.g4
check "probe reports agreement" '0x1f' 3 '' 'same answer under both predicate polarities' \
    -- --tokens --probe-super tests/grammars/RustLexer.g4
check "probe never certifies" '0x1f' 3 '' 'NOT an oracle' \
    -- --tokens --probe-super tests/grammars/RustLexer.g4
check "probe on typescript" 'function f() {}' 3 '' 'ProcessOpenBrace' \
    -- --tokens --probe-super tests/grammars/TypeScriptLexer.g4

# ANTLRv4Lexer is why the probe cannot certify. It declares TOKEN_REF and
# RULE_REF in `tokens {}` with no rule producing them, so LexerAdaptor must
# assign them from an override the grammar never names. `A : B ;` reaches no
# LexerAdaptor member and both polarities agree, yet the stub's `ID` is not
# ANTLR4's answer — so this must stay unpinnable.
check "probe cannot certify antlr4 rule" 'A : B ;' 3 '' \
    'base-class members this input reached: none' \
    -- --tokens --probe-super tests/grammars/ANTLRv4Lexer.g4

# Stub-generation shapes no corpus grammar reaches today. They are all one
# `.g4` line away, so keep them pinned here rather than in tests/grammars/.
FIXTURES=$(mktemp -d)
trap 'rm -rf "$FIXTURES"' EXIT

cat > "$FIXTURES/DupLex.g4" <<'EOF'
lexer grammar DupLex;
options { superClass = DupBase; }
GET : {this.n("get")}? 'get' ;
SET : {this.n("set")}? 'set' ;
EOF
# Two call sites, one Java signature. Emitting both is a javac "already
# defined", which would exit 1 before any run — reaching a report at all is
# the assertion. TypeScriptParser.g4 already has this shape.
check "duplicate call sites collapse" 'get' 3 '' "'get',<GET>" \
    -- --tokens --probe-super "$FIXTURES/DupLex.g4"

cat > "$FIXTURES/DualLex.g4" <<'EOF'
lexer grammar DualLex;
options { superClass = DualBase; }
KW  : {this.f()}? 'kw' ;
NUM : [0-9]+ {this.f();} ;
EOF
# `f` is a predicate and an action. A boolean stub that did not record itself
# would run the action side invisibly.
check "dual-role member is recorded" '12' 3 '' 'reached:
oracle:   f' \
    -- --tokens --probe-super "$FIXTURES/DualLex.g4"

cat > "$FIXTURES/OpaqueLex.g4" <<'EOF'
lexer grammar OpaqueLex;
options { superClass = OpaqueBase; }
ID : [a-z]+ {this.f(getText());} ;
EOF
# A non-literal argument has no inferable type; say so instead of letting
# javac report a missing symbol.
check "non-literal argument refused" 'abc' 1 '' 'cannot infer a signature' \
    -- --tokens --probe-super "$FIXTURES/OpaqueLex.g4"

if [ "$FAILED" -ne 0 ]; then
    echo "$FAILED check(s) failed" >&2
    exit 1
fi
echo "all checks passed"
