//! A two-level `extends` program run against a host implementing the `web:dom`
//! imports. See `docs/wep-2026-04-28-resource-inheritance.md`.

use std::sync::{Arc, Mutex};

use wasmtime::Store;
use wasmtime::StoreContextMut;
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
    let el = doc.create_element("div", null);
    el.set_id("app");
    el.set_text_content(Option::Some("hello"));

    assert el.tag_name() == "div";
    assert el.id() == "app";

    // Declared on `Node`, called on an `Element`.
    assert el.text_content() == Option::Some("hello");

    // The upcast converts nothing: the same handle answers as a `Node`.
    let parent: Node = el;
    assert parent.text_content() == Option::Some("hello");

    // Declared on `EventTarget`, two links above `Element`.
    let ev = Event::new("click");
    assert el.dispatch_event(ev);

    // A handle both taken as a non-receiver argument and handed back.
    let child = doc.create_element("span", null);
    child.set_text_content(Option::Some("world"));
    assert parent.append_child(child).text_content() == Option::Some("world");

    // An optional handle: the host answers with the element's own handle, or none.
    let found = doc.get_element_by_id("app");
    assert found matches { Some(_) };
    if let Some(same) = found {
        assert same.id() == "app";
    }
    assert doc.get_element_by_id("missing") matches { None };
}
"#;

/// One host object per handle. The table is the whole host model: a handle is
/// an index into it, so the guest passing the same index twice reaches the
/// same object — which is what an upcast has to preserve.
#[derive(Default)]
struct DomObjects {
    objects: Vec<Object>,
    dispatched: Vec<(u32, String)>,
}

/// Every field a `web:dom` method reads, on whichever object carries it.
#[derive(Default)]
struct Object {
    tag: String,
    id: String,
    text: String,
    event_type: String,
}

impl DomObjects {
    fn insert(&mut self, object: Object) -> u32 {
        self.objects.push(object);
        u32::try_from(self.objects.len() - 1).expect("the stub never grows past u32")
    }

    fn at(&mut self, handle: u32) -> &mut Object {
        let index = usize::try_from(handle).expect("a handle indexes the table");
        self.objects
            .get_mut(index)
            .unwrap_or_else(|| panic!("the guest passed handle {handle}, which names no object"))
    }
}

/// A host binding over the object table, so each body below is only its own
/// work. `Params` starts with the handle as a plain `u32` — the shape the
/// extern-handle lowering produces, with no CM resource anywhere in it.
fn over_dom<Params, Return>(
    dom: &Arc<Mutex<DomObjects>>,
    body: impl Fn(&mut DomObjects, Params) -> Return + Send + Sync + 'static,
) -> impl Fn(StoreContextMut<'_, WasiState>, Params) -> wasmtime::Result<Return> + Send + Sync + 'static
{
    let dom = Arc::clone(dom);
    move |_, params| Ok(body(&mut dom.lock().unwrap(), params))
}

/// Bind every `web:dom` interface the program imports.
fn add_dom_to_linker(
    linker: &mut Linker<WasiState>,
    dom: &Arc<Mutex<DomObjects>>,
) -> anyhow::Result<()> {
    linker.instance("web:dom/global")?.func_wrap(
        "document",
        over_dom(dom, |dom, ()| (dom.insert(Object::default()),)),
    )?;

    let mut document = linker.instance("web:dom/document")?;
    document.func_wrap(
        "create-element",
        over_dom(
            dom,
            |dom, (_self, local_name, _options): (u32, String, Option<String>)| {
                (dom.insert(Object {
                    tag: local_name,
                    ..Object::default()
                }),)
            },
        ),
    )?;
    document.func_wrap(
        "get-element-by-id",
        over_dom(dom, |dom, (_self, id): (u32, String)| {
            let found = dom.objects.iter().position(|o| o.id == id);
            (found.map(|index| u32::try_from(index).expect("a table index")),)
        }),
    )?;

    let mut element = linker.instance("web:dom/element")?;
    element.func_wrap(
        "tag-name",
        over_dom(dom, |dom, (handle,): (u32,)| (dom.at(handle).tag.clone(),)),
    )?;
    element.func_wrap(
        "id",
        over_dom(dom, |dom, (handle,): (u32,)| (dom.at(handle).id.clone(),)),
    )?;
    element.func_wrap(
        "set-id",
        over_dom(dom, |dom, (handle, value): (u32, String)| {
            dom.at(handle).id = value;
        }),
    )?;

    let mut node = linker.instance("web:dom/node")?;
    node.func_wrap(
        "text-content",
        over_dom(dom, |dom, (handle,): (u32,)| {
            (Some(dom.at(handle).text.clone()),)
        }),
    )?;
    node.func_wrap(
        "set-text-content",
        over_dom(dom, |dom, (handle, value): (u32, Option<String>)| {
            dom.at(handle).text = value.unwrap_or_default();
        }),
    )?;
    node.func_wrap("append-child", |_, (_parent, child): (u32, u32)| {
        Ok((child,))
    })?;

    linker.instance("web:dom/event")?.func_wrap(
        "new",
        over_dom(dom, |dom, (event_type,): (String,)| {
            (dom.insert(Object {
                event_type,
                ..Object::default()
            }),)
        }),
    )?;

    linker.instance("web:dom/event-target")?.func_wrap(
        "dispatch-event",
        over_dom(dom, |dom, (target, event): (u32, u32)| {
            let event_type = dom.at(event).event_type.clone();
            dom.dispatched.push((target, event_type));
            (true,)
        }),
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
    // `document`, the element, the event, and the child.
    assert_eq!(dom.objects.len(), 4);
    assert_eq!(dom.objects[1].tag, "div");
    assert_eq!(dom.objects[1].id, "app");
    assert_eq!(dom.objects[1].text, "hello");
    // The event reached the element's own handle, not a re-minted one.
    assert_eq!(dom.dispatched, vec![(1, "click".to_string())]);
}
