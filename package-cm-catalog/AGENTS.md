# cm-catalog Development Guide

cm-catalog enumerates the Component Model ABI surface as Wado `export`
functions, one shape each. The `wado-package-standard` skill says where each
kind of file goes. [`README.md`](./README.md) says what it covers, and
[`TODO.md`](./TODO.md) what it does not yet.

## Regenerating the WIT

`cm_catalog_matches_committed_wit` in `wado-compiler/tests/integration/wit.rs`
is the generator of record for `assets/cm-catalog.wit`: it re-emits the
interface from the source, asserts it matches the file, and prints the emitted
text on mismatch. Edit the source, run the test, and take what it prints, so
the artifact cannot drift from the source.

```sh
cd package-cm-catalog && wado wit --lib
```

emits the same library world, but with the imports the compiled component makes
(`wasi:cli/stderr`), so it reads the exports back without producing the file.
