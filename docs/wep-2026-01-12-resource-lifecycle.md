# WEP: Resource Lifecycle Management (RAII)

## Context

Wado targets Wasm GC, which provides automatic memory management through tracing garbage collection. However, many resources require **deterministic cleanup** beyond memory management:

- File handles must be closed to flush buffers and release OS resources
- Network sockets must be closed to free ports and terminate connections
- Database connections must be released to connection pools
- Locks must be released to prevent deadlocks
- GPU buffers must be freed to prevent resource exhaustion

### The GC Problem

With tracing GC, finalizers run at unpredictable times:

```wado
fn problematic() with FileSystem {
    let file = FileSystem::open("data.txt");
    file.write("hello");
    // When does file get closed? Unknown!
    // Buffer might not be flushed
    // File handle might leak
}
```

Unlike reference-counted languages (Perl, Python with CPython, Swift), where destructors run immediately when the last reference is dropped, tracing GC languages cannot guarantee when—or if—a finalizer will run.

### The Compositional Problem

Even if we solve cleanup for individual resources, we need **compositional cleanup** for structs containing resources:

```wado
struct Connection {
    socket: Socket,        // Needs close()
    buffer: Array<u8>,     // GC is fine
}

// When Connection is dropped, socket must be closed automatically
```

Manual cleanup is error-prone and violates DRY:

```wado
impl Connection {
    fn cleanup(&mut self) {
        self.socket.close();
        // Must remember to update this when adding new resource fields!
    }
}
```

### Survey of Other Languages

| Language   | Memory Management             | Resource Management                    | Notes                            |
| ---------- | ----------------------------- | -------------------------------------- | -------------------------------- |
| **Rust**   | Ownership (no GC)             | `Drop` trait                           | Deterministic, compositional     |
| **Swift**  | ARC (ref counting)            | `deinit`                               | Deterministic, compositional     |
| **C#**     | Tracing GC                    | `IDisposable` + `using`                | Manual, requires explicit syntax |
| **Java**   | Tracing GC                    | `AutoCloseable` + `try-with-resources` | Manual, requires explicit syntax |
| **Python** | Mixed (CPython: RC, PyPy: GC) | `__enter__`/`__exit__` + `with`        | Manual, requires explicit syntax |
| **Go**     | Tracing GC                    | `defer`                                | Manual, requires explicit syntax |
| **Zig**    | Manual                        | `defer`                                | Manual, requires explicit syntax |

**Key observation**: Languages with tracing GC require **explicit syntax** (`using`, `try-with-resources`, `defer`) for deterministic cleanup. None provide automatic compositional cleanup like Rust's `Drop`.

### Component Model's `resource` Type

The WebAssembly Component Model defines a `resource` type with built-in lifecycle management:

```wit
// WIT definition
resource file {
    static open: func(path: string) -> file;

    write: func(data: list<u8>) -> result<u32, error>;

    // Destructor - called when resource is dropped
    [destructor]
}
```

Key properties:

- **Owned handle**: Resource instances have unique ownership
- **Deterministic destruction**: Destructor is called when the handle is dropped
- **Component Model boundary**: Resources can't leak across components

This aligns perfectly with Wado's needs.

## Decision

### 1. `resource` Types Are Implicitly `unique`

Component Model `resource` types automatically have move-only semantics:

```wado
resource File {
    static fn open(path: String) -> File with FileSystem;

    fn write(&mut self, data: &Array<u8>) -> Result<u32, IoError> with FileSystem;

    fn [destructor]() with FileSystem;  // Called on drop
}

let file = File::open("data.txt");
let file2 = file;      // Error: cannot copy resource
let file2 = move file; // OK: explicit move
```

**Rationale**:

- Resources represent unique system objects (file handles, sockets)
- Copying would create aliasing problems (double-close, double-free)
- Move-only semantics prevent these bugs
- Consistent with Component Model semantics

### 2. Destructor Syntax: `fn [destructor]() with Effects`

The destructor is a special method with the `[destructor]` attribute:

```wado
resource Socket {
    static fn connect(addr: String) -> Socket with Network;

    fn read(&mut self, buf: &mut Array<u8>) -> i32 with Network;
    fn write(&self, data: &Array<u8>) -> i32 with Network;

    // Destructor can declare effects
    fn [destructor]() with Network;
}
```

**Syntax rules**:

- Must be named `[destructor]` (attribute syntax, not identifier)
- Must have signature `fn [destructor]() with Effects`
- No parameters, no return value
- Can declare effects (destructor may perform I/O)
- Implicitly takes `&mut self` (can access fields)

**Rationale**:

- `[destructor]` is clear and searchable
- Bracket syntax `[...]` distinguishes from regular methods
- Aligns with WIT's destructor notation
- Effects are necessary (closing a file requires `FileSystem` effect)

### 3. Compositional Cleanup: Structs with `resource` Fields

When a struct contains `resource` fields, it automatically becomes `unique` and gains a synthesized destructor:

```wado
resource Socket {
    fn [destructor]() with Network;
}

struct Connection {
    socket: Socket,        // resource field
    buffer: Array<u8>,     // normal field
}

// Connection is implicitly unique (has resource field)
// Compiler synthesizes destructor:
//
// fn [destructor]() with Network {
//     self.socket.[destructor]();  // Call resource destructor
//     // buffer is GC'd, no action needed
// }

let conn = Connection { socket: Socket::connect(...), buffer: [] };
// conn2 = conn;          // Error: Connection is unique (implicit)
let conn2 = move conn;    // OK: explicit move
```

**Synthesis rules**:

1. If struct has any `resource` fields, the struct becomes implicitly `unique`
2. Compiler generates a destructor that calls destructors of all `resource` fields in **declaration order**
3. Non-resource fields are ignored (GC handles them)
4. The synthesized destructor requires the **union of all resource field effects**

**Explicit `unique` annotation**:

You can make any struct `unique` even without resource fields:

```wado
unique struct CustomHandle {
    id: i32,
}

// No synthesized destructor (no resource fields)
// But still move-only
```

### 4. Destructor Execution Guarantees

Destructors are called deterministically in these situations:

| Situation     | When Destructor Runs    | Example                                     |
| ------------- | ----------------------- | ------------------------------------------- |
| Scope exit    | End of block            | `{ let f = File::open(...); }`              |
| Early return  | Before function returns | `if err { return; }`                        |
| Move          | Old binding invalidated | `let f2 = move f;` (f's destructor NOT run) |
| Explicit drop | `drop(f)` call          | `drop(f);`                                  |
| Panic         | Unwinding (TBD)         | `panic("error");`                           |

**Scope exit example**:

```wado
fn example() with FileSystem {
    let file = File::open("data.txt");
    file.write("hello");
    // file.[destructor]() called here automatically
}
```

**Early return example**:

```wado
fn example(path: String) -> Result<(), Error> with FileSystem {
    let file = File::open(path);

    if should_abort() {
        return Err(Error::Aborted);
        // file.[destructor]() called before return
    }

    file.write("data");
    // file.[destructor]() called at end of scope
}
```

**Move transfers ownership**:

```wado
fn example() with FileSystem {
    let file = File::open("data.txt");
    consume(move file);  // Ownership transferred
    // file's destructor NOT called here
    // consume() is responsible for cleanup
}

fn consume(f: File) with FileSystem {
    f.write("data");
    // f.[destructor]() called here
}
```

**Explicit drop**:

```wado
use {drop} from "core:memory";

fn example() with FileSystem {
    let file = File::open("data.txt");
    file.write("data");
    drop(file);  // Explicitly call destructor
    // file is now invalid

    // more work...
}
```

### 5. Panic and Unwinding (TBD)

**Current behavior**: If panic occurs while a resource is in scope, the destructor is **not guaranteed** to run (Wasm trapping doesn't unwind).

**Future consideration**: Once Wasm exception handling stabilizes, destructors could run during unwinding.

For now, resources should be designed to tolerate abrupt termination (e.g., file buffers should use write-through caching, or applications should use explicit error handling instead of panic).

### 6. Interaction with Value Semantics

This design integrates cleanly with the value semantics WEP:

| Struct Type           | Default Semantics | Implicit `unique`? | Destructor?               |
| --------------------- | ----------------- | ------------------ | ------------------------- |
| No `resource` fields  | Value (copyable)  | No                 | No                        |
| Has `resource` fields | Move-only         | Yes                | Synthesized               |
| Explicit `unique`     | Move-only         | Yes                | No (unless has resources) |

**Examples**:

```wado
// Regular struct - value semantics
struct Point {
    x: i32,
    y: i32,
}

let p1 = Point { x: 1, y: 2 };
let p2 = p1;  // Copy - p1 still valid

// Struct with resource - implicit unique
struct FileReader {
    file: File,  // resource
    line_num: i32,
}

let r1 = FileReader { file: File::open(...), line_num: 0 };
// let r2 = r1;        // Error: FileReader is unique
let r2 = move r1;      // OK: explicit move

// Explicit unique without resources
unique struct Token {
    value: String,
}

let t1 = Token { value: "secret" };
// let t2 = t1;        // Error: Token is unique
let t2 = move t1;      // OK: explicit move
```

### 7. Declaring Resources in Wado

**Option A: Import from WIT/Component**:

```wado
// Import resource from external component
use {File} from "wasi:filesystem" with {
    type: "wasm",
    wit: "./wasi-filesystem.wit",
};

// File is a resource with destructor defined in WIT
```

**Option B: Define in Wado**:

```wado
// Define resource in Wado
resource File {
    static fn open(path: String) -> File with FileSystem;

    fn write(&mut self, data: &Array<u8>) -> Result<u32, IoError> with FileSystem;
    fn read(&mut self, buf: &mut Array<u8>) -> Result<u32, IoError> with FileSystem;

    fn [destructor]() with FileSystem {
        // Implementation calls WASI close
        wasi_filesystem_close(self.handle);
    }
}
```

The compiler generates WIT with `[destructor]` annotation for Component Model export.

### 8. Resource Fields in Variants and Arrays

**Variants**: A variant with resource-containing cases becomes `unique`:

```wado
variant Stream {
    File(File),      // resource
    Network(Socket), // resource
    Memory(Array<u8>),
}

// Stream is implicitly unique
// Destructor calls destructor of the active case
```

**Arrays**: Arrays of resources are allowed:

```wado
let files: Array<File> = [];
files.push(File::open("a.txt"));
files.push(File::open("b.txt"));

// When files is dropped, all File destructors are called
```

The `Array<File>` type is itself `unique` (cannot copy an array of unique values).

### 9. Effect Requirements for Destructors

Destructors can declare effects, and these effects propagate to callers:

```wado
resource Database {
    fn [destructor]() with Network, Stdout {
        // Close connection and log
        close_connection(self.handle);
        println("Database connection closed");
    }
}

fn use_db() with Network, Stdout {
    let db = Database::connect();
    // ...
    // db.[destructor]() requires Network, Stdout
    // Caller must have these effects
}
```

**Effect propagation rule**: A function that owns a resource must declare the effects required by that resource's destructor.

This is automatically checked by the compiler.

## Examples

### Basic Resource

```wado
resource File {
    static fn open(path: String) -> Result<File, IoError> with FileSystem;

    fn write(&mut self, data: &Array<u8>) -> Result<u32, IoError> with FileSystem;

    fn [destructor]() with FileSystem {
        wasi_filesystem_close(self.handle);
    }
}

fn write_log(message: String) with FileSystem {
    let file = File::open("log.txt")?;
    file.write(message.bytes().collect());
    // file.[destructor]() called here - file is closed
}
```

### Compositional Cleanup

```wado
resource Socket {
    fn [destructor]() with Network;
}

resource File {
    fn [destructor]() with FileSystem;
}

struct Server {
    socket: Socket,      // resource
    log_file: File,      // resource
    config: Config,      // regular struct
}

// Server is implicitly unique
// Synthesized destructor:
//   fn [destructor]() with Network, FileSystem {
//       self.socket.[destructor]();
//       self.log_file.[destructor]();
//   }

fn run_server() with Network, FileSystem {
    let server = Server {
        socket: Socket::bind("0.0.0.0:8080"),
        log_file: File::open("server.log"),
        config: load_config(),
    };

    server.run();

    // server.[destructor]() called here
    // 1. socket closed
    // 2. log_file closed
    // 3. config GC'd
}
```

### Nested Resources

```wado
resource Inner {
    fn [destructor]() with Stdout {
        println("Inner destroyed");
    }
}

resource Outer {
    inner: Inner,

    fn [destructor]() with Stdout {
        println("Outer destroyed");
        // self.inner.[destructor]() called automatically after this
    }
}

fn test() with Stdout {
    let outer = Outer::new();
    // ...
}
// Output:
// Outer destroyed
// Inner destroyed
```

### Error Handling

```wado
fn safe_write(path: String, data: String) -> Result<(), Error> with FileSystem {
    let file = File::open(path)?;  // Early return on error

    file.write(data.bytes().collect())?;  // file closed on error

    return Ok(());
    // file closed on success too
}
```

### Arrays of Resources

```wado
fn open_all(paths: Array<String>) -> Array<File> with FileSystem {
    let files: Array<File> = [];

    for path in paths {
        files.push(File::open(path)?);
    }

    return files;  // Caller owns the files now
}

fn process() with FileSystem {
    let files = open_all(["a.txt", "b.txt", "c.txt"]);

    for file in files {
        file.write("data");
    }

    // All files closed here when files is dropped
}
```

## Consequences

### Positive

1. **Deterministic cleanup**: Resources are cleaned up at predictable times
2. **Compositional**: Structs with resources automatically get proper cleanup
3. **Safe**: No double-close, no leaks (enforced by `unique`)
4. **Component Model aligned**: Maps directly to WIT `resource` types
5. **Effect-aware**: Destructor effects are tracked and checked
6. **No manual bookkeeping**: Compiler synthesizes destructors for structs
7. **Familiar**: Similar to Rust's `Drop`, C++'s RAII
8. **Explicit moves**: `unique` semantics prevent accidental copies

### Negative

1. **Less ergonomic than Perl/Swift**: Requires explicit `move` for resources
   - **Mitigation**: Consistent with Wado's explicitness philosophy
2. **Panic safety unclear**: Destructors may not run on panic/trap
   - **Mitigation**: Document this limitation; use explicit error handling
3. **Learning curve**: Users must understand `unique` and move semantics
   - **Mitigation**: Clear error messages, good documentation
4. **Effect propagation burden**: Functions owning resources must declare destructor effects
   - **Mitigation**: Compiler infers and suggests required effects

### Open Questions

1. **Panic unwinding**: Should we guarantee destructor execution on panic once Wasm exceptions stabilize?
2. **`Drop` trait**: Should we allow custom user-defined "drop" logic for non-resource types?
3. **Partial moves**: Should we allow moving individual fields out of a struct containing resources?

## References

- [Wasm Component Model: Resources](https://github.com/WebAssembly/component-model/blob/main/design/mvp/Explainer.md#resources)
- [Rust Drop trait](https://doc.rust-lang.org/std/ops/trait.Drop.html)
- [Swift Deinitialization](https://docs.swift.org/swift-book/documentation/the-swift-programming-language/deinitialization/)
- [C# IDisposable](https://learn.microsoft.com/en-us/dotnet/api/system.idisposable)
- [Java AutoCloseable](https://docs.oracle.com/javase/8/docs/api/java/lang/AutoCloseable.html)
- WEP: Value Semantics and Reference Captures (2026-01-12)
