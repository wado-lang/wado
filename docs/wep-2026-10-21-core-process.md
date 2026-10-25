# WEP: `core:process` — Running Host Programs Without Extending Authority

## Context

A Wado program has no way to run another program. A build script, a test that
calls `git`, a tool that wraps a compiler: each needs one, and each is stuck.

WASI has no process interface moving through its subgroup. Wado is the one to
propose it, so this WEP designs the WIT as a WASI proposal from the start.

### A child runs outside the Wasm sandbox

A Wasm guest reaches only what its host grants. A native child process reaches
whatever the user who started the host can reach. Without OS confinement,
granting spawn grants the user's whole authority. Deno documents its
`--allow-run` this way: allowing a subprocess is allowing everything, and the
permission system no longer holds. Node's `--allow-child-process` carries the
same warning.

### A list of program names is not a boundary

Allowing `git` but not `sh` looks like a limit and is not one. A permitted
program turns into arbitrary code through its own arguments and environment:

- `git -c core.sshCommand=…`, `git --upload-pack=…`, `GIT_SSH_COMMAND`
- `LD_PRELOAD`, `DYLD_INSERT_LIBRARIES`

Denylisting these is a race against every program's option parser. The
boundary has to be the OS confining the child, or an explicit statement that
the child is fully trusted. An allowlist can only record intent.

## Decision

### The rule: spawning never extends authority

A child is confined to the guest's own grants plus a fixed system baseline. A
host that cannot confine a child does not offer process spawning at all.

This one rule carries the security model, so the WIT states it as the
interface's contract. Everything Wado would otherwise manage follows from it and
is left out:

- No program allowlist and no executable handle: the sandbox decides what may
  run.
- No denylist of environment keys: whatever the child runs, it stays inside the
  guest's grants.
- No process-specific permission per package. A dependency that spawns is in the
  same authority class as one that writes files through `Preopens`. Permissions
  per dependency are a question for effects in general, and belong to a WEP of
  their own.

### The host sandbox is the last line of defense

`#[ambient]`, `#[benign(E)]` and a user-written `#[cm("…")]` redeclaration can
each reach `Process` without the effect showing in a signature. Wado does not
close these. The effect makes intent visible; it is not the security boundary.

The `wado` CLI will also run arbitrary Component Model `.wasm`, including
untrusted modules that no Wado compiler produced. For those the language is no
boundary at all, so the host sandbox must hold on its own. Development effort
goes to the sandbox.

### A sandbox layer of Wado's own in `wado-host`

`wado-host` implements the sandbox itself rather than depending on a crate.
Three reasons:

- The policy language is small: subtrees with read-only, read-write or execute
  access, and the network all or none. A general-purpose crate's extra features
  are attack surface and audit scope Wado does not use.
- The parent stays unconfined and only the child is confined. A crate built for
  confining the current process is dangerous here.
- The candidates (nono, birdcage, wardstone and others) are young or dormant.
  Depending on one hands the speed of security fixes to someone else.

The policy is the subset both OS mechanisms express, so the two backends agree
everywhere but the rows the table marks not restricted. On Linux it is roughly
what Seatbelt provides on macOS:

| To restrict                     | macOS (Seatbelt)                       | Linux               |
| ------------------------------- | -------------------------------------- | ------------------- |
| Read and write, per subtree     | `file-read*` / `file-write*`           | Landlock            |
| Metadata, per subtree           | `file-read-metadata`                   | not restricted      |
| Exec, including by a grandchild | `process-exec`                         | Landlock `EXECUTE`  |
| Network                         | `network-outbound` / `network-inbound` | seccomp on `socket` |
| Signals and abstract sockets    | isolated by default                    | Landlock scopes     |
| UNIX sockets by path            | `network-outbound`                     | seccomp on `socket` |
| Leaving the process group       | not restricted                         | seccomp             |
| Typing into the terminal        | `file-ioctl`                           | seccomp on `ioctl`  |
| Inherited, cannot be lifted     | yes                                    | yes                 |

On Linux, seccomp covers what Landlock does not:

- The network. Landlock restricts TCP only, so a UDP socket passes it. With the
  network not granted, seccomp refuses `socket` for every family.
- UNIX sockets named by a path. Landlock does not mediate `connect` to one, so a
  child could reach D-Bus or another privileged service. seccomp refuses
  `socket` for `AF_UNIX` and leaves `socketpair`, which reaches nothing outside
  the child.
- `setsid` and `setpgid`. The host ends a child by signalling its process group,
  so a descendant that left the group would outlive it.
- `TIOCSTI` and `TIOCLINUX`, which push input into a terminal. Under
  `inherit_stdin`, a child holds the user's terminal, and input it pushes there
  runs in the user's shell, outside any sandbox. Landlock's device `ioctl` rule
  does not reach a descriptor the child inherited.
- `ptrace`.
- `io_uring_setup`. A ring performs its operations, opening a socket among
  them, without the system calls seccomp sees.

### Starting a confined child

The parent `wado` runs wasmtime on several threads. Between `fork` and `exec`, a
multithreaded process may make only async-signal-safe calls: no allocation, no
lock. Each backend fits the confinement into that rule without re-executing
`wado`, so a host embedded in another binary spawns as `wado` does.

- Linux confines the child in `pre_exec`. Landlock, seccomp and `close_range`
  are bare system calls, so the window admits them. Everything they read is
  built before the fork: the ruleset, the BPF program, and an `O_PATH`
  descriptor for each rule.
- macOS starts the program through `/usr/bin/sandbox-exec` with `posix_spawn`,
  which does not fork. `sandbox_init` compiles the profile and allocates, so it
  runs in `sandbox-exec`, a single-threaded process of its own, which then execs
  the program.

Landlock takes each rule as a descriptor rather than a path. The rules for the
guest's grants come from the descriptors of its preopens, which the host already
holds, so no path is resolved a second time between the check and the use.

The macOS profile's text holds no path. A fixed base in the host's source is
followed by one rule per grant, `(subpath (param "P0"))`, and each path reaches
it as a parameter, `-D P0=/path`. No path is spliced into SBPL, so no escaping
can rewrite the policy. Seatbelt matches the real path, so each parameter is
one: the path a grant's descriptor names (`F_GETPATH`), or the baseline path
canonicalized. The profile allows no `file-ioctl`, so no input reaches a
terminal.

`sandbox-exec` reports a program it cannot start only on its own stderr, so
the host checks that the program exists before spawning it.

### Descriptors

A child inherits descriptors 0, 1 and 2 and nothing else. Confinement mediates
what a child opens, never a descriptor it already holds, which keeps the access
it was opened with whatever the child's grants. Rust's standard library opens
every descriptor with `O_CLOEXEC`, but the descriptors `wado` itself inherited
from its parent carry no such flag.

- Linux calls `close_range(3, ~0, CLOSE_RANGE_CLOEXEC)` in `pre_exec`, after
  the confinement. Marked rather than closed, the descriptor the standard
  library reports a failed `exec` through survives until the `exec`.
- macOS sets `POSIX_SPAWN_CLOEXEC_DEFAULT`, and the spawn's file actions place
  the child's stdio at 0, 1 and 2. `POSIX_SPAWN_SETPGROUP` gives the child its
  process group.

### Escape tests

Writing the sandbox in-house obliges escape tests in CI on Linux and on macOS.
Each of these must fail: reading a file outside the grants, writing under a
read-only subtree, executing a binary outside the grants, executing one through
a grandchild, a TCP connection or a UDP datagram when the network is not
granted, connecting to a UNIX socket by path, a descendant surviving its group's
kill (on Linux), pushing input into an inherited terminal, escaping through a
symbolic link or `..`, and reading through a descriptor the host held open
across the spawn. On macOS, a grant whose path holds `"`, `\` or `)` must
confine to exactly that path.

### Where it is offered

The host offers spawning only where the kernel can enforce the rule, and never
in a weaker form:

- Linux: Landlock ABI 6 or later (kernel 6.12). ABI 6 adds the scopes that
  isolate signals and abstract UNIX sockets. Without them a child could signal
  or connect to processes outside its sandbox, which an untrusted module must
  not do. The host reads the ABI number from `landlock_create_ruleset` rather
  than the kernel version, which distributions backport against.
- macOS: Seatbelt.
- Windows: WSL2 only. Under WSL2, the Linux build of `wado` uses the Linux
  backend, so there is no Windows-specific code. The native Windows build does
  not offer spawning, and its trap message points to WSL2.

ABI 6 shuts out some current long-term-support kernels. Ubuntu 24.04's general
availability kernel (6.8) does not qualify; its hardware enablement kernel
does.

### Two ways to leave it out

A world leaves the interface out by not importing it, as the Kiln generators'
world does. That is WASI's link-time capability model. The Component Model has
no optional import yet: a host that does not supply an import refuses to
instantiate the component, even one that would never spawn. So the `wado` host
links the interface wherever its world imports it, also under `--deny-process`
and on a kernel that falls short.

There, the interface answers at run time. `get-availability` returns
`available`, `denied` (the host turned it off) or `unsupported` (the OS cannot
confine a child), with no side effect. The two refusals call for different
remedies, a host flag for one and a newer OS for the other, so an `option`
getter in the style of `wasi:cli`'s `get-terminal-stdin`, which merges them into
`none`, says too little.

Every other function of the interface requires `get-availability` to answer
`available`, and traps otherwise. The answer is fixed when the host starts, so
one query settles it for the instance's lifetime, and `spawn`'s `error-code`
carries only failures of the one spawn. The WIT cannot state this as a contract,
so its documentation does.

### System baseline

A native program reads paths a Wasm guest never needs: its loader, its shared
libraries, its locale data. The baseline is that set, and it is the one way a
child may read more than the guest. It is defined, not open-ended:

| Purpose                        | Linux                                                    | macOS                                 |
| ------------------------------ | -------------------------------------------------------- | ------------------------------------- |
| Loader and shared libraries    | `/lib*`                                                  | `/usr/lib`, `/System`, the dyld cache |
| Installed programs             | `/usr`, `/bin`, `/sbin`, `/nix/store`                    | `/usr`, `/opt/homebrew`, `/opt/local` |
| System configuration           | `/etc`                                                   | `/private/etc`                        |
| Name resolution (network only) | `/etc` alone                                             | mDNSResponder                         |
| TLS certificates               | `/etc` alone                                             | the keychain                          |
| Locale and time zones          | `/usr/share/zoneinfo`, locale data                       | the same                              |
| Devices                        | `/dev/null`, `/dev/urandom`, `/dev/tty` when inheriting  | the same                              |
| Scratch                        | a private `HOME` and `TMPDIR` per spawn, removed at exit | the same                              |

- The baseline is read and execute only. The writable entries are the private
  `HOME` and `TMPDIR`, `/dev/null`, and `/dev/tty` when it is inherited.
- The list is fixed per OS in the host's source. Nothing in it is derived from
  the program's path, which the guest chooses: deriving a prefix from
  `/home/u/bin/x` would open `/home/u`. A program outside these prefixes and
  outside the grants runs only from a path the user adds with
  `--process-read <path>` (`run`, `serve`, `test`), such as `~/.cargo`. It is
  read and execute only, opened to every child and never to the guest, and
  resolved through its symlinks when `wado` starts, so a path that names no
  file fails there. It is a command-line flag alone: a manifest is the
  project's content, and a grant the project could widen would reach past
  what its own code is given.
- The real `HOME` is never in the baseline. A child sees it only through an
  explicit grant, so `~/.ssh` stays out of reach by default.
- No UNIX socket named by a path is open, except mDNSResponder's on macOS, where
  name resolution goes through it. A socket such as D-Bus is a path to a
  privileged service. On Linux, glibc resolves through `resolv.conf` without
  one.
- `/etc` is open whole. A toolchain reads its settings there (a JDK's
  `java.security` lives in `/etc/java-*`), and what it holds is the system's,
  readable by every user. A secret under it, such as `/etc/shadow` or an SSH
  host key, is readable by root alone, which the guest's user is not.
- With the network not granted, mDNSResponder's socket drops out.

### Grants

- On by default wherever the sandbox works. `--deny-process` turns it off. The
  sandbox keeps the child within the guest, so an opt-in would protect nothing.
- `wado run`, `wado test` and `wado serve` all offer it.
- Kiln generators never do: a generator must be deterministic, and a child is
  free to read clocks and randomness.
- The child gets the guest's network grant, all or none. A child finer-grained
  than its guest would invert the rule.
- Each directory the guest is granted must map to the same path on the host;
  otherwise `get-availability` answers `denied`, and the trap a spawn raises
  names the mapping. A guest path that names another host path cannot appear in
  a child's arguments. The program still starts, since it may never spawn. `wado run`'s default
  grant, the current directory as `.`, counts as the same path: the child's
  working directory is that directory, so a relative path means the same thing
  on both sides.
- Files inside a granted directory may be executed, so a binary the project just
  built can run. This adds no authority, since the child cannot leave the grants
  either way.

### WIT

The interface ships as `core:process@0.1.0`, as `core:eval` and `core:kiln`
ship theirs, written in the shape a WASI proposal takes. Its WIT sits in
`lib/core/process/`, with the `wasi:filesystem` WIT it uses under `deps/`.
Claiming `wasi:` before the subgroup adopts it would promise compatibility Wado
cannot keep with other hosts. On adoption only the namespace changes.

```wit
package core:process@0.1.0;

interface types {
    variant exit-status { exited(u8), signaled(u8), timed-out }

    enum availability { available, denied, unsupported }

    /// `not-permitted`: the sandbox refused to execute this program.
    enum error-code { not-found, not-permitted, resource-exhausted, io }

    variant input { null, inherit, piped(stream<u8>) }
    enum output { null, inherit, piped }

    record command {
        program: string,
        args: list<string>,
        env: list<tuple<string, string>>,
        stdin: input,
        stdout: output,
        stderr: output,
        /// Nanoseconds, as `wasi:clocks` counts a duration.
        timeout: option<u64>,
    }

    resource child {
        kill: func();
    }

    record spawned {
        child: child,
        stdout: option<stream<u8>>,
        stderr: option<stream<u8>>,
        exit: future<result<exit-status, error-code>>,
    }
}

interface process {
    use wasi:filesystem/types@0.3.0.{descriptor};
    use types.{availability, command, spawned, error-code};

    get-availability: func() -> availability;

    spawn: func(cmd: command, cwd: option<borrow<descriptor>>) -> result<spawned, error-code>;
}
```

- Dropping a `child` kills its process group. The host kills every child when
  the guest exits. The host signals a group only before it reaps the group's
  leader: the unreaped leader holds its pid, so the group's id cannot pass to an
  unrelated process.
- The group dies with its leader. Once the child exits, the host kills what is
  left of its group before reaping it, so no descendant keeps the child's grants
  with nobody watching, and none holds a pipe the guest is reading open.
- `exit-status` takes Unix's shape, a `u8`, since Windows runs only under WSL2.
  `timed-out` is distinct from `signaled(9)`.
- `cwd` is its own parameter because a record cannot hold a `borrow`. With
  `none`, the child starts in the first preopened directory, the one `core:fs`
  resolves paths against. The directory the host started in may lie outside the
  grants.
- A `program` holding a `/` is a path. Any other is looked up on the host's
  `PATH`, skipping empty and relative entries, the rule external subcommands
  already follow. There is no `find-executable`: the sandbox decides what runs,
  and a name it cannot find is `not-found`.
- The guest's environment is not passed on. The child gets the baseline
  variables (`PATH`, its private `HOME` and `TMPDIR`, `LANG` and `LC_*`, and
  `TERM` only when a stream is inherited) and then the guest's `env`, which
  overrides them.
- At the timeout the host sends `SIGTERM` to the process group, then `SIGKILL`
  after a grace period. The grace is 3 seconds and a host option changes it; the
  guest does not. The host enforces the timeout, so it holds even while the
  guest is blocked reading a stream.

### `core:process`

```wado
use { Command, cmd } from "core:process";

let rev = cmd`git rev-parse ${branch}`.read()?;            // stdout as a ByteList; a non-zero exit is an error
let out = Command::new("cargo").args(["build"]).output()?;  // stdout and stderr, read together
let status = cmd`make -j${n}`.inherit_stdio().status()?;   // the child writes to the terminal itself

let mut child = Command::new("wc").stdin_piped().stdout_piped().spawn()?;
// child.stdin: StreamWritable<u8>, child.stdout: Stream<u8>
```

- One effect, `Process`. `spawn`, `output`, `status` and `read` declare it, and
  so does `process::availability()`, which returns an `Availability` mirroring
  the WIT enum. The others require it to be `Available` and trap otherwise. A
  `contract` clause cannot perform an effect, so this one is documented rather
  than written as a clause.
- `inherit_stdin`, `inherit_stdout`, `inherit_stderr` and `inherit_stdio` also
  declare `Stdin`, `Stdout` and `Stderr`: a guest passes on only a capability
  it holds.
- `output` reads stdout and stderr concurrently, so a child filling one pipe
  cannot deadlock against a parent reading the other.
- `Child` is `affine`, and dropping it kills the process. `wait(self)` returns
  `Result<ExitStatus, ProcessError>`, and `kill(&self)` ends it early. There is
  no `detach`: the host kills every child when the guest exits anyway.
- A pipe nothing can reach would block the child, so none is left open.
  `wait` closes every stream still in the `Child`, and `output` and `status`,
  which hand out no `Child`, give the child `/dev/null` for a stream piped
  there.
- A pipeline passes one child's `stdout` stream to the next one's `stdin`.
- `Command::timeout(d: Duration)` sets the timeout, in `wasi:clocks`'s
  `Duration`, the nanoseconds the WIT carries. There is no default. A child
  that runs out ends in `ExitStatus::TimedOut`: `output` returns what it read
  so far along with that status, and `read` fails.
- Captured output is capped at 64 MiB by default, and
  `Command::capture_limit(bytes)` changes the cap for both streams. Past it,
  stderr keeps its tail. Stdout past it is an `OutputTooLarge` error, since
  output with its middle cut out (JSON, say) is wrong rather than short. The
  host kills the process group the moment stdout passes the cap, and the error
  returns once the child has exited: a child left writing into a pipe nobody
  reads would block forever.
- Captured output is a `ByteList`, never a `String`. A program's output is bytes,
  and decoding it is the caller's call, so no kind reports bad UTF-8.
- `ProcessError { program, kind }` names the program as `FsError` names its
  path. The kinds are `NotFound`, `NotPermitted`, `Io(ErrorCode)`,
  `Failed([ExitStatus, ByteList])`, carrying stderr, and `OutputTooLarge`. Only `read` returns
  `Failed`: `output` and `status` hand the exit status back as a value, while
  `read` returns only stdout and has nowhere else to report it.

### The `cmd` template

`` cmd`…` `` builds an argument vector without a shell, so a hole can neither
split into several arguments nor run as shell syntax. It can still be read as an
option: a value starting with `-` is one to the program, so a hole that may hold
one goes after `--` where the program accepts it.

1. Literal text splits on whitespace, newlines included:
   `` cmd`git status --short` `` is `["git", "status", "--short"]`.
2. A hole is exactly one argument, whatever it holds: in
   `` cmd`git commit -m ${msg}` ``, a `msg` of `"fix the bug"` is the one
   argument after `-m`.
3. A `List<String>` hole expands to one argument per element:
   `` cmd`rm -- ${files}` ``.
4. Parts with no whitespace between them join into one argument:
   `` cmd`cp ${src} ${dir}/out.txt` ``. A `List<String>` hole joined to
   anything is a contract violation (`` cmd`--file=${files}` ``): neither one
   argument nor one per element is the obvious reading.

Literal text must not hold a character a shell would treat specially, since
here it would mean nothing and the program would read it wrong. Writing
`` cmd`git commit -m "fix bug"` `` would otherwise pass `"fix` and `bug"` as
two arguments. Each of these in literal text is a contract violation:

| Kind                      | Characters                          |
| ------------------------- | ----------------------------------- |
| Quoting and escaping      | `"` `'` `\`                         |
| Pipes, lists, redirection | `\|` `&` `;` `<` `>`                |
| Subshells and groups      | `(` `)` `{` `}`                     |
| Expansion                 | `$` (other than `${`), `` ` ``, `~` |
| Globbing                  | `*` `?` `[` `]`                     |
| Comments                  | `#`                                 |

A value holding any of them goes through a hole: `` cmd`grep ${"a|b"} log` ``.
Everything else is allowed, so `--flag=value` and `user@host:path` stay
literal. Starting strict leaves room to admit a character later; admitting one
cannot be taken back.

### Deliberate omissions

- A shell API. `sh -c` is a program like any other, confined like any other.
- `detach`, `find-executable`, a program allowlist and an environment denylist,
  for the reasons above.

## Roadmap

- [x] The sandbox layer in `wado-host` with the Linux backend: the policy type,
  the baseline, Landlock and seccomp, the ABI 6 check, the confinement and
  `close_range` in `pre_exec`. Done when the escape tests pass in CI on
  Linux.
- [x] The macOS backend: the fixed profile, its parameters, and the
  `posix_spawn` of `sandbox-exec`. Done when the escape tests pass in CI on
  macOS.
- [x] `core:process@0.1.0` in the `wado` host: `get-availability`, spawning,
  streams, the timeout, killing the process group on drop and on guest
  exit, `--deny-process`, and `denied` for a mapping that is not the same
  path.
- [x] `core:process` in the stdlib: `Command`, `cmd`, `Child`, `output`,
  `status` and `read`, with its stdlib reference and a cheatsheet entry.
- [ ] Filtering the network per host through a proxy, for the guest and the
  child together.
- [ ] WSL2: confirm the Landlock ABI its kernel offers. Done when the escape
  tests pass under WSL2.

## Known gaps

- The CLI surface is not named for a flag that grants the real `HOME`, or for
  the option that sets the grace period, which stays at 3 seconds until it is.
- The compiler's e2e harness does not link `core:process`, so a fixture cannot
  spawn; the stdlib's own tests cover it under `wado test`.
- A component that spawns imports `core:process`, which no WASI world defines,
  so only the `wado` host instantiates it. Nothing at compile time says so: an
  effect the selected world does not import is imported all the same
  ([Worlds](./spec-worlds.md)).
- Resources are unlimited. A child can run many children, use memory and CPU
  without bound, and fork without limit. How many children may run at once,
  rlimits and cgroups are all open.
- Landlock does not restrict `stat`. A child on Linux can learn whether a path
  outside its grants exists, and its metadata. Seatbelt hides it but for the
  directories above a grant, which resolving a path stats, and the `/var` link,
  which dyld stats.
- A guest directory granted under a different host path turns spawning off, even
  where a mount namespace could present the guest's view to the child.
- Seatbelt cannot stop a descendant from calling `setsid`. On macOS such a
  descendant survives its group's kill and keeps the child's grants after the
  guest exits.
- Darwin's `killpg` answers `EPERM` both for a group holding only zombies and
  for a live member the guest's user cannot signal (a setuid program). The
  macOS backend reads `EPERM` as the first, so a group kill that missed such a
  member reports success.
- No pseudo-terminal: a child that needs one (a full-screen editor) gets the
  inherited terminal or none.
- `sandbox-exec` is marked deprecated in its manual page. macOS still ships it,
  and nothing public replaces it.
- On macOS, a program the sandbox refuses to execute starts `sandbox-exec`,
  which then exits with a status, rather than failing the spawn with
  `not-permitted`.

## Related WEPs

- [`core:fs`](./wep-2026-09-12-core-fs.md) — the preopens whose grants a child
  inherits.
- [External Subcommands](./wep-2026-09-19-external-subcommands.md) — the `PATH`
  rule `program` resolution reuses.
- [Kiln](./wep-2026-04-12-kiln.md) — the deterministic generators that never
  spawn.
- [Concurrent Stream Copies](./wep-2026-10-22-concurrent-stream-copies.md) —
  how `output` and `read` read stdout and stderr together.
- [Affine Types](./wep-2026-10-17-affine-types.md) — the discipline `Child`
  follows.
