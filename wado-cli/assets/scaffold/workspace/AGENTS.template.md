# {name}

- [ ] Say in one paragraph what the workspace holds.

Each member of the workspace is a package. `wado new <dir>` creates one and
adds it to `members` in `wado-workspace.toml`, and `wado workspace add <dir>`
adds a package that is already there. A member inherits what
`[workspace.package]` sets, so its `wado.toml` does not repeat it. Each
package's files follow [the standard package layout]({layout_url}).
