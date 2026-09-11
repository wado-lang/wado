# Research: Language Survey — Hike

Survey against [the rubric](./research-language-survey.md).

`kanryu/hike-lang` — 115 commits / first 2026-08-27 / surveyed at fac6525a
2026-09-11. One author, sixteen consecutive days.

Arbiter: "Hike is an experimental systems language combining Go-style syntax and
ergonomics with C-equivalent execution, direct C-ABI compatibility, and no
garbage collection." — `README.md`, opening the Overview. Four goals joined by
"and", in no order.

A Go-shaped language whose compiler is a frontend only: `hikec` emits LLVM IR and
hands every backend concern — object format, optimization, linking — to Clang.
19,528 lines of Go against 3,761 lines of Hike in the whole repository.

Examined: `README.md`, `concurrency.md`, `encoding.md`, `wasm.md`,
`without_cgo.md`; the whole of `std/` by count and sampled reading, with
`std/encoding/json/json.hike` and `std/maps/maps.hike` read in full;
`pkg/token/token.go`, `pkg/target/target.go`, `pkg/backend/llvm/runtime.go` and
`runtime/runtime.ll` in full; `pkg/sema/sema.go`, `pkg/parser/parser.go`,
`pkg/backend/llvm/emitter.go` and `pkg/compiler/wasmjs.go` in the regions the
rows below cite; `tests/e2e_setup_test.go` in full and the other nine test files
by name and count; `examples/` by listing, with `shared/libcalc.hike` read.

Nothing was run, and that bounds everything here. This environment has no Go
toolchain and no Clang, so the compiler was never built and no Hike program was
ever compiled. Every "Reality" below is read from the implementation or counted
with a command. None is observed behaviour.

Not examined: `pkg/lower/` (4,411 lines) and `pkg/transform/` beyond the cited
call sites, and `pkg/codegen/debug.go`.

## A. The language

| Axis                   | Claim                                                                                                          | Reality                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          | Holds in self-application                                                                                                                                                  | Unimplemented / Rejected                                                                                                                                             |
| ---------------------- | -------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A1 Canonicity          | Not claimed anywhere                                                                                           | Synonyms at every layer. `token.go:123` maps both `Async` and `async` to one token. A C import has two spellings, `extern func f(…)` and a bodyless `func f(…)`, which sema treats identically (`IsExtern: (fd.Body == nil)`, `sema.go:788`, against `sema.go:820`). `float` is an undocumented alias of `float64` (`sema.go:67`). `runtime.ll` carries `os_sleep_ms` and `c_os_sleep_ms` as byte-identical definitions, likewise `os_now_ns` / `c_os_now_ns`. The darwin triple is `arm64-apple-darwin` in `target.go` and `arm64-apple-macosx` in `emitter.go` | No — the stdlib uses the bodyless form throughout, and spells the void return both ways: `func free(ptr *byte)` in `json.hike`, `func free(ptr *byte) void` in `maps.hike` | —                                                                                                                                                                    |
| A2 Type vocabulary     | Go's, mapped to fixed widths: `int`→`i64`, `byte`→`u8`, `bool`→`i1`, `string`→`i8*`                            | Go's, plus `cstring` for the raw null-terminated `*byte` that C-interop needs (`encoding.md` §1). `PointerSize` and the width of `int`/`string` are mutable package globals set once per process from the target (`sema.go:32`, `sema.go:80`), so the type system's `Size()` answers depend on process state                                                                                                                                                                                                                                                     | —                                                                                                                                                                          | `rune` is accepted by the parser's builtin-name list (`parser_expr.go:92`) and absent from `sema.BuiltinTypes`, while `encoding.md` §2 recommends `[]rune` traversal |
| A3 Effects             | None, and function colouring is named as the thing being avoided (`concurrency.md` §4)                         | None, not even one bit. Nothing in the type system distinguishes a function that allocates, blocks, or calls C                                                                                                                                                                                                                                                                                                                                                                                                                                                   | —                                                                                                                                                                          | Refused, with the reason written: `Async` is a property of the call site, never of the callee                                                                        |
| A4 Errors              | Go's: values, multi-return, `error` as a builtin interface                                                     | `error` is one interface registered in `sema_context.go:58`. No propagation operator; every error is branched on by hand or dropped. No `panic` / `recover` in the keyword table. `errors.New` takes a `*byte` and `errorString.Error()` returns `*byte`, so the error message travels in the FFI type, not in `string`                                                                                                                                                                                                                                          | No — the two parsers that matter return `nil` on failure, not an error: `json.Parse(jsonStr string) *Value`, `toml.Parse(tomlStr string) *Value`                           | No stated rule for when an error is worth branching on                                                                                                               |
| A5 Concurrency         | `Async(f)` / `<-task` / `chan`; 1:1 OS thread-pool offload, explicitly not green threads (`concurrency.md` §5) | Hand-written LLVM IR over kernel32 alone: `QueueUserWorkItem`, `CreateEventA`, `SetEvent`, `WaitForSingleObject`, `CloseHandle`, `Sleep`, `GetTickCount64`. A grep for `pthread`, `clock_gettime`, `nanosleep` and `usleep` across `pkg/` and `std/` returns nothing                                                                                                                                                                                                                                                                                             | —                                                                                                                                                                          | Not built yet for four of the five targets the CLI accepts. `Async`, `chan` and `std/time` cannot link on linux, darwin or wasm32                                    |
| A6 Boundary mechanisms | Four generators, no macros, no reflection                                                                      | The widest axis in the project. C in: a bodyless `func` becomes a `declare`, no wrapper. C out: `cfunc` plus `-header` emits the matching `.h` (`cgen/header.go`, 256 lines). Go out without cgo: `passthrough cfunc` emits a `.syso` and a Plan 9 assembly stub (`compiler/go_decl.go`, 274 lines). JS out: `jfunc` plus a generated `runtime.js` (`compiler/wasmjs.go`, 169 lines)                                                                                                                                                                             | —                                                                                                                                                                          | Nothing comes _in_ but a C signature retyped by hand. No IDL, no schema, no serialization framework: `std/encoding/json` and `toml` are hand-written DOM parsers     |
| A7 Hidden operations   | No list                                                                                                        | At least six, none enumerated: escape-driven heap promotion (`sema.go:1182`), monomorphization, implicit numeric and interface casts (`ast.ImplicitCastExpr`, synthesized at `sema_context.go:1192`), itab boxing on interface assignment, `memcpy` insertion for struct copies (`lower_call.go:190`, `:218`), and the `alloca` a range loop issues for a custom container's iterator state                                                                                                                                                                      | —                                                                                                                                                                          | —                                                                                                                                                                    |

Everything the four generators produce is a text file you can open, diff, and
hand to another toolchain: the `.ll`, the `.h`, the `.s`, the `runtime.js`. None
of it needs `hikec` to be read.

### The self-application cross-check

There is no self-hosting; it is the last unchecked box on the README roadmap. So
the hardest program in Hike is its own standard library: 3,001 lines across 17
files, the largest being `std/fmt/sprintf/sprintf.hike` at 501.

The privileged prefix is the bodyless `func`, which is how a C symbol enters.
9 of the 17 stdlib files declare and call C directly, 53%. 5 of them also perform
unchecked pointer casts, 24 sites. There is no module a libc declaration can live
in, so each one is retyped where it is wanted: `malloc` and `free` appear five
times each. All five land in one entry, because `ctx.Functions` is a flat map
keyed by bare name (`sema.go:823`) with no package qualification for externs.
They agree today. If two ever disagreed, the last one read would win in silence.

The floor is not a typed generic operation. It is `malloc` with a constant the
author worked out by hand:

```go
v := (*Value)(malloc(128))                // json.hike:43, ×6; toml.hike:45, ×6
b := (*Builder)(malloc(24))               // builder.hike:18
var newEntry *Entry[K, V] = malloc(32)    // maps.hike:116
m.buckets = calloc(initialCap, 8)         // maps.hike:88
```

The language has no `sizeof`: the string appears nowhere in `pkg/` or `std/`. So
every one of those numbers is a literal that nothing rechecks. Adding a field to
`json.Value` is a silent heap overflow. `Entry[K, V]` is generic and allocated at
a fixed 32 bytes, correct only while `K` and `V` are both 8-byte types, which is
every use the repository contains.

The compiler computes the same fact twice, too. `StructType.Size()`
(`sema.go:170`) is an unpadded sum of field sizes, while the struct it emits,
`%struct.X = type { … }`, is not packed, so LLVM pads it. That `Size()` supplies
the element stride for slice indexing and the byte count for `make`. The two
answers agree only for structs whose fields are uniformly wide, which every
struct in the stdlib happens to be.

The headline generics claim does not reach the stdlib either. The README's
example is `func Min[T int | float64](a T, b T) T`, and `ast.TypeParam` has two
fields, `Token` and `Name`, with no constraint node at all. `parseTypeParams`
(`parser.go:580`) reads an identifier, then breaks and calls
`expectPeek(RBRACKET)` on the next token. The repository's own copy of the
example the README quotes, `examples/shared/libcalc.hike`, writes `func Add[T](a
T, b T) T` and calls it as `Add[int](a, b)`. Nothing in `tests/`, `examples/` or
`std/` uses a union constraint.

One more thing the stdlib author lives with: a type error in Hike source is
reported as a Go `panic()`. There are 23 panic sites in `pkg/`, 9 of them in
`sema_context.go`.

### Count the implementations: two maps

Hike ships a builtin `map[K]V` and a `std/maps.Map[K, V]`, and they do not agree
about what a key is.

The builtin lowers to `__hike_map_*` in `runtime.ll`: `__hike_hash_str` is FNV-1a
over the bytes, and `__hike_map_key_eq` compares pointers first, then falls
through to `strcmp`. It is correct.

`std/maps` hashes with `func hashKey[K](key K) int { return int(key) }`, which
for a string key is the pointer value. It compares with `==`, which does compare
contents. Two equal strings at different addresses therefore land in different
buckets and never match. The README documents this one, in §7, as the way to use
a hash map. Its example works because both the insert and the lookup name the key
as a literal, and literals are interned into one `@.str` global.

Nothing holds the two together: there is no CI, no differential test, and no
e2e case for `std/maps` at all.

### Where the reality is a bug

`NewNull`, `NewBool`, `NewNumber` and `NewString` (`json.hike:42`–`67`, mirrored
in `toml.hike`) allocate 128 bytes with `malloc`, not `calloc`, and then set one
or two fields. `Value`'s three slice headers — `ArrVal`, `ObjKeys`, `ObjVals` —
keep whatever the allocator handed back. Any code that asks a scalar node for
`len(v.ArrVal)` reads that.

Both files declare `free` and never call it. That is the general case, not an
oversight in two files: the compiler emits `malloc` for closure environments,
`append` growth, string concatenation, substrings and slice-to-string, and it
never emits `free` anywhere (`grep '"free"' pkg/lower pkg/backend` finds only the
symbol-table entry). `%struct.Arena` and `%struct.Allocator` are declared in
`runtime.ll` and used by nothing; the arena allocator is a roadmap box. "No
garbage collection" turns out to mean no deallocation.

The libc dependency is imported from the host rather than removed. `wasm.md` §1
tables it as "Completely Zero (LLVM IR built-in)", and `runtime.ll` does define
`memcpy`, `memcmp`, `strlen` and `strcmp` as
`define internal`. Three lines above them it declares `malloc`, `calloc` and
`free` as external. The wasm link passes `-Wl,--allow-undefined`, and the
generated `runtime.js` implements `malloc` as a JavaScript bump allocator whose
`free` is an empty function (`wasmjs.go:36`–`56`). The allocator that the table
counts as zero is in the glue that the same document counts as generated.

## B. The practice

| Axis                        | Present / Practised / Refused / Absent | Scope or doctrine                                                                                                                                                                                                                                                                                                                            |
| --------------------------- | -------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| B1 Arbiter                  | Absent                                 | The Overview sentence joins four values with "and" and never ranks them: Go ergonomics, C-equivalent execution, C-ABI compatibility, no GC. Nothing in the repository says which one yields                                                                                                                                                  |
| B2 Accept/reject criteria   | Absent                                 | No CONTRIBUTING, no PR template, no CI. `go test ./...` is the README's stated check and `pkg/` contains zero test files                                                                                                                                                                                                                     |
| B3 The "why" record         | Practised                              | Four topic documents each argue their design in prose: `concurrency.md` §5 tables `Async` against `go func()` and gives three reasons for the rename; `without_cgo.md` §2 explains the `.syso` and Plan 9 mechanisms; `encoding.md` §1–2 states the UTF-8 invariant and why slicing breaks it. No index, no template, no per-decision record |
| B4 Falsifier                | Absent                                 | No decision template exists to carry one                                                                                                                                                                                                                                                                                                     |
| B5 Rejection record         | Practised, one entry                   | `concurrency.md` §5 is a full rejection of Go's `go` keyword with the reasoning kept: the green-thread fallacy, and what a 1:1 pool does when a user spawns a hundred thousand tasks. It is a section of a feature document, not a list, and there is no rule for citing it                                                                  |
| B6 Sync gate                | Absent                                 | No CI of any kind. The README roadmap still lists "Dynamic interface dispatch (`vtable`)" as an open box while `tests/e2e_interface_test.go` exercises it through an itab and `emitter.go` has an `emitItabs` stage. Nothing in the repository could have caught that                                                                        |
| B7 Self-reported violations | Practised, scoped to one feature       | `without_cgo.md` §6 lists two standing constraints of `passthrough cfunc` with the mechanism and the consequence, including "silent stack corruption" on a ~2 KB goroutine stack. `encoding.md` §2 does the same for string indexing. The README's Status note scopes all testing to Windows/MinGW. No file:line, no ledger                  |
| B8 Who decides              | No rule                                | One author, 115 commits, no co-author trailers                                                                                                                                                                                                                                                                                               |

B7 and B6 point opposite ways. Where this project writes a document about a
mechanism, it is unusually candid about that mechanism's failure modes: §6 of
`without_cgo.md` names the corruption before anyone hits it. Where a claim lives
in a list rather than in an argument, nothing maintains it. The roadmap
contradicts the test suite, and the CLI offers four targets the runtime cannot
serve.

The Almide survey met the same split from the other side. There, the rule with
nothing on screen to prompt it was the one that leaked.

## C. What outlives it

| Kind        | What                                                                                                                                                                      | Externality                                                                               | Contender                      | Wins                         |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------- | ------------------------------ | ---------------------------- |
| C1 Artifact | None. The largest non-stdlib, non-test Hike program is `examples/toml/main.hike`, 110 lines                                                                               | —                                                                                         | —                              | —                            |
| C2 Method   | Cgo-free native FFI: compile to a `.syso` the Go linker merges automatically, bridge the ABI with a generated Plan 9 assembly stub, and keep the object free of libc refs | High — the recipe needs no Hike                                                           | cgo                            | On call cost and build deps  |
| C2 Method   | The 2-pass stack iterator protocol: `InitIterator(nil)` returns the state size, the caller `alloca`s it, `InitIterator(buf)` initializes it in place                      | High — a way to give a container a zero-allocation range loop with no GC and no lifetimes | An iterator object on the heap | On allocation, not on safety |
| C4 Corpus   | 74 e2e cases, each an inline Hike program with its expected stdout and exit code, run end-to-end through Clang                                                            | Low                                                                                       | —                              | —                            |
| C4 Corpus   | `runtime/runtime.ll`: 1,106 lines of hand-written LLVM IR — a thread pool, channels, a hash map, string ops, in both 64-bit and 32-bit forms                              | Medium — readable by anyone emitting IR                                                   | Writing the same runtime in C  | Not obviously                |

C1 is empty, and here that means "not built yet" rather than a consequence of the
design. Hike's boundary is wide open in four directions, and on day sixteen
nobody has walked through it yet. Almide's empty C1 was the opposite case:
nothing external could have been built there, because there was no way in.

C2 is where the project's spare effort went. It does not follow from B1, because
B1 does not exist. Both methods came out of one instinct rather than a stated
arbiter: refuse the runtime, and pay for it at the boundary with generated code
you can read.

## For Wado

Learned:

- A generator is worth what its output is worth as a file. Each of Hike's four
  boundary mechanisms emits a text file a person can open, diff, and hand to a
  different toolchain. That is the whole reason a sixteen-day-old project can
  link into a Go binary without cgo: no step in the chain is a black box that
  only its own compiler can consume. The test for Wado's generation is not
  whether the output is correct. It is whether a user who does not trust it can
  look.
- Giving a language raw allocation without `sizeof` creates a defect class, not a
  defect. Every allocation in Hike's stdlib is a hand-computed literal, and
  adding a field to a struct silently overflows the heap. The same shape appears
  wherever a compiler
  computes one fact in two places: Hike's `StructType.Size()` is an unpadded sum
  while the LLVM struct it emits is padded, and the two agree only because every
  stdlib struct happens to have uniform field widths. A size computed twice is a
  bug waiting for the first struct that makes the answers differ.
- "No GC" names what was removed, not what replaced it. Hike's arbiter says no
  garbage collection, and what that turns out to mean is that the compiler emits
  `malloc` for closures, `append`, concatenation and substrings and emits `free`
  nowhere. For a short-lived CLI that is a defensible choice; the defect is that
  no document says it. A memory-model claim has to name who frees, or it is a
  claim about the implementation rather than about the language.
- Two spellings of one concept are the same defect as two backends, at a smaller
  scale. The rubric's "count the implementations" question is aimed at native
  versus wasm; applied to Hike it found a builtin `map[K]V` that hashes string
  keys with FNV-1a and a `std/maps.Map` that hashes them by pointer value, both
  shipped, the broken one documented. The useful version of the question is not
  "how many backends" but "how many things in this project claim to be the map".
- A claim attached to its reasoning survives; a claim filed in a list goes stale.
  Hike's feature documents volunteer their own failure modes, down to naming a
  silent stack corruption nobody has hit yet. Its README roadmap still lists
  dynamic interface dispatch as unbuilt, with a passing e2e test for it in the
  same repository. Nothing separates the two but where the sentence lives.

Take:

- Nothing. Every candidate was checked and settled the other way.

  Hike's strongest axis by far is A6, and that is where a Take should have come
  from. Every one of its four boundary mechanisms is retyped by hand, and every
  Wado counterpart is driven by an IDL: a C signature against `use` from a
  WIT-described component, a generated `.h` against
  [WIT bundling](./wep-2026-03-21-wit-bundling.md), an untyped `--export-all`
  module plus a generated bump allocator against jco's typed bindings generated
  from the same WIT. A project can be ahead on how wide its boundary is and
  behind on every single crossing.

Refuse:

- Reconciling a type's size across the places that compute it. This is Hike's
  sharpest defect: `StructType.Size()` is an unpadded sum while the emitted LLVM
  struct is padded, two answers to one question with nothing between them. It
  does not reach Wado. Checked: `wado-compiler/src/cm_abi.rs` is the sole owner
  of canonical-ABI layout and every caller goes through it, and a WasmGC struct
  has typed slots rather than a byte layout, so there is no second answer for the
  first to disagree with. The lesson survives; the work item does not.
- The 2-pass stack iterator protocol. It buys one heap allocation per range loop
  in a language that has no GC to make that allocation cheap. Wado has one. The
  price is a two-call contract the user has to see, plus a hand-written byte
  count, which is the construct that produced `malloc(32)` in Hike's own map.
- `cstring` as a second string type. Hike splits it from `string` so non-UTF-8
  bytes cannot enter the managed type. Wado holds the same invariant with a
  discipline instead,
  [checked / unchecked / internal](./wep-2026-05-16-string-checked-unchecked-discipline.md),
  and validates at the CM boundary. A second surface type would add a spelling
  without adding a guarantee.
- A gate against two spellings of one keyword. Checked, and structurally
  impossible in Wado: `wado-compiler/src/syntax.rs` generates the keyword table,
  the lexer's lookup and the reverse mapping from one `keyword_registry!` entry
  list, so a duplicate literal is an unreachable match arm and the workspace
  compiles with zero warnings. Hike's `Async` / `async` pair needs a hand-written
  map to exist.
