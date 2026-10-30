# Wadopoet Development Guide

[README.md](./README.md) is the API. `src/vocabulary.wado` is generated from
the compiler: regenerate it with `mise run update-wadopoet-vocabulary`.

## Round Trips

`src/wadopoet_test.wado` has a `printed` helper. It wraps a body in a `run()`
that may call `print`, evaluates it, asserts it exited with 0, and returns
what it printed. A round trip reads like this:

```wado
test "a hole in a string literal reads back as its value" {
    let values: List<String> = ["", "a\"\\b", "\n\u{1}", "あ"];
    let mut run = String::new();
    let mut expected = String::new();
    for let v of values {
        run.push_str(wado`print("${v}");`);
        expected.push_str(&v);
    }
    assert printed(run) == expected;
}
```

When a body holds a template, write the body as a `"..."` literal. Written as a
template of the test's own, its `${...}` would be filled in by the test.
