#!/bin/bash
# .claude/hooks/mise-setup.sh
# SessionStart hook for Claude Code Web to install mise and project tools

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

# Check if mise is already available
if command -v mise &>/dev/null; then
    log "mise already available: $(mise --version)"
else
    log "Installing mise to $LOCAL_BIN..."

    if ! curl -fsSL https://mise.run | sh; then
        log "Failed to install mise"
        exit 1
    fi

    log "mise installed: $($LOCAL_BIN/mise --version)"
fi

# Persist PATH and mise settings to CLAUDE_ENV_FILE
if [ -n "$CLAUDE_ENV_FILE" ]; then
    cat >> "$CLAUDE_ENV_FILE" << 'EOF'
export PATH="$HOME/.local/share/mise/shims:$HOME/.local/bin:$PATH"
export MISE_YES=true
export MISE_TRUSTED_CONFIG_PATHS="$HOME:$PWD"
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

log "mise setup complete"
exit 0
