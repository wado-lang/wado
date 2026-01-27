# Effect System Design

Status: Draft

## Context

Wado tracks side effects through an effect system. This WEP defines the syntax and semantics for effect declarations, effect checking, and effect handlers.

## Decision

### Effect Declaration

Effects must be explicitly declared on functions. No inference.

```wado
fn greet(name: String) with Stdout {
    println(`Hello, {name}!`);
}

fn pure_add(a: i32, b: i32) -> i32 {
    return a + b;  // no effects
}
```

Multiple effects use comma separation:

```wado
fn process() with Stdout, Stderr, FileSystem {
    // ...
}
```

### Effect Checking

Calling a function requires its effects. Violations are compile errors.

```wado
fn caller() with Stdout {
    greet("Alice");  // OK: caller has Stdout
}

fn bad() {
    greet("Bob");  // ERROR: missing Stdout effect
}
```

### Generic Effects

For callbacks and higher-order functions, use generic effect parameters:

```wado
fn wrapper<E>(f: fn() with E) with E {
    f();
}

fn map<T, U, E>(arr: Array<T>, f: fn(T) -> U with E) -> Array<U> with E {
    // ...
}
```

Effects are types. No bounds needed.

### Closure Types

Closures require explicit effect annotation:

```wado
let f: fn(i32) -> i32 with Stdout = |x| {
    println(`{x}`);
    return x;
};
```

### Test Functions

Test functions implicitly have generic effects:

```wado
// Equivalent to: test<E> "name" with E { ... }
test "can use any effect" {
    println("stdout");
    eprintln("stderr");
}
```

### Non-Effects

`panic` and `unreachable` are not effects. They have return type `!` (never).

```wado
fn safe_div(a: i32, b: i32) -> i32 {
    if b == 0 { panic("division by zero"); }
    return a / b;
}
```

### Handlers

#### DI-Style (Named Handler)

```wado
handler MockStdin for Stdin {
    fn read_line() -> String {
        resume "mocked input"
    }
}

fn test_input() {
    with Stdin = MockStdin {
        let line = Stdin::read_line();
        assert line == "mocked input";
    }
}
```

Multiple handlers:

```wado
with Stdin = MockStdin, Stdout = MockStdout {
    // ...
}
```

#### Inline Handler

Uses the same method definition style as named handlers:

```wado
with Stdin as {
    fn read_line() -> String {
        resume "simple mock"
    }
} do {
    let line = Stdin::read_line();
}
```

With arguments and complex logic:

```wado
with FileSystem as {
    fn read_file(path: String) -> String {
        resume `contents of {path}`
    }
    fn write_file(path: String, data: String) {
        log.push([path, data]);
        resume;
    }
} do {
    let content = FileSystem::read_file("test.txt");
}
```

#### Mixed

```wado
with Stdin = MockStdin, Stdout as {
    fn write(s: String) {
        captured.push(s);
        resume;
    }
} do {
    // ...
}
```

### Resume Keyword

Use `resume` to return a value to the computation:

```wado
with Stdin as {
    fn read_line() -> String {
        resume "value"
    }
} do { ... }
```

For post-processing (one-shot continuations):

```wado
with FileSystem as {
    fn open_file(path: String) -> Handle {
        let handle = real_open(path);
        resume handle;
        real_close(handle);  // runs after computation uses handle
    }
} do { ... }
```

### Continuation Semantics

One-shot only. Each `resume` executes at most once. Multi-shot continuations are a future consideration pending Wasm Stack Switching support.

### Relation to `stores`

The `stores` annotation shares syntax with effects:

```wado
fn register(data: &Data) -> Handle with Stdout, stores[data] {
    // ...
}
```

## Consequences

- All function effects are explicit and checked at compile time
- Effect violations produce clear compile errors
- Handlers enable testing and dependency injection
- One-shot semantics ensure resource safety
- Generic effects support higher-order functions without effect polymorphism complexity
