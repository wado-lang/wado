#!/usr/bin/env bash
# Run the published ANTLR4 jar as a clean-room oracle on a .g4 grammar +
# input. Used to validate descriptor-level expectations for ATN-class
# cases that Gale's static prediction cannot resolve. See the License
# hygiene section in package-gale/AGENTS.md: we may run the published
# jar to observe its black-box behavior, but we may not read ANTLR4's
# implementation source.
#
# Caches the jar at ~/.cache/gale/antlr-4.13.2-complete.jar on first
# use (downloads from antlr.org). Generates Java sources, compiles
# them, runs TestRig with -tree, and emits the resulting S-expression
# parse tree to stdout. The Java sources, .class files, and lexed
# tokens stay in a per-invocation temp directory that is removed on
# exit unless ORACLE_KEEP=1 is set.
#
# Usage:
#   package-gale/scripts/antlr4-oracle.sh <grammar.g4> <start_rule> < input
#   package-gale/scripts/antlr4-oracle.sh --tokens <grammar.g4> < input
#
# Options:
#   --tokens        print the token stream instead of the parse tree (lexer
#                   oracle; <start_rule> is omitted).
#   --super <file>  compile <file> alongside the generated recognizer.
#                   Repeatable. Supplies the hand-written base class an
#                   `options { superClass = X }` grammar extends.
#   --probe-super   report on stderr what the input does against a
#                   synthesized base class, and always exit 3.
#
# A grammar with `options { superClass = X }` has no behaviour without X,
# so `--super` is the only way to oracle one and `--probe-super` is a
# diagnostic that never yields pinnable output. Why the probe cannot be
# promoted to an oracle: "Oracling a superClass grammar" in
# antlr4-compatibility.md.
#
# Exit codes:
#   0 = oracle ran and printed output
#   1 = invocation error (missing args, missing jar after download, etc.)
#   2 = ANTLR4 reported a parse error on the input
#   3 = --probe-super ran; its report is on stderr and stdout is empty

set -euo pipefail

# We deliberately do NOT pin a specific ANTLR4 jar version. Each
# extract resolves the current latest release from Maven Central and
# caches it locally; the resolved version is written to
# `$CACHE_DIR/antlr4-resolved-version` on every run so the descriptor
# extractor (a separate process — an `export` here could never reach
# it) can stamp it into the generated test files. Reproducibility is
# preserved via that comment in the committed test file: any drift in
# the oracle's answer (caused by an ANTLR4 patch release) surfaces as a
# diff in the re-extract output, which surfaces in commit history.
#
# Override: setting ANTLR4_VERSION in the environment skips the
# Maven-Central lookup. Useful when offline or to reproduce an older
# extract.

CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/gale"
mkdir -p "$CACHE_DIR"

resolve_latest_version() {
    # Hit Maven Central's REST endpoint. Returns the latest version
    # number (e.g. "4.13.2") on stdout. Cached for ~24h to avoid
    # hammering the API on repeated extracts.
    local cache="$CACHE_DIR/antlr4-latest-version"
    if [ -f "$cache" ] && [ $(($(date +%s) - $(stat -c %Y "$cache" 2>/dev/null || stat -f %m "$cache" 2>/dev/null || echo 0))) -lt 86400 ]; then
        cat "$cache"
        return 0
    fi
    local url="https://search.maven.org/solrsearch/select?q=g:%22org.antlr%22+AND+a:%22antlr4%22&rows=1&wt=json"
    local body
    if command -v curl >/dev/null 2>&1; then
        body=$(curl -fsSL "$url") || return 1
    elif command -v wget >/dev/null 2>&1; then
        body=$(wget -q -O - "$url") || return 1
    else
        return 1
    fi
    # Extract `"latestVersion":"X.Y.Z"` without requiring jq.
    local version
    version=$(printf '%s' "$body" | sed -n 's/.*"latestVersion":"\([^"]*\)".*/\1/p')
    if [ -z "$version" ]; then
        return 1
    fi
    printf '%s' "$version" > "$cache"
    printf '%s' "$version"
}

# Verify a freshly downloaded ANTLR4 jar against the SHA-1 checksum
# published on Maven Central for the same version. The jar binary is
# fetched from antlr.org; the checksum comes from an independent host
# (repo1.maven.org), so a corrupted or tampered download fails the
# comparison. The version is resolved dynamically (see the note above),
# so a single pinned hash is not viable — we fetch the publisher's own
# digest instead. Best-effort: if the checksum cannot be fetched (e.g. a
# transient Maven Central outage) or no SHA-1 tool is on PATH, warn and
# accept the download rather than breaking the oracle entirely. Returns
# non-zero only on a definite mismatch.
verify_jar_checksum() {
    local jar="$1" version="$2"
    local sha_url="https://repo1.maven.org/maven2/org/antlr/antlr4/${version}/antlr4-${version}-complete.jar.sha1"

    local sha_tool
    if command -v sha1sum >/dev/null 2>&1; then
        sha_tool="sha1sum"
    elif command -v shasum >/dev/null 2>&1; then
        sha_tool="shasum -a 1"
    else
        echo "oracle: no sha1 tool (sha1sum/shasum) on PATH; skipping checksum verification" >&2
        return 0
    fi

    local expected=""
    if command -v curl >/dev/null 2>&1; then
        expected=$(curl -fsSL "$sha_url" 2>/dev/null) || expected=""
    elif command -v wget >/dev/null 2>&1; then
        expected=$(wget -q -O - "$sha_url" 2>/dev/null) || expected=""
    fi
    # Maven Central .sha1 files hold the bare hex digest; take the first
    # whitespace-delimited field and lowercase it for a stable compare.
    expected=$(printf '%s' "$expected" | awk '{print $1}' | tr 'A-Z' 'a-z')
    if [ -z "$expected" ]; then
        echo "oracle: could not fetch published SHA-1 for $version; skipping checksum verification" >&2
        return 0
    fi

    local actual
    actual=$($sha_tool "$jar" | awk '{print $1}' | tr 'A-Z' 'a-z')
    if [ "$actual" != "$expected" ]; then
        echo "oracle: SHA-1 mismatch for downloaded jar (expected $expected, got $actual)" >&2
        return 1
    fi
    echo "oracle: verified SHA-1 $actual" >&2
    return 0
}

ANTLR4_VERSION="${ANTLR4_VERSION:-}"
if [ -z "$ANTLR4_VERSION" ]; then
    if ! ANTLR4_VERSION=$(resolve_latest_version); then
        echo "oracle: cannot resolve latest ANTLR4 version from Maven Central" >&2
        echo "oracle: set ANTLR4_VERSION in the environment to pin a known version" >&2
        exit 1
    fi
fi
ANTLR4_URL="https://www.antlr.org/download/antlr-${ANTLR4_VERSION}-complete.jar"
JAR_PATH="$CACHE_DIR/antlr-${ANTLR4_VERSION}-complete.jar"
# Record the version actually used (pinned or resolved) for the wrapper's
# Phase 3 stamping. The latest-version cache is NOT suitable for this: it
# is bypassed when ANTLR4_VERSION is pinned and goes stale across runs.
printf '%s' "$ANTLR4_VERSION" > "$CACHE_DIR/antlr4-resolved-version"

usage() {
    cat >&2 <<EOF
Usage: $(basename "$0") [options] <grammar.g4> <start_rule> < input
       $(basename "$0") [options] --tokens <grammar.g4> < input

Options:
  --tokens          print the token stream instead of the parse tree
  --super <file>    compile <file> alongside the recognizer (repeatable)
  --probe-super     report what the input does against a synthesized base
                    class; diagnostic only, always exits 3
EOF
    exit 1
}

MODE="tree"
PROBE_SUPER=0
SUPER_SRCS=()
# Counted separately: `${#arr[@]}` on an empty array trips `set -u` on the
# bash 3.2 still shipped as /bin/bash on macOS.
SUPER_N=0
while [ $# -gt 0 ]; do
    case "$1" in
        --tokens) MODE="tokens"; shift ;;
        --probe-super) PROBE_SUPER=1; shift ;;
        --super) [ $# -ge 2 ] || usage; SUPER_SRCS[$SUPER_N]="$2"; SUPER_N=$((SUPER_N + 1)); shift 2 ;;
        --super=*) SUPER_SRCS[$SUPER_N]="${1#--super=}"; SUPER_N=$((SUPER_N + 1)); shift ;;
        --) shift; break ;;
        -*) echo "oracle: unknown option: $1" >&2; usage ;;
        *) break ;;
    esac
done

if [ "$PROBE_SUPER" = "1" ] && [ "$SUPER_N" -gt 0 ]; then
    echo "oracle: --probe-super and --super are mutually exclusive" >&2
    exit 1
fi

if [ "$MODE" = "tree" ]; then
    [ $# -eq 2 ] || usage
    GRAMMAR_PATH="$1"
    START_RULE="$2"
else
    [ $# -eq 1 ] || usage
    GRAMMAR_PATH="$1"
    START_RULE="tokens"  # TestRig convention for lexer grammars
fi

if [ ! -f "$GRAMMAR_PATH" ]; then
    echo "oracle: cannot find grammar file: $GRAMMAR_PATH" >&2
    exit 1
fi

if [ "$SUPER_N" -gt 0 ]; then
    for src in "${SUPER_SRCS[@]}"; do
        if [ ! -f "$src" ]; then
            echo "oracle: cannot find --super source: $src" >&2
            exit 1
        fi
    done
fi

# ANTLR4 requires the source file name to match the declared
# `grammar Name;` identifier, so use that rather than the caller's
# basename (descriptor-derived inputs often disagree). `sed -n .. p`
# prints only on a successful substitution: the legal layout with the
# `;` on a later line (`grammar Foo\n;`) must extract `Foo`, not pass
# the raw line through as the "name".
declared_name=$(grep -E '^[[:space:]]*(lexer[[:space:]]+grammar|parser[[:space:]]+grammar|grammar)[[:space:]]+[A-Za-z_]' "$GRAMMAR_PATH" 2>/dev/null \
    | head -1 \
    | sed -nE 's/^[[:space:]]*(lexer[[:space:]]+grammar|parser[[:space:]]+grammar|grammar)[[:space:]]+([A-Za-z_][A-Za-z0-9_]*).*/\2/p')
if [ -n "$declared_name" ]; then
    GRAMMAR_NAME="$declared_name"
else
    GRAMMAR_NAME="$(basename "$GRAMMAR_PATH" .g4)"
fi

# The generated recognizer will `extends X`, so X has to be on the compile
# path. Most grammars declare none, and no match must not trip `pipefail`.
SUPER_CLASS=$(tr '\n' ' ' < "$GRAMMAR_PATH" \
    | { grep -oE 'superClass[[:space:]]*=[[:space:]]*[A-Za-z_][A-Za-z0-9_]*' || true; } \
    | head -1 \
    | sed -E 's/.*=[[:space:]]*//')

if [ -n "$SUPER_CLASS" ] && [ "$PROBE_SUPER" != "1" ] && [ "$SUPER_N" -eq 0 ]; then
    cat >&2 <<EOF
oracle: $GRAMMAR_NAME declares 'options { superClass = $SUPER_CLASS; }' but no
oracle: base class was supplied, so the generated recognizer has nothing to
oracle: extend and javac would fail. Either:
oracle:   --super <path/to/$SUPER_CLASS.java>  the real base class — the oracle
oracle:       then runs the specification the Gale side models, so a diff is
oracle:       a Gale bug and not two different grammars disagreeing
oracle:   --probe-super  report what the input does against a synthesized
oracle:       base — a diagnostic to size that work, never an oracle
EOF
    exit 1
fi
if [ "$PROBE_SUPER" = "1" ] && [ -z "$SUPER_CLASS" ]; then
    echo "oracle: --probe-super given but $GRAMMAR_NAME declares no superClass" >&2
    exit 1
fi

if [ ! -f "$JAR_PATH" ]; then
    echo "oracle: downloading $ANTLR4_URL → $JAR_PATH" >&2
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL -o "$JAR_PATH.tmp" "$ANTLR4_URL"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$JAR_PATH.tmp" "$ANTLR4_URL"
    else
        echo "oracle: neither curl nor wget is available" >&2
        exit 1
    fi
    if ! verify_jar_checksum "$JAR_PATH.tmp" "$ANTLR4_VERSION"; then
        rm -f "$JAR_PATH.tmp"
        exit 1
    fi
    mv "$JAR_PATH.tmp" "$JAR_PATH"
fi

if ! command -v java >/dev/null 2>&1; then
    echo "oracle: 'java' not on PATH" >&2
    exit 1
fi
if ! command -v javac >/dev/null 2>&1; then
    echo "oracle: 'javac' not on PATH (need JDK, not just JRE)" >&2
    exit 1
fi

# Java type for a literal appearing at a `this.<name>(...)` call site. Only
# literals occur in the corpus; anything else stays Object and surfaces as a
# javac error rather than a silently wrong signature.
probe_param_type() {
    case "$1" in
        \"*) echo "String" ;;
        \'*) echo "char" ;;
        true|false) echo "boolean" ;;
        -[0-9]*|[0-9]*) echo "int" ;;
        *) echo "Object" ;;
    esac
}

# Two call sites differing only in their literals (`n("get")`, `n("set")`)
# collapse to one signature — emitting both would be a duplicate method.
probe_signature() {
    local sig="$1"
    local name args params i part parts
    name=${sig#this.}
    name=${name%%(*}
    args=${sig#*(}
    args=${args%)}
    args=$(printf '%s' "$args" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
    params=""
    i=0
    if [ -n "$args" ]; then
        IFS=',' read -ra parts <<< "$args"
        for part in "${parts[@]}"; do
            part=$(printf '%s' "$part" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
            i=$((i + 1))
            params="${params}${params:+, }$(probe_param_type "$part") a$i"
        done
    fi
    printf '%s|%s\n' "$name" "$params"
}

# Predicates and actions alike record to -Dgale.probe.log: a name used in
# both roles is stubbed boolean, so without that its action side would run
# unrecorded.
emit_probe_super() {
    local grammar="$1" out="$2"
    local flat names calls pred_names super_type ctor_param name
    flat=$(tr '\n' ' ' < "$grammar")
    names=$(printf '%s' "$flat" | grep -oE 'this\.[A-Za-z_][A-Za-z0-9_]*\(' | sort -u || true)
    calls=$(printf '%s' "$flat" | grep -oE 'this\.[A-Za-z_][A-Za-z0-9_]*\([^()]*\)' | sort -u || true)
    # A `{ ... }?` body with no nested braces — a predicate whose Java spans
    # braces is missed and lands among the actions, where javac rejects it as
    # a void expression. Visibly wrong beats silently boolean.
    pred_names=$(printf '%s' "$flat" | grep -oE '\{[^{}]*\}[[:space:]]*\?' \
        | grep -oE 'this\.[A-Za-z_][A-Za-z0-9_]*\(' | sort -u || true)

    # `calls` only matches an argument list with no nested parens, so a call
    # like `this.f(getText())` yields a name with no signature. Say so here
    # rather than let javac report a missing symbol.
    while IFS= read -r name; do
        [ -n "$name" ] || continue
        if ! printf '%s\n' "$calls" | grep -q "^${name//./\\.}"; then
            echo "oracle: cannot infer a signature for ${name}...) — its argument" >&2
            echo "oracle: list is not a plain literal. Supply the base class with --super." >&2
            return 1
        fi
    done <<< "$names"

    if grep -qE '^[[:space:]]*lexer[[:space:]]+grammar' "$grammar"; then
        super_type="Lexer"; ctor_param="CharStream"
    else
        super_type="Parser"; ctor_param="TokenStream"
    fi

    {
        echo "// Generated by antlr4-oracle.sh --probe-super. NOT a port of any real"
        echo "// base class: predicates answer a constant and actions do nothing."
        echo "import org.antlr.v4.runtime.*;"
        echo
        echo "public abstract class $SUPER_CLASS extends $super_type {"
        echo "    private static final boolean PRED ="
        echo "        Boolean.parseBoolean(System.getProperty(\"gale.probe.predicate\", \"true\"));"
        echo
        echo "    public $SUPER_CLASS($ctor_param input) { super(input); }"
        echo
        echo "    private static void mark(String name) {"
        echo "        String path = System.getProperty(\"gale.probe.log\");"
        echo "        if (path == null) return;"
        echo "        try (java.io.Writer w = new java.io.FileWriter(path, true)) {"
        echo "            w.write(name);"
        echo "            w.write('\\n');"
        echo "        } catch (java.io.IOException e) {"
        echo "            throw new RuntimeException(e);"
        echo "        }"
        echo "    }"

        local entry params
        while IFS= read -r entry; do
            [ -n "$entry" ] || continue
            name=${entry%%|*}
            params=${entry#*|}
            echo
            if printf '%s\n' "$pred_names" | grep -qx "this\.$name("; then
                echo "    public boolean $name($params) { mark(\"$name\"); return PRED; }"
            else
                echo "    public void $name($params) { mark(\"$name\"); }"
            fi
        done <<< "$(while IFS= read -r sig; do
            [ -n "$sig" ] && probe_signature "$sig"
        done <<< "$calls" | sort -u)"

        echo "}"
    } > "$out"
}

WORK_DIR="$(mktemp -d -t gale-antlr4-oracle.XXXXXX)"
if [ "${ORACLE_KEEP:-0}" != "1" ]; then
    trap 'rm -rf "$WORK_DIR"' EXIT
else
    echo "oracle: keeping work dir: $WORK_DIR" >&2
fi

cp "$GRAMMAR_PATH" "$WORK_DIR/$GRAMMAR_NAME.g4"

# --probe-super replays the input under both predicate polarities, and a
# pipe can only be read once.
cat > "$WORK_DIR/input.txt"

if [ "$SUPER_N" -gt 0 ]; then
    cp "${SUPER_SRCS[@]}" "$WORK_DIR/"
elif [ "$PROBE_SUPER" = "1" ]; then
    emit_probe_super "$GRAMMAR_PATH" "$WORK_DIR/$SUPER_CLASS.java"
fi

# Generate Java sources. -no-listener avoids emitting Listener
# scaffolding we don't need.
if ! java -jar "$JAR_PATH" -Dlanguage=Java -no-listener -o "$WORK_DIR" "$WORK_DIR/$GRAMMAR_NAME.g4" >"$WORK_DIR/antlr.log" 2>&1; then
    echo "oracle: antlr4 codegen failed; see $WORK_DIR/antlr.log" >&2
    cat "$WORK_DIR/antlr.log" >&2
    exit 1
fi

# Compile the generated sources.
if ! (cd "$WORK_DIR" && javac -cp "$JAR_PATH" *.java) >"$WORK_DIR/javac.log" 2>&1; then
    echo "oracle: javac failed; see $WORK_DIR/javac.log" >&2
    cat "$WORK_DIR/javac.log" >&2
    exit 1
fi

# Run TestRig. -tree prints the parse tree as an S-expression;
# -tokens prints the token stream. We capture stderr separately so
# parse errors flagged by ANTLR4's recognizer surface as exit-2 below.
TESTRIG_FLAG="-tree"
[ "$MODE" = "tokens" ] && TESTRIG_FLAG="-tokens"

run_testrig() {
    local outf="$1" errf="$2"
    shift 2
    set +e
    java ${1+"$@"} -cp "$JAR_PATH:$WORK_DIR" org.antlr.v4.gui.TestRig \
        "$GRAMMAR_NAME" "$START_RULE" "$TESTRIG_FLAG" \
        <"$WORK_DIR/input.txt" >"$outf" 2>"$errf"
    local rc=$?
    set -e
    return $rc
}

# ANTLR4's ConsoleErrorListener reports recognizer errors as
# `line <l>:<c> <message>`; everything else on stderr is JVM or tool
# chatter that must not be read as a parse failure.
recognizer_errors() {
    grep -E '^line [0-9]+:[0-9]+ ' "$1" || true
}

if [ "$PROBE_SUPER" = "1" ]; then
    # The stub has only the members the grammar names, so not even
    # "reached nothing, both polarities agree" certifies anything.
    : > "$WORK_DIR/reached.txt"
    PROBE_LOG="-Dgale.probe.log=$WORK_DIR/reached.txt"
    run_testrig "$WORK_DIR/out.true" "$WORK_DIR/err.true" \
        -Dgale.probe.predicate=true "$PROBE_LOG" || true
    run_testrig "$WORK_DIR/out.false" "$WORK_DIR/err.false" \
        -Dgale.probe.predicate=false "$PROBE_LOG" || true

    echo "oracle: --probe-super report for $GRAMMAR_NAME against a synthesized $SUPER_CLASS." >&2
    echo "oracle: NOT an oracle — $SUPER_CLASS may also override inherited methods" >&2
    echo "oracle: the grammar never names, which this stub does not have." >&2
    if [ -s "$WORK_DIR/reached.txt" ]; then
        echo "oracle: base-class members this input reached:" >&2
        sort -u "$WORK_DIR/reached.txt" | sed 's/^/oracle:   /' >&2
    else
        echo "oracle: base-class members this input reached: none" >&2
    fi
    if cmp -s "$WORK_DIR/out.true" "$WORK_DIR/out.false"; then
        echo "oracle: same answer under both predicate polarities:" >&2
        sed 's/^/oracle:   /' "$WORK_DIR/out.true" >&2
    else
        echo "oracle: with every predicate true:" >&2
        sed 's/^/oracle:   /' "$WORK_DIR/out.true" >&2
        echo "oracle: with every predicate false:" >&2
        sed 's/^/oracle:   /' "$WORK_DIR/out.false" >&2
    fi
    echo "oracle: to pin any of this, write $SUPER_CLASS.java and pass --super." >&2
    exit 3
fi

if run_testrig "$WORK_DIR/out.txt" "$WORK_DIR/err.txt"; then RC=0; else RC=$?; fi

if [ -s "$WORK_DIR/err.txt" ]; then
    cat "$WORK_DIR/err.txt" >&2
fi

cat "$WORK_DIR/out.txt"

if [ $RC -ne 0 ]; then
    exit 1
fi
# Only a recognizer error means the input failed to parse (mismatched
# input, no viable alternative, token recognition error, extraneous /
# missing input, …). Other stderr output — JVM warnings, ANTLR4 advisory
# notices — is benign and must NOT be classified as a parse error, or the
# descriptor is wrongly dropped from Stage B′.
if [ -n "$(recognizer_errors "$WORK_DIR/err.txt")" ]; then
    exit 2
fi
exit 0
