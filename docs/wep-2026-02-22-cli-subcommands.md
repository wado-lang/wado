# WEP: CLI Subcommands for Package Management

## Context

[WEP: Package Manifest (`wado.toml`)](./wep-2026-02-14-package-manifest.md) defines the manifest format, dependency resolution, and lock file design. This WEP covers the CLI commands that operate on `wado.toml` and `wado.lock`.

## Decision

### Project Commands

```sh
wado init                          # create wado.toml interactively
wado add router --git https://github.com/user/router.git --version "^1.0.0"
wado add regex --package docs:regex --version "^0.1.0"
wado remove router
wado update                        # update wado.lock
wado update regex                  # update specific dependency
```

These are future CLI commands. The initial implementation focuses on `wado.toml` parsing and module resolution.

### Entry Point and CLI Commands

When `wado.toml` is present, the existing CLI commands use the entry point fields:

```sh
# Without wado.toml (single-file mode, unchanged)
wado run file.wado
wado serve file.wado

# With wado.toml (entry point auto-discovered)
wado run                           # uses [package].command
wado serve                         # uses [package].service
wado compile -o out.wasm           # compiles the command entry point
wado compile --lib -o out.wasm     # compiles the lib entry point
```

When a file argument is provided, it overrides the entry point from `wado.toml`.

### `wado exec` for Dependency Entry Points

```sh
wado exec <dep-name>               # run dependency's command entry point
wado exec <dep-name> [args...]     # pass arguments to the dependency
```

`wado exec` looks up `<dep-name>` in `[dependencies]` and `[dev-dependencies]`, resolves the dependency (using `wado.lock` if present), and runs its `command` entry point. This enables tool packages (formatters, linters, generators) to be installed as dependencies and executed directly.

The lock file's `command` field for the dependency determines which source file to compile and run. If the dependency has no `command` entry point, `wado exec` reports an error. If the dependency is a dev-dependency and dev-dependencies have not been fetched, `wado exec` reports an error.

## Consequences

TBD — to be filled in as the CLI commands are designed in detail.
