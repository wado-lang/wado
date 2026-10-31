//! The declaration attributes whose meaning depends on the declaration they sit
//! on: what a body-less function states about its calls, the parameters
//! `#[immediate]` names, a global's `#[param]`, and the shape `#[wire(tag)]`
//! asks of each case. The body walk checks each against its declaration and
//! records what it states, so reify only copies the record into TIR. What an
//! attribute admits written anywhere is `attribute::check`'s.

use crate::ast::{self, Attribute, Function, GlobalDecl, Module, NamePolicy, SelfKind};
use crate::attribute::{
    self, CM, IMMEDIATE, PARAM, SIDE_EFFECT, STORAGE, wire_name_override, wire_name_policy,
    wire_value,
};
use crate::builtin_facts::{self, BuiltinFacts, ParamShape, ReturnShape};
use crate::compiler_host::{Code, CompilerHost, Diagnostic, DiagnosticSpan, Severity};
use crate::compiler_item::CompilerItem;
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::tir::{ParamSpec, TypeId};
use crate::token::Span;
use crate::type_table::TypeTable;

use super::Elaborator;
use super::item::extract_compiler_item;
use super::sem::types::DeclaredAttrs;
use super::types::TypeError;
use super::tysys::TypeSystem;

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Check `func`'s declaration attributes against its resolved signature,
    /// receiver included in `param_types`, and record what they state.
    pub(super) fn record_declared_attrs(
        &mut self,
        func: &Function,
        param_types: &[TypeId],
        return_type: TypeId,
    ) {
        let params: Vec<(&str, TypeId)> = func
            .params
            .iter()
            .zip(param_types)
            .map(|(param, &ty)| match param.self_kind {
                SelfKind::None => (param.name.as_str(), ty),
                SelfKind::Value | SelfKind::Ref | SelfKind::MutRef => ("self", ty),
            })
            .collect();
        let facts = self.declared_facts(func, &params, return_type);
        let immediates = func
            .attrs
            .iter()
            .filter(|attr| attr.name == IMMEDIATE)
            .filter_map(|attr| self.immediate(attr, &params, carries_cm(func)))
            .collect();
        if func.body.is_some() {
            self.reject_bodyless_attrs_on_body(&func.attrs);
        }
        // An operation's default body is registered under no owner, so this is
        // the only report for one.
        let compiler_item = extract_compiler_item(
            &func.attrs,
            func.span,
            &self.current_module_source,
            self.logger,
        );
        self.sem.types.fn_declared_attrs.insert(
            func.id,
            DeclaredAttrs {
                facts,
                immediates,
                compiler_item,
            },
        );
    }

    /// Check `global`'s `#[param]`, and record the build input it names.
    pub(super) fn record_param_attr(&mut self, global: &GlobalDecl) {
        if let Some(param) = self.param_attr(global) {
            self.sem.types.global_params.insert(global.id, param);
        }
    }

    fn error_at(&self, code: Code, span: Span, message: String) {
        let _ = self.emit(Diagnostic {
            severity: Severity::Error,
            code,
            message,
            span: Some(DiagnosticSpan::from_span(&span, None)),
        });
    }

    /// Report each attribute that describes a declaration with no body, written
    /// on a function that has one.
    fn reject_bodyless_attrs_on_body(&self, attrs: &[Attribute]) {
        for (attr, name, bodyless) in attribute::bodyless(attrs) {
            let _ = self.emit(TypeError::AttributeOnBody {
                attribute: name,
                reason: bodyless.on_body,
                span: attr.span,
            });
        }
    }

    /// The build input a global's `#[param]` names, or `None` where it carries
    /// none or a malformed one. The resolution itself (overrides, env,
    /// conversion) happens later in the param-resolution pass — see
    /// `wep-2026-04-26-compile-time-params.md`.
    fn param_attr(&self, global: &GlobalDecl) -> Option<ParamSpec> {
        let attr = global.attributes.iter().find(|a| a.name == PARAM)?;
        let emit = |message: String| self.error_at(Code::ParamAttr, attr.span, message);

        let mut ok = true;
        if global.mutable {
            emit("#[param] cannot be applied to a mutable global".to_string());
            ok = false;
        }

        for arg in &attr.args {
            match arg {
                ast::AttrArg::KeyValue(k, _) if k == "name" || k == "from_env" => {}
                ast::AttrArg::KeyValue(k, _)
                | ast::AttrArg::KeyArray(k, _)
                | ast::AttrArg::KeyIdentArray(k, _)
                | ast::AttrArg::KeyIdent(k, _)
                | ast::AttrArg::KeyNumber(k, _) => {
                    emit(format!("unknown #[param] argument: {k}"));
                    ok = false;
                }
                bare
                @ (ast::AttrArg::Str(_) | ast::AttrArg::Ident(_) | ast::AttrArg::Number(_)) => {
                    emit(format!("unknown #[param] argument: {}", bare.name()));
                    ok = false;
                }
            }
        }

        let name = match attr.kv_value("name") {
            Some("") => {
                emit("#[param] name must not be empty".to_string());
                ok = false;
                global.name.clone()
            }
            Some(n) => n.to_string(),
            None => global.name.clone(),
        };
        let from_env = match attr.kv_value("from_env") {
            Some("") => {
                emit("#[param] from_env must not be empty".to_string());
                ok = false;
                None
            }
            Some(env) => Some(env.to_string()),
            None => None,
        };

        ok.then_some(ParamSpec { name, from_env })
    }

    /// What a body-less declaration states with `#[storage]` and
    /// `#[side_effect]`. It owes both, unless it only reserves a name. A
    /// Component Model import states neither: the raw call its adapter makes
    /// carries the facts, the same for every import.
    fn declared_facts(
        &self,
        func: &Function,
        params: &[(&str, TypeId)],
        return_type: TypeId,
    ) -> Option<BuiltinFacts<String>> {
        if func.body.is_some() || reserves_a_name_only(func) {
            return None;
        }
        let written = func
            .attrs
            .iter()
            .filter(|attr| attr.name == STORAGE || attr.name == SIDE_EFFECT);
        if carries_cm(func) {
            for attr in written {
                self.error_at(
                    code_of(&attr.name),
                    attr.span,
                    format!(
                        "#[{}] does not belong on a Component Model import: the raw call \
                         its adapter makes carries the facts, the same for every import",
                        attr.name
                    ),
                );
            }
            return None;
        }
        let read = {
            let type_table = self.tysys.type_table.borrow();
            let shapes: Vec<ParamShape<'_>> = params
                .iter()
                .map(|&(name, ty)| ParamShape::of(name, ty, &type_table))
                .collect();
            let ret = ReturnShape::of(return_type, &type_table);
            builtin_facts::read(&func.attrs, &shapes, ret)
        };
        match read {
            Ok(Some(facts)) => Some(facts),
            Ok(None) => {
                self.error_at(
                    Code::StorageAttr,
                    func.name_span,
                    format!(
                        "`{}` has no body, so it states what a call does: \
                         declare #[storage(...)] and #[side_effect(...)]",
                        func.name
                    ),
                );
                None
            }
            Err(faults) => {
                for fault in faults {
                    let attr = func
                        .attrs
                        .iter()
                        .filter(|attr| attr.name == fault.attr)
                        .nth(fault.occurrence)
                        .expect("a fault points at an attribute the declaration carries");
                    self.error_at(code_of(fault.attr), attr.span, fault.message);
                }
                None
            }
        }
    }

    /// The parameter `#[immediate(p)]` names, lowered to a Wasm immediate and
    /// so required to still be a literal when codegen reads it.
    fn immediate(
        &self,
        attr: &Attribute,
        params: &[(&str, TypeId)],
        is_cm_import: bool,
    ) -> Option<String> {
        let emit = |message: String| {
            self.error_at(Code::ImmediateAttr, attr.span, message);
            None
        };
        if is_cm_import {
            return emit(
                "#[immediate] does not belong on a Component Model import: its adapter \
                 calls it rather than encoding it as one instruction"
                    .to_string(),
            );
        }
        let Some(ast::AttrArg::Ident(name)) = attr.args.first() else {
            return emit("#[immediate] names one parameter, unquoted".to_string());
        };
        if !params.iter().any(|&(param, _)| param == name) {
            return emit(format!("#[immediate] names no parameter: {name}"));
        }
        if attr.args.len() > 1 {
            return emit(
                "#[immediate] names one parameter; repeat the attribute for another".to_string(),
            );
        }
        Some(name.clone())
    }
}

/// Each case of a `#[wire(tag)]` variant in `module` whose payload an
/// internally tagged form cannot write. Such a case is written as an object
/// holding the tag beside its payload's fields, so the payload is a struct, or
/// nothing, and writes no field under the tag.
pub(super) fn internal_tag_faults(
    module: &Module,
    tysys: &TypeSystem,
    modules: &IndexMap<ModuleSource, Module>,
) -> Vec<Diagnostic> {
    let mut faults = Vec::new();
    for item in &module.items {
        let ast::Item::Variant(decl) = item else {
            continue;
        };
        let Some(tag) = wire_value(&decl.attrs, "tag") else {
            continue;
        };
        let cases = &tysys.data.variant_cases[&tysys.def_at(decl.id)].cases;
        let unit = tysys.type_table.borrow().type_key(TypeTable::UNIT);
        for (case, data) in decl.cases.iter().zip(cases) {
            if tysys.type_table.borrow().type_key(data.payload) == unit {
                continue;
            }
            let case_path = format!("{}::{}", decl.name, case.name);
            let fault = |message: String| Diagnostic {
                severity: Severity::Error,
                code: Code::AttrMisuse,
                message,
                span: Some(DiagnosticSpan::from_span(&case.span, None)),
            };
            let Some(payload) = derived_struct_decl_of(data.payload, tysys, modules) else {
                let payload = tysys.type_table.borrow().type_name(data.payload);
                faults.push(fault(format!(
                    "`{case_path}` carries `{payload}`, but a case of a variant with \
                     `#[wire(tag)]` carries nothing or a struct its derivation writes"
                )));
                continue;
            };
            let policy = wire_name_policy(&payload.attrs);
            for field in &payload.fields {
                let wire_name = NamePolicy::wire_name(
                    wire_name_override(&field.attrs).as_deref(),
                    policy,
                    &field.name,
                );
                if wire_name == tag {
                    faults.push(fault(format!(
                        "`{case_path}` writes its payload's field `{}` as `{tag}`, \
                         the key its tag is written under",
                        field.name
                    )));
                }
            }
        }
    }
    faults
}

/// The struct declaration `ty` instantiates, through any newtype over it,
/// where the derivation reads and writes it as a struct: no link of `ty` has a
/// hand-written `Serialize` or `Deserialize`, whose shape nothing here can see.
/// `String` and `List` are declared as structs and are such.
fn derived_struct_decl_of<'m>(
    ty: TypeId,
    tysys: &TypeSystem,
    modules: &'m IndexMap<ModuleSource, Module>,
) -> Option<&'m ast::StructDecl> {
    let written = [CompilerItem::Serialize, CompilerItem::Deserialize]
        .into_iter()
        .filter_map(|item| tysys.compiler_trait_def(item))
        .any(|trait_| tysys.impl_link(ty, trait_).is_some());
    if written {
        return None;
    }
    let def = {
        let type_table = tysys.type_table.borrow();
        let head = type_table.newtype_representation(ty).unwrap_or(ty);
        type_table.nominal_def(head)?
    };
    let defs = tysys.resolutions.defs();
    let target = defs.ast_id(def);
    modules
        .get(defs.module(def))?
        .items
        .iter()
        .find_map(|item| match item {
            ast::Item::Struct(decl) if decl.id == target => Some(decl),
            _ => None,
        })
}

/// Whether the declaration reserves a name rather than a signature. Nothing
/// ever calls a `#[unavailable]` one, so it states nothing about a call and is
/// owed nothing about one either.
fn reserves_a_name_only(func: &Function) -> bool {
    func.unavailable_attr().is_some()
}

/// Whether the declaration carries `#[cm(...)]`, which makes it a Component
/// Model import called through an adapter. Not `Function::is_cm_import`, which
/// a `#[canonical(...)]` builtin answers too.
fn carries_cm(func: &Function) -> bool {
    func.attrs.iter().any(|attr| attr.name == CM)
}

/// The code a fault in the attribute named `name` is reported under.
fn code_of(name: &str) -> Code {
    if name == STORAGE {
        Code::StorageAttr
    } else {
        assert_eq!(name, SIDE_EFFECT, "a fact attribute is one of the two");
        Code::SideEffectAttr
    }
}
