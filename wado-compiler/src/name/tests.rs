
use super::*;
use crate::ast::AstIdSpace;
use crate::hashmap::IndexSet;
use crate::module_source::ModuleSourceInterner;
use crate::tir;

#[test]
fn cm_names() {
    assert_eq!(to_cm_name("distance"), "distance");
    assert_eq!(to_cm_name("MyApi"), "my-api");
    assert_eq!(to_cm_name("set_level"), "set-level");
    assert_eq!(to_cm_name("HTTPServer"), "http-server");
    assert_eq!(to_cm_name("parse2html"), "parse2html");
    assert_eq!(to_cm_name("a_1"), "a-1");
}

#[test]
fn a_function_type_name_spells_as_its_mangle() {
    let inner = FqTypeName::function(false, vec![], FqTypeName::builtin("()"), vec![]);
    let f = FqTypeName::function(
        true,
        vec![FqTypeName::binder("T"), FqTypeName::builtin("i32")],
        inner,
        vec![tir::EffectRef::Param { name: "E".into() }],
    );
    assert_eq!(f.to_mangled(), "fn mut(T,i32)->(fn()->())with(E)");
    assert_eq!(f.to_display(), "fn mut(T, i32) -> (fn() -> ()) with E");
    assert!(f.mentions_binder());
    assert_eq!(f.head_only(), f);
}

#[test]
fn before_dae_peels_only_the_dropped_parameters() {
    let x = NarrowedParam {
        param: "self",
        fields: vec!["x"],
        form: FieldForm::Value,
    };
    assert_eq!(
        before_dae(&reshaped_name(
            "$call",
            &[],
            &["self", "x"],
            ReturnShape::Kept
        )),
        "$call"
    );
    assert_eq!(before_dae("$call"), "$call");
    assert_eq!(
        before_dae(&reshaped_name("$call", &[x], &["y"], ReturnShape::Kept)),
        "$call$sroa[self.x]"
    );
}

#[test]
fn diagnostic_name_drops_the_module_and_suffixes_but_keeps_a_minted_name() {
    let mut interner = ModuleSourceInterner::new();
    let entry = interner.entry_point("main.wado");
    let named = |module: &ModuleSource, name: &str| {
        let mangled = FreeFunctionName::new(module.clone(), name.to_string()).to_string();
        diagnostic_function_name(&mangled, module, None)
    };
    assert_eq!(named(&entry, "run"), "run");
    assert_eq!(named(&entry, "$cm_export__run"), "$cm_export__run");
    assert_eq!(
        named(
            &interner.core("prelude/fmt.wado"),
            "write_decimal_digits$sroa[&mut(self.repr)]"
        ),
        "write_decimal_digits"
    );
    // A module path may hold the characters a mangled name separates with.
    assert_eq!(
        named(
            &interner.local("./src (old)/my lib.wado"),
            "follow<core:prelude/string.wado/String>$spec0"
        ),
        "follow<String>"
    );
    // A minted name opens with the prefix; cutting there would leave nothing.
    assert_eq!(
        diagnostic_function_name("$test_0_adds", &entry, None),
        "$test_0_adds"
    );
    assert_eq!(
        diagnostic_function_name(
            &value_copy_helper_name("core:prelude/string.wado/String"),
            &ModuleSource::builtin(),
            None
        ),
        "$value_copy"
    );
    // A type argument carries a module path of its own.
    let traits = interner.core("prelude/traits.wado");
    assert_eq!(
        named(
            &traits,
            "TemplateHole<a.wado/$tmpl$59e1,core:prelude/string.wado/String>::lit$sroa[self.index]"
        ),
        "TemplateHole<$tmpl$59e1,String>::lit"
    );
    let f1 = interner.entry_point("f1.wado");
    assert_eq!(named(&f1, "id<&f1.wado/P>"), "id<&P>");
    assert_eq!(named(&f1, "id<[f1.wado/P,i32]>"), "id<[P,i32]>");
    // A packaged module is named by its package's coordinate.
    assert_eq!(
        named(&f1, "id<acme:dep@0.2.0/src/lib.wado/P,../x.wado/Q>"),
        "id<P,Q>"
    );
    assert_eq!(
        named(
            &f1,
            &format!(
                "id<{}/P>",
                packaged_module_name("dep@1.0.0+b.5", "lib.wado")
            )
        ),
        "id<P>"
    );
    assert_eq!(
        named(&f1, "id<fn(a.wado/P)->i32with($e_263)>$$Closure_0"),
        "id<fn(P)->i32with($e_263)>"
    );
    let method = LocalMethodName::with_method_type_args(
        FqTypeName::builtin("Box"),
        None,
        "transform$spec0".into(),
        vec![FqTypeName::builtin("i64")],
    );
    assert_eq!(
        diagnostic_function_name(&method.to_mangled_name(), &entry, Some(&method)),
        "Box::transform<i64>"
    );
}

#[test]
fn type_bridges_are_told_from_plainly_synthesized_functions() {
    assert!(is_type_bridge(&value_copy_helper_name("main.wado/Point")));
    assert!(is_type_bridge(&effect_default_impl_name("Log", "event")));
    assert!(is_type_bridge(
        &MarkerBridge::HoleFmt.shape_name("$tmpl$abc")
    ));
    assert!(!is_type_bridge(MODULE_INIT_FUNCTION));
    assert!(!is_type_bridge("$test_0_adds"));
    assert!(!is_type_bridge("run"));
}

#[test]
fn test_test_name_to_snake_is_ascii_only() {
    // ASCII alphanumerics survive (lowercased); punctuation/space → `_`.
    assert_eq!(test_name_to_snake("Hello, World!"), "hello__world_");
    // Non-ASCII letters collapse to `_` so the kebab export stays valid.
    assert_eq!(test_name_to_snake("café résumé"), "caf__r_sum_");
    assert_eq!(test_name_to_snake("日本語のテスト ok"), "________ok");
    // A fully non-ASCII name leaves no ASCII segment at all.
    assert_eq!(test_name_to_snake("完全に日本語"), "______");
    // The output must never contain a non-ASCII byte.
    for input in ["café résumé", "日本語のテスト ok", "Ω±∞ μ"] {
        assert!(
            test_name_to_snake(input).is_ascii(),
            "snake form of {input:?} must be ASCII"
        );
    }
}

#[test]
fn mangle_local_item_name_is_space_independent() {
    // The storage name only disambiguates local items *within* a module
    // (the module source qualifies it across modules), so it must depend
    // solely on the module-local `local` index — never on the
    // process-global `AstIdSpace`, which varies with unrelated parse
    // history and would otherwise leak into compiler output (mangled WIR
    // function names), making it non-deterministic.
    let space_a = AstIdSpace::next();
    let space_b = AstIdSpace::next();
    assert_ne!(space_a, space_b);
    let a = mangle_local_item_name("UserId", AstId::new(space_a, 19));
    let b = mangle_local_item_name("UserId", AstId::new(space_b, 19));
    assert_eq!(a, b, "mangled name must not encode the AstIdSpace");
    // Distinct declaration sites within a module still get distinct names.
    assert_ne!(a, mangle_local_item_name("UserId", AstId::new(space_a, 20)));
    // The storage name is a rendering of the declared one, not a
    // replacement for it: readers ask the declaration, so nothing reads
    // the declared name back out of this string.
    assert!(a.starts_with("UserId"));
}

#[test]
fn a_packaged_module_keeps_its_unversioned_name_across_versions() {
    assert_eq!(
        unversioned_module_name("acme:app@1.0.0+build.5/src/lib.wado"),
        "acme:app/src/lib.wado"
    );
    for name in [
        "core:collections/hashmap.wado",
        "src/lib.wado",
        "app.wado",
        "https://user@example.com/lib.wado",
    ] {
        assert_eq!(unversioned_module_name(name), name);
    }
}

#[test]
fn a_function_is_qualified_by_its_module_name() {
    let mut interner = ModuleSourceInterner::new();
    let func = FreeFunctionName::new(interner.core("cli"), "println".to_string());
    assert_eq!(func.to_string(), "core:cli/println");

    let entry = FreeFunctionName::from_module_source(
        &interner.entry_point("/abs/pkg/src/main.wado"),
        "main",
    );
    assert_eq!(entry.to_string(), "main.wado/main");
}

#[test]
fn test_struct_name_to_string() {
    let mut interner = ModuleSourceInterner::new();
    let struct_name = StructName::new(interner.local("./geometry.wado"), "Point".to_string());
    assert_eq!(struct_name.to_string(), "geometry.wado/Point");
}

#[test]
fn test_struct_name_hash_eq() {
    let mut interner = ModuleSourceInterner::new();
    let mut point_in = |path: &str| StructName::new(interner.local(path), "Point".to_string());
    let s1 = point_in("./geometry.wado");
    let s2 = point_in("./geometry.wado");
    let s3 = point_in("./other.wado");

    let mut set = IndexSet::default();
    set.insert(s1);
    assert!(set.contains(&s2));
    assert!(!set.contains(&s3));
}

#[test]
fn test_receiver_mangle() {
    let shared = Receiver::Ref(RefKind::Shared);
    assert_eq!(shared.mangle(&["i32".into()]), "&i32");
    assert_eq!(shared.mangle(&["List<i32>".into()]), "&List<i32>");
    let mutable = Receiver::Ref(RefKind::Mut);
    assert_eq!(mutable.mangle(&["String".into()]), "&mut String");
    assert_eq!(mutable.mangle(&["List<i32>".into()]), "&mut List<i32>");
    // Named receivers fall through to mangle_generic_name.
    assert_eq!(
        Receiver::Type(FqTypeName::builtin("List")).mangle(&["i32".into()]),
        "List<i32>"
    );
    // A reference to a named head keeps the head, its arguments applied.
    let list = FqTypeName::builtin("List");
    let shared_list = Receiver::RefTo(RefKind::Shared, list.clone());
    assert_eq!(shared_list.mangle(&["i32".into()]), "&List<i32>");
    assert_eq!(shared_list.head_key().as_mangled_str(), "&List");
    let mut_list = Receiver::RefTo(RefKind::Mut, list);
    assert_eq!(mut_list.mangle(&["i32".into()]), "&mut List<i32>");
    assert_eq!(mut_list.mangle(&[]), "&mut List");
}

#[test]
fn test_ref_impl_receiver_names_its_referent() {
    let item = FqTypeName::builtin("Item");
    let template = LocalMethodName::of(
        Receiver::of_ref_impl(
            RefKind::Shared,
            &item.clone().with_reference(RefKind::Shared),
        ),
        None,
        "eq".into(),
    );
    assert_eq!(template.to_mangled_name(), "&Item::eq");
    let instance = template.with_struct_type_args(&[FqTypeName::builtin("i32")]);
    assert_eq!(instance.to_mangled_name(), "&Item<i32>::eq");
    assert_eq!(instance.fq_struct_name().to_mangled(), "&Item<i32>");
    // A type parameter behind the reference leaves the bare kind.
    let blanket = Receiver::of_ref_impl(
        RefKind::Mut,
        &FqTypeName::binder("T").with_reference(RefKind::Mut),
    );
    assert_eq!(blanket, Receiver::Ref(RefKind::Mut));
    let at_owner = template.at_owner(
        &item
            .with_args(vec![FqTypeName::builtin("bool")])
            .with_reference(RefKind::Shared),
    );
    assert_eq!(
        at_owner,
        instance.with_struct_type_args(&[FqTypeName::builtin("bool")])
    );
}
