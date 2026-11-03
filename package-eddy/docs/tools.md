# Eddy's Tools

The tools a reviewer agent reads and checks a change with. A checked item is
built; an unchecked one is wanted. The reasons come from the evaluation set in
[`eval/`](../eval/README.md).

Each tool sees the repository as it stood once the change merged, and nothing
later. In the evaluation that tree is a `git worktree` of the merge, so no
answer leaks from the fix that came after.

## Reading

They are in `src/tools.wado`. They take the names and inputs of Claude Code's
tools, because current models are trained on those. Paths are relative to the
repository root.

- [x] Read: a file, numbered as `cat -n` prints it, 2000 lines from `offset`.
- [x] Grep: ripgrep, with Claude Code's Grep parameters.
- [x] Glob: file paths matching a glob, sorted by path. ripgrep lists them.

Reading finds more of the evaluation set's defects, most where the defect needs
something outside the diff, such as the names another program writes. It can
also talk the reviewer out of a right finding: on one entry, every run traced
the code at length and then dropped the defect a single pass had named.

## History

- [ ] The pull request's own description, given with its diff. Today the
  reviewer gets only the title, so it does not know what the change meant to
  do.
- [ ] Log, blame and pickaxe (`git log -S`), up to the merge and no further.
  They tell whether the change wrote a line or only moved it, and why a line
  was written as it was. The history check of the evaluation set rested on
  them. The evaluation's worktree cannot serve them as it is: it shares the
  repository's objects, the later commits included.

## Language Services

They are in `src/symbols.wado`. They take a name, as Grep does, but find one
item's uses where Grep finds every match of the text. They read SCIP indexes of
the tree, which `rust-analyzer scip` writes for the Rust code and
`wado query scip` for the Wado code, and one reader serves both. The Wado index
leaves out the e2e fixtures (`wado-compiler/tests/**`). They are most of the
tree's Wado, and their stdlib uses would bury the ones a reviewer asks about.

- [x] FindSymbol: each definition of `name` or `Owner::name`, with its lines
  and its signature.
- [x] FindReferences: each line that uses it, with the declaration the line
  sits in.
- [ ] Index the Wado code with the toolchain built at the merge's revision.
  The evaluation indexes it with today's `wado`, which reads an old tree
  through today's standard library and language.
- [ ] Hover and diagnostics, which an index does not carry.

## Running

This also needs the toolchain built at the merge's revision.

- [ ] Run a check: compile and run a small Wado program or fixture
  (`wado run`, `wado test`), print the compiler's internal stages
  (`wado dump`), or run a chosen Rust test. A finding it reproduces becomes
  evidence, and one the code refutes is dropped on what the code did, not on a
  chain of reasoning. The entry where reading dropped a right finding could
  have been settled by running it once.
