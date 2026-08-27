//! A two-level `extends` program run against a host that implements the
//! `web:dom` imports over a handle table. Proves the whole chain end to end:
//! the handle crosses as one opaque value, an upcast converts nothing, and a
//! method inherited from a grandparent reaches the interface that declares it.
//! See `docs/wep-2026-04-28-resource-inheritance.md`.

use std::sync::{Arc, Mutex};

use wasmtime::Store;
use wasmtime::component::{Component, Linker};
use wasmtime_wasi::ResourceTable;
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi_http::WasiHttpCtx;

use crate::common::{
    self, DEFAULT_TIMEOUT_MS, TestHttpCtx, WasiState, build_tls_ctx, compile_source, engine,
    limit_store, runtime,
};

/// `Element` is two `extends` links below `EventTarget`, so `dispatch_event`
/// resolves through the whole chain while `text_content` resolves through one.
const PROGRAM: &str = r#"
use { Dom, Event, EventTarget, Node } from "web:dom";

export fn run() with (Dom, Event, EventTarget) {
    let doc = Dom::document();
    let el = doc.create_element("div");
    el.set_id("app");
    el.set_text_content("hello");

    assert el.tag_name() == "div";
    assert el.id() == "app";

    // Declared on `Node`, called on an `Element`.
    assert el.text_content() == "hello";

    // The upcast converts nothing: the same handle answers as a `Node`.
    let parent: Node = el;
    assert parent.text_content() == "hello";

    // Declared on `EventTarget`, two links above `Element`.
    let ev = Event::new("click");
    assert el.dispatch_event(&ev);
}
"#;

/// One host object per handle. The table is the whole host model: a handle is
/// an index into it, so the guest passing the same index twice reaches the
/// same object — which is what an upcast has to preserve.
#[derive(Default)]
struct DomObjects {
    /// `(tag, id, text)` per element; a non-element object leaves them empty.
    objects: Vec<Node>,
    dispatched: Vec<(u32, String)>,
}

#[derive(Default, Clone)]
struct Node {
    tag: String,
    id: String,
    text: String,
    event_type: String,
}

impl DomObjects {
    fn insert(&mut self, node: Node) -> u32 {
        self.objects.push(node);
        u32::try_from(self.objects.len() - 1).expect("the stub never grows past u32")
    }

    fn at(&mut self, handle: u32) -> &mut Node {
        let index = usize::try_from(handle).expect("a handle indexes the table");
        self.objects
            .get_mut(index)
            .unwrap_or_else(|| panic!("the guest passed handle {handle}, which names no object"))
    }
}

/// Bind every `web:dom` interface the program imports. Each closure takes the
/// handle as an ordinary `u32` first parameter — the shape the extern-handle
/// lowering produces, with no CM resource anywhere in it.
fn add_dom_to_linker(
    linker: &mut Linker<WasiState>,
    dom: &Arc<Mutex<DomObjects>>,
) -> anyhow::Result<()> {
    let state = Arc::clone(dom);
    linker
        .instance("web:dom/global")?
        .func_wrap("document", move |_, ()| {
            let handle = state.lock().unwrap().insert(Node::default());
            Ok((handle,))
        })?;

    let state = Arc::clone(dom);
    linker.instance("web:dom/document")?.func_wrap(
        "create-element",
        move |_, (_self_handle, local_name): (u32, String)| {
            let handle = state.lock().unwrap().insert(Node {
                tag: local_name,
                ..Node::default()
            });
            Ok((handle,))
        },
    )?;

    let mut element = linker.instance("web:dom/element")?;
    let state = Arc::clone(dom);
    element.func_wrap("tag-name", move |_, (handle,): (u32,)| {
        Ok((state.lock().unwrap().at(handle).tag.clone(),))
    })?;
    let state = Arc::clone(dom);
    element.func_wrap("id", move |_, (handle,): (u32,)| {
        Ok((state.lock().unwrap().at(handle).id.clone(),))
    })?;
    let state = Arc::clone(dom);
    element.func_wrap("set-id", move |_, (handle, value): (u32, String)| {
        state.lock().unwrap().at(handle).id = value;
        Ok(())
    })?;

    let mut node = linker.instance("web:dom/node")?;
    let state = Arc::clone(dom);
    node.func_wrap("text-content", move |_, (handle,): (u32,)| {
        Ok((state.lock().unwrap().at(handle).text.clone(),))
    })?;
    let state = Arc::clone(dom);
    node.func_wrap(
        "set-text-content",
        move |_, (handle, value): (u32, String)| {
            state.lock().unwrap().at(handle).text = value;
            Ok(())
        },
    )?;
    node.func_wrap("append-child", |_, (_parent, child): (u32, u32)| {
        Ok((child,))
    })?;

    let state_new = Arc::clone(dom);
    linker
        .instance("web:dom/event")?
        .func_wrap("new", move |_, (event_type,): (String,)| {
            let handle = state_new.lock().unwrap().insert(Node {
                event_type,
                ..Node::default()
            });
            Ok((handle,))
        })?;

    let state = Arc::clone(dom);
    linker.instance("web:dom/event-target")?.func_wrap(
        "dispatch-event",
        move |_, (target, event): (u32, u32)| {
            let mut dom = state.lock().unwrap();
            let event_type = dom.at(event).event_type.clone();
            dom.dispatched.push((target, event_type));
            Ok((true,))
        },
    )?;
    Ok(())
}

#[test]
fn a_two_level_extends_program_runs_against_a_host_stub() {
    common::install_rustls_provider_for_tests();
    let wasm = compile_source(PROGRAM)
        .unwrap_or_else(|e| panic!("the web:dom program should compile, got {e}"))
        .wasm;

    let dom = Arc::new(Mutex::new(DomObjects::default()));
    let engine = engine();
    let stderr = MemoryOutputPipe::new(65536);
    let stderr_reader = stderr.clone();

    runtime()
        .block_on(async {
            let component = Component::new(engine, &wasm)?;
            let mut linker = common::linker(engine)?;
            add_dom_to_linker(&mut linker, &dom)?;

            let mut builder = WasiCtxBuilder::new();
            builder.stderr(stderr);
            let state = WasiState {
                ctx: builder.build(),
                table: ResourceTable::new(),
                http_ctx: WasiHttpCtx::new(),
                http_hooks: TestHttpCtx {
                    mocks: indexmap::IndexMap::default(),
                },
                tls_ctx: build_tls_ctx(indexmap::IndexMap::default()),
            };
            let mut store = Store::new(engine, state);
            limit_store(&mut store, DEFAULT_TIMEOUT_MS);

            let command = wasmtime_wasi::p3::bindings::Command::instantiate_async(
                &mut store, &component, &linker,
            )
            .await?;
            store
                .run_concurrent(async |accessor| command.wasi_cli_run().call_run(accessor).await)
                .await??
                .map_err(|()| anyhow::anyhow!("run() returned an error"))
        })
        .unwrap_or_else(|e| {
            let log = String::from_utf8_lossy(&stderr_reader.contents()).to_string();
            panic!("the program should run: {e:#}\n{log}");
        });

    let dom = dom.lock().unwrap();
    // `document`, the element, and the event — three distinct objects.
    assert_eq!(dom.objects.len(), 3);
    assert_eq!(dom.objects[1].tag, "div");
    assert_eq!(dom.objects[1].id, "app");
    assert_eq!(dom.objects[1].text, "hello");
    // The event reached the element's own handle, not a re-minted one.
    assert_eq!(dom.dispatched, vec![(1, "click".to_string())]);
}
