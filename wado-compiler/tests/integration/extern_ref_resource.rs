//! `#[cm(..., type="extern-ref")]` marks a resource as a host-object handle:
//! copyable, and outside the affine discipline every other resource follows.
//! See `docs/wep-2026-04-28-resource-inheritance.md`.

use crate::common::InMemoryHost;
use wado_compiler::check_resource_moves_semantic;
use wado_compiler::semantics::semantics;

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

/// Elaboration diagnostics, as `"CODE: message"`.
fn diagnostics(source: &str) -> Vec<String> {
    let host = InMemoryHost::new();
    let _ = block_on(semantics(source, &host, Some("entry.wado")));
    host.diagnostics()
        .into_iter()
        .map(|d| format!("{:?}: {}", d.code, d.message))
        .collect()
}

fn move_errors(source: &str) -> Vec<String> {
    let host = InMemoryHost::new();
    let sem = block_on(semantics(source, &host, Some("entry.wado")));
    check_resource_moves_semantic(&sem)
        .into_iter()
        .map(|e| e.to_string())
        .collect()
}

const USE_TWICE: &str = r#"
fn consume(h: Handle) {}

fn use_twice(h: Handle) {
    consume(h);
    consume(h);
}

export fn run() {}
"#;

#[test]
fn a_plain_resource_is_move_only() {
    let source = format!("resource Handle {{}}\n{USE_TWICE}");
    let errors = move_errors(&source);
    assert!(
        errors.iter().any(|e| e.contains("use_twice") || e.contains('h')),
        "expected a use-after-move error, got {errors:?}"
    );
}

#[test]
fn an_extern_ref_resource_is_copyable() {
    let source = format!(
        "#[cm(\"web:dom/handle\", type=\"extern-ref\")]\nresource Handle {{}}\n{USE_TWICE}"
    );
    let errors = move_errors(&source);
    assert!(
        errors.is_empty(),
        "an extern-ref handle is copyable, got {errors:?}"
    );
}

#[test]
fn an_i32_backed_resource_stays_move_only() {
    let source =
        format!("#[cm(\"wasi:demo/handle\", type=\"i32\")]\nresource Handle {{}}\n{USE_TWICE}");
    let errors = move_errors(&source);
    assert!(
        !errors.is_empty(),
        "an i32-backed handle keeps the affine discipline"
    );
}

const EXTERN_REF: &str = "#[cm(\"web:dom/event-target\", type = \"extern-ref\")]";

#[test]
fn extends_links_two_extern_ref_resources() {
    let source = format!(
        "{EXTERN_REF}\nresource EventTarget {{}}\n\
         #[cm(\"web:dom/node\", type = \"extern-ref\")]\n\
         resource Node extends EventTarget {{}}\n\
         export fn run() {{}}\n"
    );
    assert!(
        diagnostics(&source).is_empty(),
        "a well-formed chain reports nothing, got {:?}",
        diagnostics(&source)
    );
}

#[test]
fn extends_requires_extern_ref_on_both_sides() {
    let source = "resource EventTarget {}
         resource Node extends EventTarget {}
         export fn run() {}
";
    let d = diagnostics(source);
    assert!(
        d.iter().any(|e| e.contains("extern-ref")),
        "expected a backing-mismatch error, got {d:?}"
    );
}

#[test]
fn extends_parent_must_be_a_resource() {
    let source = "struct EventTarget {}\n\
         #[cm(\"web:dom/node\", type = \"extern-ref\")]\n\
         resource Node extends EventTarget {}\n\
         export fn run() {}\n";
    let d = diagnostics(source);
    assert!(
        d.iter().any(|e| e.contains("is not a resource")),
        "expected a parent-kind error, got {d:?}"
    );
}

#[test]
fn extends_rejects_a_cycle() {
    let source = "#[cm(\"web:dom/a\", type = \"extern-ref\")]
         resource A extends B {}
         #[cm(\"web:dom/b\", type = \"extern-ref\")]
         resource B extends A {}
         export fn run() {}
";
    let d = diagnostics(source);
    assert!(
        d.iter().any(|e| e.to_lowercase().contains("cycle")),
        "expected a cycle error, got {d:?}"
    );
}

#[test]
fn extends_rejects_a_generic_parent_written_with_arguments() {
    let source = "#[cm(\"web:dom/base\", type = \"extern-ref\")]
         resource Base<T> {}
         #[cm(\"web:dom/leaf\", type = \"extern-ref\")]
         resource Leaf extends Base<i32> {}
         export fn run() {}
";
    let d = diagnostics(source);
    assert!(
        d.iter().any(|e| e.contains("generic")),
        "a generic parent must be reported, got {d:?}"
    );
}

/// Two extern-ref resources in a chain, plus whatever the case needs.
fn chain(rest: &str) -> String {
    format!(
        "#[cm(\"web:dom/event-target\", type = \"extern-ref\")]\n\
         resource EventTarget {{}}\n\
         #[cm(\"web:dom/node\", type = \"extern-ref\")]\n\
         resource Node extends EventTarget {{}}\n\
         {rest}\n\
         export fn run() {{}}\n"
    )
}

#[test]
fn a_child_passes_where_the_parent_is_expected() {
    let source = chain(
        "fn takes(t: EventTarget) -> EventTarget { return t; }\n\
         fn give(n: Node) -> EventTarget { return takes(n); }",
    );
    let d = diagnostics(&source);
    assert!(d.is_empty(), "upcast is implicit, got {d:?}");
}

#[test]
fn a_parent_does_not_pass_where_the_child_is_expected() {
    let source = chain(
        "fn takes(n: Node) -> Node { return n; }\n\
         fn give(t: EventTarget) -> Node { return takes(t); }",
    );
    let d = diagnostics(&source);
    assert!(!d.is_empty(), "downcast is never implicit");
}

#[test]
fn a_shared_reference_is_covariant() {
    let source = chain(
        "fn takes(t: &EventTarget) -> i32 { return 1; }\n\
         fn give(n: &Node) -> i32 { return takes(n); }",
    );
    let d = diagnostics(&source);
    assert!(d.is_empty(), "&Child is usable as &Parent, got {d:?}");
}

#[test]
fn a_mutable_reference_is_invariant() {
    let source = chain(
        "fn takes(t: &mut EventTarget) -> i32 { return 1; }\n\
         fn give(n: &mut Node) -> i32 { return takes(n); }",
    );
    let d = diagnostics(&source);
    assert!(!d.is_empty(), "&mut is invariant");
}

#[test]
fn a_container_is_invariant() {
    let source = chain(
        "fn takes(t: List<EventTarget>) -> i32 { return t.len(); }\n\
         fn give(n: List<Node>) -> i32 { return takes(n); }",
    );
    let d = diagnostics(&source);
    assert!(!d.is_empty(), "List<Child> is not List<Parent>");
}

#[test]
fn unrelated_resources_are_incomparable() {
    let source = chain(
        "#[cm(\"web:dom/other\", type = \"extern-ref\")]\n\
         resource Other {}\n\
         fn takes(t: EventTarget) -> EventTarget { return t; }\n\
         fn give(o: Other) -> EventTarget { return takes(o); }",
    );
    let d = diagnostics(&source);
    assert!(!d.is_empty(), "an unrelated resource is not a subtype");
}

/// A parent carrying one instance method, plus whatever the case needs.
fn chain_with_method(parent_body: &str, rest: &str) -> String {
    format!(
        "#[cm(\"web:dom/event-target\", type = \"extern-ref\")]\n\
         resource EventTarget {{\n{parent_body}\n}}\n\
         #[cm(\"web:dom/node\", type = \"extern-ref\")]\n\
         resource Node extends EventTarget {{}}\n\
         {rest}\n\
         export fn run() {{}}\n"
    )
}

#[test]
fn a_child_calls_an_inherited_method() {
    let source = chain_with_method(
        "    fn tag(&self) -> String;",
        "fn use_it(n: Node) -> String { return n.tag(); }",
    );
    let d = diagnostics(&source);
    assert!(d.is_empty(), "an inherited method is callable, got {d:?}");
}

#[test]
fn a_static_method_does_not_inherit() {
    let source = chain_with_method(
        "    fn make() -> EventTarget;",
        "fn use_it() -> EventTarget { return Node::make(); }",
    );
    let d = diagnostics(&source);
    assert!(!d.is_empty(), "a static method is not inherited");
}

#[test]
fn self_stays_the_declaring_resource() {
    let source = chain_with_method(
        "    fn me(&self) -> Self;",
        "fn widen(n: Node) -> EventTarget { return n.me(); }",
    );
    let d = diagnostics(&source);
    assert!(
        d.is_empty(),
        "an inherited `Self` is the declaring resource, got {d:?}"
    );

    let narrowed = chain_with_method(
        "    fn me(&self) -> Self;",
        "fn narrow(n: Node) -> Node { return n.me(); }",
    );
    let d = diagnostics(&narrowed);
    assert!(!d.is_empty(), "`Self` does not follow the receiver's type");
}

/// `Self` on a resource method is the declaring resource, so a return typed
/// `Self` is checked like any other — it used to resolve to `unknown`, which
/// deferred every check against it.
#[test]
fn an_inherited_return_type_is_checked() {
    let source = chain_with_method(
        "    fn me(&self) -> EventTarget;",
        "fn narrow(n: Node) -> Node { return n.me(); }",
    );
    let d = diagnostics(&source);
    assert!(
        !d.is_empty(),
        "a parent's return type does not narrow to the child"
    );
}

#[test]
fn a_self_return_is_checked() {
    let source = "#[cm(\"web:dom/event-target\", type = \"extern-ref\")]\n\
         resource EventTarget {\n    fn me(&self) -> Self;\n}\n\
         #[cm(\"web:dom/other\", type = \"extern-ref\")]\n\
         resource Other {}\n\
         fn narrow(e: EventTarget) -> Other { return e.me(); }\n\
         export fn run() {}\n";
    let d = diagnostics(source);
    assert!(!d.is_empty(), "`Self` is the declaring resource, not `Other`");
}

#[test]
fn a_child_cannot_redeclare_an_inherited_method() {
    let source = "#[cm(\"web:dom/event-target\", type = \"extern-ref\")]\n\
         resource EventTarget {\n    fn tag(&self) -> String;\n}\n\
         #[cm(\"web:dom/node\", type = \"extern-ref\")]\n\
         resource Node extends EventTarget {\n    fn tag(&self) -> String;\n}\n\
         export fn run() {}\n";
    let d = diagnostics(source);
    assert!(
        d.iter().any(|e| e.contains("tag")),
        "expected an override error naming the method, got {d:?}"
    );
}

#[test]
fn a_child_may_declare_its_own_method_names() {
    let source = "#[cm(\"web:dom/event-target\", type = \"extern-ref\")]\n\
         resource EventTarget {\n    fn tag(&self) -> String;\n}\n\
         #[cm(\"web:dom/node\", type = \"extern-ref\")]\n\
         resource Node extends EventTarget {\n    fn text(&self) -> String;\n}\n\
         export fn run() {}\n";
    let d = diagnostics(source);
    assert!(d.is_empty(), "a distinct name is fine, got {d:?}");
}

#[test]
fn a_trait_impl_colliding_with_a_resource_method_is_ambiguous() {
    let source = "#[cm(\"web:dom/element\", type = \"extern-ref\")]\n\
         resource Element {\n    fn id(&self) -> String;\n}\n\
         trait Identified {\n    fn id(&self) -> String;\n}\n\
         impl Identified for Element {\n    fn id(&self) -> String { return \"x\"; }\n}\n\
         fn use_it(e: Element) -> String { return e.id(); }\n\
         export fn run() {}\n";
    let d = diagnostics(source);
    assert!(
        d.iter().any(|e| e.contains("ambiguous")),
        "expected an ambiguity error, got {d:?}"
    );
}

#[test]
fn a_trait_impl_without_a_collision_is_fine() {
    let source = "#[cm(\"web:dom/element\", type = \"extern-ref\")]\n\
         resource Element {\n    fn id(&self) -> String;\n}\n\
         trait Named {\n    fn name(&self) -> String;\n}\n\
         impl Named for Element {\n    fn name(&self) -> String { return \"x\"; }\n}\n\
         fn use_it(e: Element) -> String { return e.name(); }\n\
         export fn run() {}\n";
    let d = diagnostics(source);
    assert!(d.is_empty(), "distinct names do not collide, got {d:?}");
}

#[test]
fn a_cycle_above_the_child_terminates() {
    // `A extends B` is well-formed on its own; the cycle is between its
    // ancestors, so the override walk must not follow it forever.
    let source = "#[cm(\"web:dom/a\", type = \"extern-ref\")]\n\
         resource A extends B {\n    fn tag(&self) -> String;\n}\n\
         #[cm(\"web:dom/b\", type = \"extern-ref\")]\n\
         resource B extends C {\n    fn tag(&self) -> String;\n}\n\
         #[cm(\"web:dom/c\", type = \"extern-ref\")]\n\
         resource C extends B {\n    fn tag(&self) -> String;\n}\n\
         export fn run() {}\n";
    let d = diagnostics(source);
    assert!(
        d.iter().any(|e| e.to_lowercase().contains("cycle")),
        "expected the cycle reported, got {d:?}"
    );
}

#[test]
fn branches_agree_on_the_ancestor() {
    let source = chain(
        "fn pick(flag: bool, n: Node, t: EventTarget) -> EventTarget {\n\
         \x20   let x = if flag { n } else { t };\n\
         \x20   return x;\n\
         }",
    );
    let d = diagnostics(&source);
    assert!(d.is_empty(), "an if joins on the ancestor, got {d:?}");
}

#[test]
fn match_arms_agree_on_the_ancestor() {
    let source = chain(
        "fn pick(flag: bool, n: Node, t: EventTarget) -> EventTarget {\n\
         \x20   return match flag {\n\
         \x20       true => n,\n\
         \x20       false => t,\n\
         \x20   };\n\
         }",
    );
    let d = diagnostics(&source);
    assert!(d.is_empty(), "match arms join on the ancestor, got {d:?}");
}

#[test]
fn probe_the_declaring_resource_qualifies_its_own_method() {
    let source = chain_with_method(
        "    fn tag(&self) -> String;",
        "fn via_parent(n: Node) -> String { return EventTarget::tag(&n); }",
    );
    let d = diagnostics(&source);
    assert!(d.is_empty(), "probe: parent-qualified, got {d:?}");
}

#[test]
fn probe_a_resource_qualifies_its_own_method() {
    let source = "#[cm(\"web:dom/node\", type = \"extern-ref\")]\n\
         resource Node {\n    fn tag(&self) -> String;\n}\n\
         fn own(n: Node) -> String { return Node::tag(&n); }\n\
         export fn run() {}\n";
    let d = diagnostics(source);
    assert!(d.is_empty(), "probe: own method qualified, got {d:?}");
}

#[test]
fn the_qualified_form_reaches_an_inherited_method() {
    let source = chain_with_method(
        "    fn tag(&self) -> String;",
        "fn via_child(n: Node) -> String { return Node::tag(&n); }",
    );
    let d = diagnostics(&source);
    assert!(
        d.is_empty(),
        "the child qualifies an inherited method, got {d:?}"
    );
}

// --- The rules the WEP states, each against every construct that can reach
// --- them. Derived from the spec, not from the implementation.

#[test]
fn extends_rejects_an_unknown_parent() {
    let source = "#[cm(\"web:dom/node\", type = \"extern-ref\")]\n\
         resource Node extends Nope {}\n\
         export fn run() {}\n";
    let d = diagnostics(source);
    assert!(
        d.iter().any(|e| e.contains("Nope")),
        "an unknown parent must be reported, got {d:?}"
    );
}

#[test]
fn extends_rejects_a_type_parameter_as_parent() {
    let source = "#[cm(\"web:dom/node\", type = \"extern-ref\")]\n\
         resource Node<T> extends T {}\n\
         export fn run() {}\n";
    let d = diagnostics(source);
    assert!(!d.is_empty(), "a type parameter is not a resource");
}

#[test]
fn extends_rejects_a_generic_parent_named_without_arguments() {
    let source = "#[cm(\"web:dom/base\", type = \"extern-ref\")]\n\
         resource Base<T> {\n    fn get(&self) -> T;\n}\n\
         #[cm(\"web:dom/leaf\", type = \"extern-ref\")]\n\
         resource Leaf extends Base {}\n\
         export fn run() {}\n";
    let d = diagnostics(source);
    assert!(
        d.iter().any(|e| e.contains("generic")),
        "the declaration's arity decides, not the spelling, got {d:?}"
    );
}

#[test]
fn extends_rejects_a_generic_child() {
    let source = "#[cm(\"web:dom/parent\", type = \"extern-ref\")]\n\
         resource Parent {}\n\
         #[cm(\"web:dom/child\", type = \"extern-ref\")]\n\
         resource Child<T> extends Parent {}\n\
         export fn run() {}\n";
    let d = diagnostics(source);
    assert!(
        d.iter().any(|e| e.contains("generic")),
        "a generic child has no subtype relation to give, got {d:?}"
    );
}

#[test]
fn another_attributes_type_field_is_its_own_business() {
    let source = "#[wire(type = \"i32\")]\n\
         struct Wrapper { a: i32 }\n\
         export fn run() {}\n";
    let d = diagnostics(source);
    assert!(
        !d.iter().any(|e| e.contains("resource")),
        "only #[cm] names a backing, got {d:?}"
    );
}

#[test]
fn a_child_may_declare_a_static_the_parent_also_declares() {
    let source = "#[cm(\"web:dom/event-target\", type = \"extern-ref\")]\n\
         resource EventTarget {\n    fn make() -> EventTarget;\n}\n\
         #[cm(\"web:dom/node\", type = \"extern-ref\")]\n\
         resource Node extends EventTarget {\n    fn make() -> Node;\n}\n\
         export fn run() {}\n";
    let d = diagnostics(source);
    assert!(
        d.is_empty(),
        "a static is not inherited, so it shadows nothing, got {d:?}"
    );
}

/// `&Child` / `&Parent` branches, in both orders, through every construct that
/// unifies branches.
fn ref_branches(body: &str) -> String {
    format!(
        "#[cm(\"web:dom/event-target\", type = \"extern-ref\")]\n\
         resource EventTarget {{}}\n\
         #[cm(\"web:dom/node\", type = \"extern-ref\")]\n\
         resource Node extends EventTarget {{}}\n\
         fn pick(flag: bool, n: &Node, t: &EventTarget) -> i32 {{\n{body}\n}}\n\
         fn size(e: &EventTarget) -> i32 {{ return 1; }}\n\
         export fn run() {{}}\n"
    )
}

#[test]
fn shared_reference_branches_join_in_both_orders() {
    for body in [
        "    let x = if flag { n } else { t };\n    return size(x);",
        "    let x = if flag { t } else { n };\n    return size(x);",
    ] {
        let d = diagnostics(&ref_branches(body));
        assert!(d.is_empty(), "`if` joins &Child with &Parent, got {d:?}");
    }
}

#[test]
fn shared_reference_match_arms_join_in_both_orders() {
    for body in [
        "    let x = match flag { true => n, false => t };\n    return size(x);",
        "    let x = match flag { true => t, false => n };\n    return size(x);",
    ] {
        let d = diagnostics(&ref_branches(body));
        assert!(d.is_empty(), "`match` joins &Child with &Parent, got {d:?}");
    }
}

#[test]
fn if_let_branches_join_on_the_ancestor() {
    let source = "#[cm(\"web:dom/event-target\", type = \"extern-ref\")]\n\
         resource EventTarget {}\n\
         #[cm(\"web:dom/node\", type = \"extern-ref\")]\n\
         resource Node extends EventTarget {}\n\
         fn pick(maybe: Option<Node>, t: EventTarget) -> EventTarget {\n\
         \x20   let x = if let Option::Some(n) = maybe { n } else { t };\n\
         \x20   return x;\n\
         }\n\
         export fn run() {}\n";
    let d = diagnostics(source);
    assert!(d.is_empty(), "`if let` joins on the ancestor, got {d:?}");
}
