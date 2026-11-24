//! Marl's `format`, which gives `wado doc`'s Markdown its final layout.
//!
//! `wado` compiles Marl from source on first use and caches the component in
//! one file, keyed by the compiler and the sources, never beside the files it
//! documents. A self-contained build (the `dist` feature) embeds the sources
//! and caches in the user cache. Any other reads the checkout's sources, as it
//! does the stdlib, and caches in cargo's profile directory, so each profile
//! keeps its own and `cargo clean` removes it.

use std::panic::resume_unwind;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sha2::{Digest, Sha256};
use wasmtime::component::{
    Component, ComponentExportIndex, InstancePre, Linker, ResourceTable, TypedFunc,
};
use wasmtime::{Engine, OptLevel as CraneliftOptLevel};
use wasmtime_wasi::cli::{WasiCli, WasiCliView};
use wasmtime_wasi::p3::bindings::cli::stderr;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use wado_compiler::{CompilerOptions, InMemoryCompilerHost, OptLevel};
use wado_host::atomic_write::write_atomic;
use wado_host::engine::{default_config, guest_store};
use wado_host::random;
use wado_lsp::host::install_dev_stdlib;

use crate::COMPILER_STACK_SIZE;
use crate::cache::user_cache_dir;
use crate::compile::render_error_diagnostics;
use crate::eval_host::current_thread_runtime;
use crate::fingerprint::{compiler_digest, hash_field};

/// The checkout's Marl sources, which a build without `dist` reads.
#[cfg(any(test, not(feature = "dist")))]
const MARL_SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../package-marl/src");

macro_rules! embedded_marl {
    ($($name:literal),* $(,)?) => {
        #[cfg(test)]
        const EMBEDDED: &[&str] = &[$($name),*];

        /// Every `.wado` file of `package-marl/src`, as `("./name.wado", source)`.
        #[cfg(feature = "dist")]
        fn sources() -> &'static [(&'static str, &'static str)] {
            &[$((concat!("./", $name), include_str!(concat!("../../package-marl/src/", $name)))),*]
        }
    };
}

/// Every `.wado` file of [`MARL_SRC`], read once per process, so an edit to
/// Marl needs no rebuild and a new file no list.
#[cfg(not(feature = "dist"))]
fn sources() -> &'static [(&'static str, &'static str)] {
    static SOURCES: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();
    SOURCES.get_or_init(|| {
        fn read<T>(e: std::io::Error) -> T {
            panic!("reading Marl at {MARL_SRC}: {e}")
        }
        let mut sources: Vec<(&'static str, &'static str)> = std::fs::read_dir(MARL_SRC)
            .unwrap_or_else(read)
            .map(|entry| entry.unwrap_or_else(read).path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "wado"))
            .map(|path| {
                let name = format!("./{}", path.file_name().unwrap().display());
                let source = std::fs::read_to_string(&path).unwrap_or_else(read);
                (String::leak(name) as &str, String::leak(source) as &str)
            })
            .collect();
        sources.sort_unstable();
        sources
    })
}

embedded_marl![
    "code_blocks.wado",
    "fmt_ast.wado",
    "fmt_html.wado",
    "fmt_links.wado",
    "fmt_parse_block.wado",
    "fmt_parse_inline.wado",
    "fmt_print.wado",
    "fmt_slug.wado",
    "format.wado",
    "html.wado",
    "lib.wado",
    "main.wado",
    "str_cursor.wado",
    "unicode_width.wado",
];

const ENTRY: &str = "./format.wado";

/// The library interface the component exports `format` in.
const INTERFACE: &str = "wado-lang:marl/marl@0.0.0";

/// Formats `markdown` as Marl's canonical layout.
#[must_use]
pub fn format(markdown: &str) -> String {
    // A thread of its own: compiling Marl takes the compiler's stack, and the
    // call blocks on a runtime, which a caller already inside one cannot.
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("marl".to_string())
            .stack_size(COMPILER_STACK_SIZE)
            .spawn_scoped(scope, || {
                let formatter = FORMATTER.get_or_init(Formatter::load);
                current_thread_runtime().block_on(formatter.format(markdown))
            })
            .expect("spawning the Marl thread")
            .join()
            .unwrap_or_else(|panic| resume_unwind(panic))
    })
}

static FORMATTER: OnceLock<Formatter> = OnceLock::new();

struct Formatter {
    engine: Engine,
    pre: InstancePre<Guest>,
    format: ComponentExportIndex,
}

impl Formatter {
    fn load() -> Self {
        let engine = Engine::new(&default_config(CraneliftOptLevel::Speed))
            .unwrap_or_else(|e| panic!("wasmtime rejected the default configuration: {e:#}"));
        let component = load_component(&engine);
        let interface = component
            .get_export_index(None, INTERFACE)
            .expect("the bundled Marl exports its interface");
        let format = component
            .get_export_index(Some(&interface), "format")
            .expect("the bundled Marl exports `format`");
        let mut linker = Linker::new(&engine);
        stderr::add_to_linker::<_, WasiCli>(&mut linker, Guest::cli)
            .expect("linking wasi:cli/stderr");
        random::add_to_linker(&mut linker).expect("linking wasi:random");
        let pre = linker
            .instantiate_pre(&component)
            .unwrap_or_else(|e| panic!("the bundled Marl imports what is linked: {e:#}"));
        Self {
            engine,
            pre,
            format,
        }
    }

    async fn format(&self, markdown: &str) -> String {
        let mut builder = WasiCtx::builder();
        random::fixed_seed(&mut builder);
        builder.inherit_stderr();
        let mut store = guest_store(
            &self.engine,
            Guest {
                ctx: builder.build(),
                table: ResourceTable::new(),
            },
        );
        let instance = self
            .pre
            .instantiate_async(&mut store)
            .await
            .unwrap_or_else(|e| panic!("instantiating the bundled Marl: {e:#}"));
        let format: TypedFunc<(String,), (String,)> = instance
            .get_typed_func(&mut store, self.format)
            .unwrap_or_else(|e| panic!("`format` takes and returns a string: {e:#}"));
        let (formatted,) = format
            .call_async(&mut store, (markdown.to_string(),))
            .await
            .unwrap_or_else(|e| panic!("the bundled Marl failed to format: {e:#}"));
        formatted
    }
}

fn load_component(engine: &Engine) -> Component {
    let Some(path) = cache_path() else {
        return compile_component(engine);
    };
    let key = cache_key();
    if let Ok(cached) = std::fs::read(&path)
        && let Some(serialized) = cached.strip_prefix(key.as_slice())
        // SAFETY: the key proves this binary serialized it, into a directory
        // of the user's own; wasmtime still refuses an incompatible engine's.
        && let Ok(component) = unsafe { Component::deserialize(engine, serialized) }
    {
        return component;
    }
    let component = compile_component(engine);
    let serialized = component
        .serialize()
        .unwrap_or_else(|e| panic!("serializing the bundled Marl: {e:#}"));
    // One file, replaced on a miss, so stale entries never pile up. Losing it
    // only costs the next run a compile.
    let _ = write_atomic(&path, &[key.as_slice(), &serialized].concat());
    component
}

/// What the compiled component depends on: this compiler and these sources.
fn cache_key() -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(compiler_digest());
    for (name, source) in sources() {
        hash_field(&mut hasher, name.as_bytes());
        hash_field(&mut hasher, source.as_bytes());
    }
    hasher.finalize().into()
}

fn cache_path() -> Option<PathBuf> {
    let dir = if cfg!(feature = "dist") {
        user_cache_dir()
    } else {
        cargo_profile_dir().or_else(user_cache_dir)
    };
    dir.map(|dir| dir.join("marl.cwasm"))
}

/// The cargo profile directory (`target/debug`) the running binary was built
/// into, which cargo marks with its `.cargo-lock`; `None` for a binary copied
/// out of it, which caches as a distributed one does.
fn cargo_profile_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .find(|dir| dir.join(".cargo-lock").is_file())
        .map(Path::to_path_buf)
}

fn compile_component(engine: &Engine) -> Component {
    install_dev_stdlib();
    let entry = sources()
        .iter()
        .find(|(name, _)| *name == ENTRY)
        .map(|(_, source)| *source)
        .expect("Marl has a format.wado");
    let host = InMemoryCompilerHost::with_files(sources());
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        lib_world: Some(INTERFACE.to_string()),
        ..CompilerOptions::default()
    };
    let compiled = current_thread_runtime()
        .block_on(wado_compiler::compile_with_options(
            entry,
            &host,
            Some(ENTRY),
            options,
        ))
        .unwrap_or_else(|_| {
            panic!(
                "the bundled Marl failed to compile:\n{}",
                render_error_diagnostics(&host.diagnostics())
                    .expect("a failed compile reports an error")
            )
        });
    Component::new(engine, &compiled.wasm)
        .unwrap_or_else(|e| panic!("wasmtime rejected the bundled Marl: {e:#}"))
}

/// The store data of a Marl instance.
struct Guest {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiView for Guest {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_markdown() {
        assert_eq!(format("# Title\n* a\n* b\n"), "# Title\n\n- a\n- b\n");
    }

    #[cfg(not(feature = "dist"))]
    #[test]
    fn a_build_from_cargo_caches_in_its_profile_directory() {
        assert_eq!(format("x\n"), "x\n");
        let path = cache_path().expect("a test binary has somewhere to cache");
        assert!(path.is_file(), "{} was not written", path.display());
        let profile = cargo_profile_dir().expect("a test binary runs from its profile directory");
        assert!(path.starts_with(&profile), "{}", path.display());
    }

    #[test]
    fn dist_embeds_every_marl_file() {
        let mut on_disk: Vec<String> = std::fs::read_dir(MARL_SRC)
            .expect("reading package-marl/src")
            .map(|entry| entry.expect("listing package-marl/src").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "wado"))
            .map(|path| path.file_name().unwrap().display().to_string())
            .collect();
        on_disk.sort();
        assert_eq!(
            EMBEDDED, on_disk,
            "embedded_marl! must list package-marl/src, sorted"
        );
    }
}
