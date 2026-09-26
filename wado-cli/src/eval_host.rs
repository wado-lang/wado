//! The `wado test` host for `core:eval`: compile a source string as a command,
//! run it with nothing but stdout, stderr and exit, and cache the outcome.
//!
//! See WEP 2026-09-26 (Eval).

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use tokio::sync::OnceCell;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, GcHeapOutOfMemory, ResourceLimiter, Store, Trap};
use wasmtime_wasi::cli::{WasiCli, WasiCliView};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::p3::bindings::Command;
use wasmtime_wasi::p3::bindings::cli::{exit, stderr, stdout};
use wasmtime_wasi::{I32Exit, WasiCtx, WasiCtxView, WasiView};

use wado_compiler::hashmap::IndexMap;
use wado_compiler::{CompilerOptions, Diagnostic, InMemoryCompilerHost, Severity};

use crate::COMPILER_STACK_SIZE;
use crate::cache::write_atomic;
use crate::compile::{build_dir, load_nearest_manifest, render_error_diagnostics};
use crate::kiln_provider::hex32;
use crate::knobs::CompileKnobs;
use crate::runtime::{CAPTURED_STDIO_CAPACITY, DEFAULT_GC_HEAP_INITIAL_SIZE, create_fuel_engine};
use crate::sync::lock;

wasmtime::component::bindgen!({
    inline: "package wado:eval-runner; world runner { import core:eval/eval-host@0.1.0; }",
    path: "../wado-compiler/lib/core/eval",
    world: "wado:eval-runner/runner",
    imports: { default: async },
    additional_derives: [serde::Serialize, serde::Deserialize],
});

use self::core::eval::eval_host::{self, CompileFailure, Outcome, Ran, Status, TrapKind};

/// The file name an evaluated program's diagnostics carry. It has no directory,
/// so a relative `use` resolves to nothing.
const EVAL_FILE: &str = "eval.wado";

/// How long a compile may run. Only a compiler bug makes one loop, and a
/// machine-dependent limit is why a timeout is never cached.
const COMPILE_TIME_LIMIT: Duration = Duration::from_secs(120);

/// The most any one memory of an evaluated program may grow to, in bytes.
const MEMORY_CEILING: usize = 1 << 30;

/// The most elements any one table may grow to: as many pointers as fit under
/// [`MEMORY_CEILING`]. Derived from it, so the cache key covers it.
const TABLE_CEILING: usize = MEMORY_CEILING / size_of::<usize>();

// The GC heap grows through the same limiter, so it has to start below the
// ceiling or no program could allocate at all.
const _: () = assert!(MEMORY_CEILING as u64 > DEFAULT_GC_HEAP_INITIAL_SIZE);

/// The interfaces the program is linked against. A component importing
/// anything else is refused as `Unavailable` before it is instantiated.
const LINKED_INTERFACES: &[&str] = &[
    "wasi:cli/types@0.3.0",
    "wasi:cli/stdout@0.3.0",
    "wasi:cli/stderr@0.3.0",
    "wasi:cli/exit@0.3.0",
];

/// What every outcome depends on beyond its own inputs: the running `wado`
/// binary, which links the compiler and wasmtime, and the stdlib it compiles
/// against, which a dev build reads from disk.
///
/// The binary is known by its size and modification time, as ccache knows a
/// compiler by default. Hashing its contents costs seconds for a dev build,
/// and every rebuild changes both anyway.
static COMPILER_DIGEST: LazyLock<[u8; 32]> = LazyLock::new(|| {
    let exe = std::env::current_exe().expect("locating the running `wado` binary");
    let stat = std::fs::metadata(&exe)
        .unwrap_or_else(|e| panic!("reading the running `wado` binary {}: {e}", exe.display()));
    let modified = stat
        .modified()
        .expect("the platform reports a modification time")
        .duration_since(UNIX_EPOCH)
        .expect("the `wado` binary was modified after 1970");
    let mut hasher = Sha256::new();
    hasher.update(stat.len().to_le_bytes());
    hasher.update(modified.as_nanos().to_le_bytes());
    hash_dev_stdlib(&mut hasher);
    hasher.finalize().into()
});

#[cfg(debug_assertions)]
fn hash_dev_stdlib(hasher: &mut Sha256) {
    wado_lsp::host::install_dev_stdlib();
    for (file, source) in wado_compiler::stdlib::installed_dev_stdlib() {
        hash_field(hasher, file.as_bytes());
        hash_field(hasher, source.as_bytes());
    }
}

/// A release build embeds the stdlib, so rebuilding the binary is the only way
/// to change it.
#[cfg(not(debug_assertions))]
fn hash_dev_stdlib(_hasher: &mut Sha256) {}

/// Length-prefixed, so no two sequences of fields hash alike.
fn hash_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// The `core:eval` host for one `wado test` run, shared by every test in it.
pub struct EvalHost {
    knobs: CompileKnobs,
    engine: OnceLock<(Engine, Linker<Program>)>,
    /// One slot per key being evaluated, so calls sharing a key at once
    /// evaluate once. A resolved slot leaves, and a later call reads the cache.
    slots: Mutex<IndexMap<[u8; 32], Arc<OnceCell<Outcome>>>>,
}

impl EvalHost {
    /// A host compiling at the test's `-O` and with its `-f` flags. `--no-cache`
    /// skips reading the outcome cache.
    #[must_use]
    pub fn new(knobs: &CompileKnobs) -> Self {
        Self {
            knobs: knobs.clone(),
            engine: OnceLock::new(),
            slots: Mutex::new(IndexMap::default()),
        }
    }

    fn key(&self, source: &str, fuel: u64) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(*COMPILER_DIGEST);
        hash_field(
            &mut hasher,
            format!("{:?}", self.knobs.opt_level).as_bytes(),
        );
        hasher.update((self.knobs.codegen_flags.len() as u64).to_le_bytes());
        for flag in &self.knobs.codegen_flags {
            hash_field(&mut hasher, flag.as_bytes());
        }
        hasher.update(fuel.to_le_bytes());
        hasher.update((MEMORY_CEILING as u64).to_le_bytes());
        hash_field(&mut hasher, source.as_bytes());
        hasher.finalize().into()
    }

    async fn outcome(self: &Arc<Self>, caller: &Path, source: String, fuel: u64) -> Outcome {
        let key = self.key(&source, fuel);
        let slot = Arc::clone(lock(&self.slots).entry(key).or_default());
        let outcome = slot
            .get_or_init(|| self.cached_or_evaluate(key, caller, source, fuel))
            .await
            .clone();
        let mut slots = lock(&self.slots);
        // A waiter leaving late must not evict a newer slot for the same key.
        if slots.get(&key).is_some_and(|live| Arc::ptr_eq(live, &slot)) {
            slots.swap_remove(&key);
        }
        outcome
    }

    async fn cached_or_evaluate(
        self: &Arc<Self>,
        key: [u8; 32],
        caller: &Path,
        source: String,
        fuel: u64,
    ) -> Outcome {
        let path = cache_dir(caller).join(format!("{}.json", hex32(&key)));
        if !self.knobs.no_cache
            && let Some(outcome) = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        {
            return outcome;
        }
        let outcome = self.evaluate(source, fuel).await;
        if !matches!(outcome, Outcome::CompileTimedOut) {
            let bytes = serde_json::to_vec(&outcome).expect("an outcome serializes");
            // Losing the cache is never an error: the next run evaluates again.
            let _ = write_atomic(&path, &bytes);
        }
        outcome
    }

    async fn evaluate(self: &Arc<Self>, source: String, fuel: u64) -> Outcome {
        let wasm = match self.compile(source).await {
            Compiled::Wasm(wasm) => wasm,
            Compiled::Failed(failure) => return Outcome::CompileFailed(failure),
            Compiled::TimedOut => return Outcome::CompileTimedOut,
        };
        let host = Arc::clone(self);
        // The AOT compile takes seconds, so it runs off the async workers, on a
        // runtime of its own as the compile does.
        tokio::task::spawn_blocking(move || {
            let (engine, linker) = host.engine();
            current_thread_runtime().block_on(run(engine, linker, &wasm, fuel))
        })
        .await
        .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()))
    }

    /// The compile's future is `!Send`, so it runs on a thread of its own. Past
    /// the limit that thread is abandoned, not stopped: nothing can interrupt a
    /// compile. It is not one of the runtime's blocking threads, which the
    /// runtime would wait for when `wado test` shuts it down.
    ///
    /// A panic on either thread `evaluate` starts is a bug in the compiler or
    /// the host, so it carries on into the calling test, which reports it.
    async fn compile(&self, source: String) -> Compiled {
        let options = CompilerOptions {
            opt_level: self.knobs.opt_level.to_compiler(),
            codegen_flags: self.knobs.codegen_flags.clone(),
            ..CompilerOptions::default()
        };
        let (report, compiled) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("eval-compile".to_string())
            .stack_size(COMPILER_STACK_SIZE)
            .spawn(move || {
                let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    // A host with no sources: the program is one module, and
                    // nothing on the host's disk is read.
                    let host = InMemoryCompilerHost::new();
                    let compiled =
                        current_thread_runtime().block_on(wado_compiler::compile_with_options(
                            &source,
                            &host,
                            Some(EVAL_FILE),
                            options,
                        ));
                    match compiled {
                        Ok(result) => Compiled::Wasm(result.wasm),
                        Err(_) => Compiled::Failed(compile_failure(&host.diagnostics())),
                    }
                }));
                // Past the limit nobody is listening, and the outcome is dropped.
                let _ = report.send(outcome);
            })
            .expect("spawning the eval compile thread");
        match tokio::time::timeout(COMPILE_TIME_LIMIT, compiled).await {
            Ok(outcome) => outcome
                .expect("the compile thread reports before it exits")
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
            Err(_) => Compiled::TimedOut,
        }
    }

    fn engine(&self) -> &(Engine, Linker<Program>) {
        self.engine.get_or_init(|| {
            let engine = create_fuel_engine(self.knobs.opt_level.to_wasmtime())
                .expect("building the eval engine");
            let linker = program_linker(&engine).expect("linking the eval host");
            (engine, linker)
        })
    }
}

enum Compiled {
    Wasm(Vec<u8>),
    Failed(CompileFailure),
    TimedOut,
}

fn compile_failure(diagnostics: &[Diagnostic]) -> CompileFailure {
    CompileFailure {
        rendered: render_error_diagnostics(diagnostics).expect("a failed compile reports an error"),
        codes: diagnostics
            .iter()
            .filter(|d| matches!(d.severity, Severity::Error | Severity::Fatal))
            .map(|d| d.code.to_string())
            .collect(),
    }
}

/// Outcomes live in `build/eval/` under the calling file's package root, or
/// under its directory when it is in no package.
fn cache_dir(caller: &Path) -> PathBuf {
    let root = load_nearest_manifest(caller).map_or_else(
        || {
            caller
                .parent()
                .expect("the caller is a file, so it has a parent")
                .to_path_buf()
        },
        |project| project.root,
    );
    build_dir(&root).join("eval")
}

fn current_thread_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("building a current-thread runtime")
}

/// The store data of an evaluated program.
struct Program {
    ctx: WasiCtx,
    table: ResourceTable,
    ceiling: Ceiling,
}

impl WasiView for Program {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

/// Stops a program that would grow a memory past [`MEMORY_CEILING`] or a table
/// past [`TABLE_CEILING`]. It fails the growth rather than denying it, so the
/// program stops on an error that [`stopped`] can name.
struct Ceiling;

#[derive(Debug)]
struct CeilingReached;

impl std::fmt::Display for CeilingReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("grew past the eval ceiling")
    }
}

impl std::error::Error for CeilingReached {}

fn within(desired: usize, ceiling: usize) -> wasmtime::Result<bool> {
    if desired > ceiling {
        return Err(CeilingReached.into());
    }
    Ok(true)
}

impl ResourceLimiter for Ceiling {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        within(desired, MEMORY_CEILING)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        within(desired, TABLE_CEILING)
    }
}

fn program_linker(engine: &Engine) -> wasmtime::Result<Linker<Program>> {
    let mut linker = Linker::new(engine);
    stdout::add_to_linker::<_, WasiCli>(&mut linker, Program::cli)?;
    stderr::add_to_linker::<_, WasiCli>(&mut linker, Program::cli)?;
    exit::add_to_linker::<_, WasiCli>(&mut linker, Program::cli)?;
    Ok(linker)
}

async fn run(engine: &Engine, linker: &Linker<Program>, wasm: &[u8], fuel: u64) -> Outcome {
    let component = Component::new(engine, wasm)
        .unwrap_or_else(|e| panic!("wasmtime rejected the compiler's output: {e:#}"));
    if let Some((name, _)) = component
        .component_type()
        .imports(engine)
        .find(|(name, _)| !LINKED_INTERFACES.contains(name))
    {
        return Outcome::Unavailable(name.to_string());
    }

    let stdout = MemoryOutputPipe::new(CAPTURED_STDIO_CAPACITY);
    let stderr = MemoryOutputPipe::new(CAPTURED_STDIO_CAPACITY);
    let mut builder = WasiCtx::builder();
    builder.stdout(stdout.clone());
    builder.stderr(stderr.clone());
    let mut store = Store::new(
        engine,
        Program {
            ctx: builder.build(),
            table: ResourceTable::new(),
            ceiling: Ceiling,
        },
    );
    store.limiter(|program| &mut program.ceiling);
    store.set_fuel(fuel).expect("the eval engine consumes fuel");

    let status = match Command::instantiate_async(&mut store, &component, linker).await {
        Ok(command) => {
            let ran = store
                .run_concurrent(async |accessor| command.wasi_cli_run().call_run(accessor).await)
                .await;
            match ran {
                Ok(Ok(Ok(()))) => Status::Exited(0),
                // How a command reports failure without calling exit; wasmtime's
                // own CLI exits with 1 on it.
                Ok(Ok(Err(()))) => Status::Exited(1),
                Ok(Err(e)) | Err(e) => stopped(&e),
            }
        }
        Err(e) => stopped(&e),
    };
    Outcome::Ran(Ran {
        stdout: stdout.contents().to_vec(),
        stderr: stderr.contents().to_vec(),
        status,
    })
}

/// How a program that did not return from `run` stopped.
fn stopped(error: &wasmtime::Error) -> Status {
    if let Some(I32Exit(code)) = error.downcast_ref::<I32Exit>() {
        return Status::Exited(*code);
    }
    // The GC heap swallows a failed growth and reports the allocation instead.
    if error.is::<CeilingReached>() || error.is::<GcHeapOutOfMemory<()>>() {
        return Status::OutOfMemory;
    }
    match error.downcast_ref::<Trap>() {
        Some(Trap::OutOfFuel) => Status::OutOfFuel,
        Some(trap) => Status::Trapped(trap_kind(*trap)),
        None => Status::Trapped(TrapKind::Other),
    }
}

fn trap_kind(trap: Trap) -> TrapKind {
    match trap {
        Trap::UnreachableCodeReached => TrapKind::Unreachable,
        Trap::StackOverflow => TrapKind::StackOverflow,
        Trap::MemoryOutOfBounds => TrapKind::MemoryOutOfBounds,
        Trap::TableOutOfBounds => TrapKind::TableOutOfBounds,
        Trap::IndirectCallToNull => TrapKind::IndirectCallToNull,
        Trap::BadSignature => TrapKind::BadSignature,
        Trap::IntegerOverflow => TrapKind::IntegerOverflow,
        Trap::IntegerDivisionByZero => TrapKind::IntegerDivisionByZero,
        Trap::BadConversionToInteger => TrapKind::BadConversionToInteger,
        Trap::NullReference => TrapKind::NullReference,
        Trap::ArrayOutOfBounds => TrapKind::ArrayOutOfBounds,
        Trap::AllocationTooLarge => TrapKind::AllocationTooLarge,
        Trap::CastFailure => TrapKind::CastFailure,
        _ => TrapKind::Other,
    }
}

/// One test's handle on the run's [`EvalHost`]: which file is calling, and how
/// long it has spent inside `eval`.
#[derive(Clone)]
pub struct EvalSession {
    host: Arc<EvalHost>,
    caller: Arc<Path>,
    paused: Duration,
}

impl EvalSession {
    /// A session for a test in the file at `caller`.
    #[must_use]
    pub fn new(host: Arc<EvalHost>, caller: &str) -> Self {
        Self {
            host,
            caller: Arc::from(Path::new(caller)),
            paused: Duration::ZERO,
        }
    }

    /// The time spent inside `eval` so far, which does not count against the
    /// test's deadline.
    #[must_use]
    pub fn paused(&self) -> Duration {
        self.paused
    }
}

impl eval_host::Host for EvalSession {
    async fn run(&mut self, source: String, fuel: u64) -> Outcome {
        let start = Instant::now();
        let outcome = self.host.outcome(&self.caller, source, fuel).await;
        self.paused += start.elapsed();
        outcome
    }
}

/// Link `core:eval` into a test linker, reaching each store's session through
/// `session`.
///
/// # Errors
///
/// Returns an error if the linker already defines the interface.
pub fn add_to_linker<T: Send + 'static>(
    linker: &mut Linker<T>,
    session: fn(&mut T) -> &mut EvalSession,
) -> wasmtime::Result<()> {
    eval_host::add_to_linker::<T, HasSelf<EvalSession>>(linker, session)
}
