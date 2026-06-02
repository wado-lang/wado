#!/bin/bash
# .claude/hooks/mise-setup.sh
# SessionStart hook for Claude Code Web to install mise and project tools
# cf. https://code.claude.com/docs/en/settings

set -e
LOG_PREFIX="[mise-setup]"

log() {
    echo "$LOG_PREFIX $1" >&2
}

# Only run in remote Claude Code environment
if [ "$CLAUDE_CODE_REMOTE" != "true" ]; then
    log "Not a remote session, skipping mise setup"
    exit 0
fi

log "Remote session detected, setting up mise..."

# Setup local bin directory
LOCAL_BIN="$HOME/.local/bin"
mkdir -p "$LOCAL_BIN"

# Fallback: mise.run has been observed to return 403 from some egress IPs.
# Grab the latest release tarball from GitHub directly.
install_mise_from_github() {
    local arch os
    case "$(uname -m)" in
        x86_64) arch="x64" ;;
        aarch64 | arm64) arch="arm64" ;;
        *) log "Unsupported arch for GitHub fallback: $(uname -m)"; return 1 ;;
    esac
    case "$(uname -s)" in
        Linux) os="linux" ;;
        Darwin) os="macos" ;;
        *) log "Unsupported OS for GitHub fallback: $(uname -s)"; return 1 ;;
    esac

    # Resolve the latest version by following the /releases/latest 302.
    # api.github.com is commonly rate-limited or blocked; this only hits github.com.
    local effective tag
    effective=$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
        https://github.com/jdx/mise/releases/latest 2>/dev/null) || {
        log "Could not resolve latest mise tag from GitHub"
        return 1
    }
    tag="${effective##*/}"
    case "$tag" in
        v[0-9]*) ;;
        *) log "Unexpected tag resolved from GitHub: '$tag'"; return 1 ;;
    esac

    local url="https://github.com/jdx/mise/releases/download/$tag/mise-$tag-$os-$arch.tar.gz"
    local tmp
    tmp=$(mktemp -d) || return 1

    log "Downloading $url ..."
    if ! curl -fsSL -o "$tmp/mise.tar.gz" "$url"; then
        log "Download failed from $url"
        rm -rf "$tmp"
        return 1
    fi
    if ! tar -xzf "$tmp/mise.tar.gz" -C "$tmp"; then
        log "Failed to extract mise tarball"
        rm -rf "$tmp"
        return 1
    fi
    if [ ! -f "$tmp/mise/bin/mise" ]; then
        log "mise binary not found at expected path in tarball"
        rm -rf "$tmp"
        return 1
    fi

    cp "$tmp/mise/bin/mise" "$LOCAL_BIN/mise"
    chmod +x "$LOCAL_BIN/mise"
    rm -rf "$tmp"
    return 0
}

# Check if mise is already available
if command -v mise &>/dev/null; then
    log "mise already available: $(mise --version)"
else
    log "Installing mise to $LOCAL_BIN..."

    if curl -fsSL https://mise.run | sh; then
        log "mise installed via mise.run: $($LOCAL_BIN/mise --version)"
    else
        log "mise.run installer failed; falling back to GitHub Releases"
        if ! install_mise_from_github; then
            log "Failed to install mise (both mise.run and GitHub fallback)"
            exit 1
        fi
        log "mise installed from GitHub: $($LOCAL_BIN/mise --version)"
    fi
fi

# Persist PATH and mise settings to CLAUDE_ENV_FILE
if [ -n "$CLAUDE_ENV_FILE" ]; then
    cat >> "$CLAUDE_ENV_FILE" << 'EOF'
export PATH="$HOME/.local/bin:$PATH"
export MISE_YES=true
export MISE_TRUSTED_CONFIG_PATHS="$HOME:$PWD"
eval "$(mise activate bash)"
EOF
    log "Environment persisted to CLAUDE_ENV_FILE"
fi

# Ensure mise is in PATH for the rest of this script
export PATH="$LOCAL_BIN:$PATH"

# Trust the mise configuration and install project tools
log "Trusting mise configuration..."
mise trust --all 2>/dev/null || true

log "Installing project tools..."
if mise install; then
    log "Project tools installed successfully"
else
    log "Warning: Some tools may have failed to install"
fi

# Sync the wasmtime submodule to the commit pinned in the superproject.
# wado-compiler has a path dependency on vendor/wasmtime/crates/wasmtime
# (see [workspace.dependencies] in Cargo.toml), so any cargo build/test/
# benchmark fails without it. Only this submodule is needed for builds; the
# rest of vendor/ holds reference specs that can be initialized on demand.
# --recommend-shallow keeps the checkout small.
#
# We run this unconditionally (not just when the checkout is absent): the
# remote container provisions submodules at the fork's branch tip rather than
# the pinned gitlink SHA, and since wado-lang/wasmtime's main tracks upstream,
# a populated-but-stale checkout drifts away from the pin each session. A bare
# `git submodule update` is idempotent when already in sync and reconciles the
# checkout back to the pinned SHA otherwise; --force resets it even if the
# container left the working tree pointing elsewhere.
if [ -f .gitmodules ]; then
    log "Syncing vendor/wasmtime submodule to the pinned commit..."
    if git submodule update --init --force --recommend-shallow vendor/wasmtime; then
        log "vendor/wasmtime synced to pinned commit"
    else
        log "Warning: failed to sync vendor/wasmtime submodule"
    fi
fi

log "mise setup complete"
exit 0
