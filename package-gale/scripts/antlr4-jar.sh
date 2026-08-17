#!/usr/bin/env bash
# Shared by the ANTLR4 oracle scripts: resolve, download and verify the
# published jar, then check that a JDK is on PATH. Source it, then call
# `ensure_antlr4_jar`; it sets ANTLR4_VERSION and JAR_PATH for the caller.
#
# We deliberately do NOT pin a specific ANTLR4 jar version. Each extract
# resolves the current latest release from Maven Central and caches it
# locally; the resolved version is written to
# `$CACHE_DIR/antlr4-resolved-version` on every run so the descriptor
# extractor (a separate process — an `export` here could never reach it) can
# stamp it into the generated test files. Reproducibility is preserved via
# that comment in the committed test file: any drift in the oracle's answer
# (caused by an ANTLR4 patch release) surfaces as a diff in the re-extract
# output, which surfaces in commit history.
#
# Override: setting ANTLR4_VERSION in the environment skips the Maven-Central
# lookup. Useful when offline or to reproduce an older extract.

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

# Sets ANTLR4_VERSION and JAR_PATH, downloading the jar on first use.
ensure_antlr4_jar() {
    ANTLR4_VERSION="${ANTLR4_VERSION:-}"
    if [ -z "$ANTLR4_VERSION" ]; then
        if ! ANTLR4_VERSION=$(resolve_latest_version); then
            echo "oracle: cannot resolve latest ANTLR4 version from Maven Central" >&2
            echo "oracle: set ANTLR4_VERSION in the environment to pin a known version" >&2
            exit 1
        fi
    fi
    local url="https://www.antlr.org/download/antlr-${ANTLR4_VERSION}-complete.jar"
    JAR_PATH="$CACHE_DIR/antlr-${ANTLR4_VERSION}-complete.jar"
    # Record the version actually used (pinned or resolved) for the wrapper's
    # Phase 3 stamping. The latest-version cache is NOT suitable for this: it
    # is bypassed when ANTLR4_VERSION is pinned and goes stale across runs.
    printf '%s' "$ANTLR4_VERSION" > "$CACHE_DIR/antlr4-resolved-version"

    if [ ! -f "$JAR_PATH" ]; then
        echo "oracle: downloading $url → $JAR_PATH" >&2
        if command -v curl >/dev/null 2>&1; then
            curl -fsSL -o "$JAR_PATH.tmp" "$url"
        elif command -v wget >/dev/null 2>&1; then
            wget -q -O "$JAR_PATH.tmp" "$url"
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
}
