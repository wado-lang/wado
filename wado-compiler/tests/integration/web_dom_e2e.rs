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
    self, DEFAULT_TIMEOUT_MS, TestHttpCtx, WasiState, build_tls_ctx, compile_against_web, engine,
    limit_store, runtime,
};

/// `Element` is two `extends` links below `EventTarget`, so `dispatch_event`
/// resolves through the whole chain while `text_content` resolves through one.
const PROGRAM: &str = r#"
use { Dom, Event, EventTarget, Node } from "wado-lang:web";

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

    // An optional handle argument, absent and present.
    assert !parent.contains(null);
    let child_node: Node = child;
    assert parent.contains(Option::Some(child_node));
}
"#;

/// Type patterns narrow a handle by the class the host tagged it with.
const NARROWING_PROGRAM: &str = r#"
use { Dom, Element, HtmlInputElement, Node } from "wado-lang:web";

fn describe(n: Node) -> String {
    return match n {
        input: HtmlInputElement => `input:${input.value()}`,
        el: Element => `element:${el.tag_name()}`,
        _ => "node",
    };
}

fn input_value(el: Element) -> String {
    let input: HtmlInputElement = el else {
        return "none";
    };
    return input.value();
}

export fn run() with Dom {
    let doc = Dom::document();
    let div = doc.create_element("div", null);
    div.set_id("d");
    let input = doc.create_element("input", null);
    input.set_id("i");

    assert describe(div) == "element:div";
    assert describe(input) == "input:typed";
    assert describe(doc) == "node";

    if let _: HtmlInputElement = div {
        panic("a div is not an input");
    }
    assert input_value(input) == "typed";
    assert input_value(div) == "none";

    assert doc.get_element_by_id("i") matches { Some(_: HtmlInputElement) };
    assert !(doc.get_element_by_id("d") matches { Some(_: HtmlInputElement) });
}
"#;

/// `==` on handles compares what the host interned, across an upcast and on two roots.
const IDENTITY_PROGRAM: &str = r#"
use { Dom, Event, Node } from "wado-lang:web";

export fn run() with (Dom, Event) {
    let doc = Dom::document();
    let div = doc.create_element("div", null);
    div.set_id("d");
    let as_node: Node = div;

    assert div == div;
    assert as_node == div;
    assert div == as_node == div;
    assert div != doc.create_element("div", null);
    assert doc.get_element_by_id("d") matches { Some(found) && found == div };

    let ev = Event::new("click");
    assert ev == ev;
    assert ev != Event::new("click");
}
"#;

/// One host object per handle. The table is the whole host model: a handle is
/// the object's class and its index, `class * 2^37 + index`, so the guest
/// passing the same handle twice reaches the same object — which is what an
/// upcast has to preserve — and `==` on two handles is identity.
#[derive(Default)]
struct DomObjects {
    objects: Vec<Object>,
    dispatched: Vec<(f64, String)>,
}

/// The `classes` numbers `web:dom` declares for the classes the stub mints.
const HTML_ELEMENT: u16 = 3;
const HTML_INPUT_ELEMENT: u16 = 4;
const DOCUMENT: u16 = 5;
const EVENT: u16 = 7;
const CLASS_STRIDE: f64 = 137_438_953_472.0;

/// Every field a `web:dom` method reads, on whichever object carries it.
#[derive(Default)]
struct Object {
    class: u16,
    tag: String,
    id: String,
    text: String,
    event_type: String,
}

fn handle(class: u16, index: usize) -> f64 {
    let index = u32::try_from(index).expect("the stub never grows past u32");
    f64::from(class) * CLASS_STRIDE + f64::from(index)
}

impl DomObjects {
    fn insert(&mut self, object: Object) -> f64 {
        let class = object.class;
        self.objects.push(object);
        handle(class, self.objects.len() - 1)
    }

    fn handle_of(&self, index: usize) -> f64 {
        handle(self.objects[index].class, index)
    }

    fn at(&mut self, handle: f64) -> &mut Object {
        let index = (handle % CLASS_STRIDE) as usize;
        assert!(
            index < self.objects.len() && self.handle_of(index) == handle,
            "the guest passed handle {handle}, which the stub never minted"
        );
        &mut self.objects[index]
    }
}

/// A host binding over the object table, so each body below is only its own
/// work. `Params` starts with the handle as a plain `f64` — the shape the
/// universal-handle lowering produces, with no CM resource anywhere in it.
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
        over_dom(dom, |dom, ()| {
            (dom.insert(Object {
                class: DOCUMENT,
                ..Object::default()
            }),)
        }),
    )?;

    // A `div` is an `HTMLDivElement`, which the slice leaves out, so the host
    // tags it with its nearest ancestor the slice holds.
    let mut document = linker.instance("web:dom/document")?;
    document.func_wrap(
        "create-element",
        over_dom(
            dom,
            |dom, (_self, local_name, _options): (f64, String, Option<String>)| {
                let class = if local_name == "input" {
                    HTML_INPUT_ELEMENT
                } else {
                    HTML_ELEMENT
                };
                (dom.insert(Object {
                    class,
                    tag: local_name,
                    ..Object::default()
                }),)
            },
        ),
    )?;
    document.func_wrap(
        "get-element-by-id",
        over_dom(dom, |dom, (_self, id): (f64, String)| {
            let found = dom.objects.iter().position(|o| o.id == id);
            (found.map(|index| dom.handle_of(index)),)
        }),
    )?;

    let mut element = linker.instance("web:dom/element")?;
    element.func_wrap(
        "tag-name",
        over_dom(dom, |dom, (handle,): (f64,)| (dom.at(handle).tag.clone(),)),
    )?;
    element.func_wrap(
        "id",
        over_dom(dom, |dom, (handle,): (f64,)| (dom.at(handle).id.clone(),)),
    )?;
    element.func_wrap(
        "set-id",
        over_dom(dom, |dom, (handle, value): (f64, String)| {
            dom.at(handle).id = value;
        }),
    )?;

    let mut node = linker.instance("web:dom/node")?;
    node.func_wrap(
        "text-content",
        over_dom(dom, |dom, (handle,): (f64,)| {
            (Some(dom.at(handle).text.clone()),)
        }),
    )?;
    node.func_wrap(
        "set-text-content",
        over_dom(dom, |dom, (handle, value): (f64, Option<String>)| {
            dom.at(handle).text = value.unwrap_or_default();
        }),
    )?;
    node.func_wrap("append-child", |_, (_parent, child): (f64, f64)| {
        Ok((child,))
    })?;
    node.func_wrap("contains", |_, (_parent, other): (f64, Option<f64>)| {
        Ok((other.is_some(),))
    })?;

    linker.instance("web:dom/event")?.func_wrap(
        "new",
        over_dom(dom, |dom, (event_type,): (String,)| {
            (dom.insert(Object {
                class: EVENT,
                event_type,
                ..Object::default()
            }),)
        }),
    )?;

    linker.instance("web:dom/event-target")?.func_wrap(
        "dispatch-event",
        over_dom(dom, |dom, (target, event): (f64, f64)| {
            let event_type = dom.at(event).event_type.clone();
            dom.dispatched.push((target, event_type));
            (true,)
        }),
    )?;

    linker
        .instance("web:dom/html-input-element")?
        .func_wrap("value", |_, (_handle,): (f64,)| Ok(("typed".to_string(),)))?;
    Ok(())
}

/// Compile `program`, run it against the stub, and hand back the host's table.
fn run_against_stub(program: &str) -> DomObjects {
    common::install_rustls_provider_for_tests();
    let wasm = compile_against_web(program)
        .result
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

    Arc::into_inner(dom)
        .expect("the store is gone, so the table has one owner")
        .into_inner()
        .unwrap()
}

#[test]
fn a_two_level_extends_program_runs_against_a_host_stub() {
    let dom = run_against_stub(PROGRAM);
    // `document`, the element, the event, and the child.
    assert_eq!(dom.objects.len(), 4);
    assert_eq!(dom.objects[1].tag, "div");
    assert_eq!(dom.objects[1].id, "app");
    assert_eq!(dom.objects[1].text, "hello");
    // The event reached the element's own handle, not a re-minted one.
    assert_eq!(
        dom.dispatched,
        vec![(handle(HTML_ELEMENT, 1), "click".to_string())]
    );
}

#[test]
fn type_patterns_narrow_by_the_class_the_host_tagged() {
    run_against_stub(NARROWING_PROGRAM);
}

#[test]
fn handles_compare_as_the_host_interned_them() {
    run_against_stub(IDENTITY_PROGRAM);
}
