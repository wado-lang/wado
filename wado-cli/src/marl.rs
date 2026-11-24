//! Marl's `format`, which gives `wado doc`'s Markdown its final layout.
//!
//! `wado` carries Marl as source and compiles it on first use. A release build
//! keeps the compiled component in the user cache, keyed by the compiler and
//! the sources, so a run never writes beside the files it documents. A dev
//! build rebuilds the binary on every edit, which would leave a stale entry per
//! rebuild, so it compiles once per process and keeps nothing.

use std::panic::resume_unwind;
use std::path::PathBuf;
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
use crate::kiln_provider::hex32;

macro_rules! marl_sources {
    ($($name:literal),* $(,)?) => {
        &[$((concat!("./", $name), include_str!(concat!("../../package-marl/src/", $name)))),*]
    };
}

/// Every `.wado` file of `package-marl/src`, as `("./name.wado", source)`.
const SOURCES: &[(&str, &str)] = marl_sources![
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

/// Where the user cache keeps the compiled component.
const CACHE_KIND: &str = "marl";

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
    if cfg!(debug_assertions) {
        return compile_component(engine);
    }
    let Some(path) = cache_path() else {
        return compile_component(engine);
    };
    // SAFETY: the file was serialized by this binary, which its name keys
    // through `compiler_digest`, into the user's own cache; wasmtime still
    // refuses one from an incompatible engine.
    if let Ok(component) = unsafe { Component::deserialize_file(engine, &path) } {
        return component;
    }
    let component = compile_component(engine);
    let bytes = component
        .serialize()
        .unwrap_or_else(|e| panic!("serializing the bundled Marl: {e:#}"));
    // Losing the entry only costs the next run a compile. What another `wado`
    // cached stays: it may be in use, and `wado clean` clears the user cache.
    let _ = write_atomic(&path, &bytes);
    component
}

/// The cache entry for this compiler and these sources.
fn cache_path() -> Option<PathBuf> {
    let mut hasher = Sha256::new();
    hasher.update(compiler_digest());
    for (name, source) in SOURCES {
        hash_field(&mut hasher, name.as_bytes());
        hash_field(&mut hasher, source.as_bytes());
    }
    let key: [u8; 32] = hasher.finalize().into();
    user_cache_dir().map(|dir| dir.join(CACHE_KIND).join(format!("{}.cwasm", hex32(&key))))
}

fn compile_component(engine: &Engine) -> Component {
    install_dev_stdlib();
    let entry = SOURCES
        .iter()
        .find(|(name, _)| *name == ENTRY)
        .map(|(_, source)| *source)
        .expect("Marl has a format.wado");
    let host = InMemoryCompilerHost::with_files(SOURCES);
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
    use std::path::Path;

    #[test]
    fn formats_markdown() {
        assert_eq!(format("# Title\n* a\n* b\n"), "# Title\n\n- a\n- b\n");
    }

    #[test]
    fn sources_list_every_marl_file() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../package-marl/src");
        let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
            .expect("reading package-marl/src")
            .map(|entry| entry.expect("listing package-marl/src").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "wado"))
            .map(|path| format!("./{}", path.file_name().unwrap().display()))
            .collect();
        on_disk.sort();
        let listed: Vec<&str> = SOURCES.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            listed, on_disk,
            "SOURCES must list package-marl/src, sorted"
        );
    }
}
