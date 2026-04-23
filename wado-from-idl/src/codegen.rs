//! Wado code generation

use std::fmt::Write;

use crate::ir::{
    WadoEffect, WadoEnum, WadoFlags, WadoFunction, WadoModule, WadoParam, WadoResource, WadoStruct,
    WadoType, WadoTypeDef, WadoVariant, WadoWorld,
};

pub struct WadoCodeGenerator {
    output: String,
    indent: usize,
}

impl WadoCodeGenerator {
    #[must_use]
    pub fn new() -> Self {
        Self {
            output: String::new(),
            indent: 0,
        }
    }

    pub fn generate(&mut self, module: &WadoModule) -> String {
        self.output.clear();

        // Header comment
        self.write_header(module);

        // Types
        for type_def in &module.types {
            self.write_type_def(type_def);
            self.writeln("");
        }

        // Resources
        for resource in &module.resources {
            self.write_resource(resource);
            self.writeln("");
        }

        // Effects
        for effect in &module.effects {
            self.write_effect(effect);
            self.writeln("");
        }

        // Worlds
        for world in &module.worlds {
            self.write_world(world);
            self.writeln("");
        }

        self.output.clone()
    }

    fn write_header(&mut self, module: &WadoModule) {
        // Emit `#![generated(...)]` with key-value metadata so that
        // `sources` / `by` survive formatting and can be consumed by tooling.
        // `#![generated]` itself marks the module as machine-generated.
        let mut parts = vec![String::from("by = \"wado-from-idl\"")];
        if !module.source_files.is_empty() {
            let quoted: Vec<String> = module
                .source_files
                .iter()
                .map(|s| format!("\"{s}\""))
                .collect();
            parts.push(format!("sources = [{}]", quoted.join(", ")));
        }
        self.writeln(&format!("#![generated({})]", parts.join(", ")));
        self.writeln("");

        // Cross-interface use imports
        for import in &module.imports {
            let names = import.type_names.join(", ");
            self.writeln(&format!("use {{ {names} }} from \"{}\";", import.from_path));
        }
        if !module.imports.is_empty() {
            self.writeln("");
        }
    }

    fn write_type_def(&mut self, type_def: &WadoTypeDef) {
        match type_def {
            WadoTypeDef::Enum(e) => self.write_enum(e),
            WadoTypeDef::Flags(f) => self.write_flags(f),
            WadoTypeDef::Struct(s) => self.write_struct(s),
            WadoTypeDef::Variant(v) => self.write_variant(v),
            WadoTypeDef::Newtype(a) => {
                // Skip self-referential newtypes (e.g., `type Duration = Duration;`)
                // These occur when WIT re-exports a type from another interface via `use`.
                if let WadoType::Named(target_name) = &a.target
                    && target_name == &a.name
                {
                    return;
                }
                if let Some(ref attr) = a.cm_attr {
                    self.writeln(&format!("#[cm(\"{attr}\")]"));
                }
                self.writeln(&format!(
                    "pub type {} = {};",
                    a.name,
                    Self::format_type(&a.target)
                ));
            }
        }
    }

    fn write_enum(&mut self, e: &WadoEnum) {
        self.write_doc_comment(e.doc_comment.as_ref());
        if let Some(ref attr) = e.cm_attr {
            self.writeln(&format!("#[cm(\"{attr}\")]"));
        }
        self.writeln(&format!("pub enum {} {{", e.name));
        self.indent += 1;

        for variant in &e.variants {
            self.write_doc_comment(variant.doc_comment.as_ref());
            if let Some(ref attr) = variant.cm_attr {
                self.writeln(&format!("#[cm(\"{attr}\")]"));
            }
            self.writeln(&format!("{},", variant.name));
        }

        self.indent -= 1;
        self.writeln("}");
    }

    fn write_flags(&mut self, f: &WadoFlags) {
        self.write_doc_comment(f.doc_comment.as_ref());
        if let Some(ref attr) = f.cm_attr {
            self.writeln(&format!("#[cm(\"{attr}\")]"));
        }
        self.writeln(&format!("pub flags {} {{", f.name));
        self.indent += 1;

        for flag in &f.flags {
            self.write_doc_comment(flag.doc_comment.as_ref());
            self.writeln(&format!("#[cm(\"{}\")]", flag.cm_attr));
            self.writeln(&format!("{},", flag.name));
        }

        self.indent -= 1;
        self.writeln("}");
    }

    fn write_struct(&mut self, s: &WadoStruct) {
        self.write_doc_comment(s.doc_comment.as_ref());
        if let Some(ref attr) = s.cm_attr {
            self.writeln(&format!("#[cm(\"{attr}\")]"));
        }
        self.writeln(&format!("pub struct {} {{", s.name));
        self.indent += 1;

        for field in &s.fields {
            self.write_doc_comment(field.doc_comment.as_ref());
            self.writeln(&format!("#[cm(\"{}\")]", field.cm_attr));
            self.writeln(&format!(
                "pub {}: {},",
                field.name,
                Self::format_type(&field.ty)
            ));
        }

        self.indent -= 1;
        self.writeln("}");
    }

    fn write_variant(&mut self, v: &WadoVariant) {
        self.write_doc_comment(v.doc_comment.as_ref());
        if let Some(ref attr) = v.cm_attr {
            self.writeln(&format!("#[cm(\"{attr}\")]"));
        }
        self.writeln(&format!("pub variant {} {{", v.name));
        self.indent += 1;

        for case in &v.cases {
            self.write_doc_comment(case.doc_comment.as_ref());
            if let Some(ref attr) = case.cm_attr {
                self.writeln(&format!("#[cm(\"{attr}\")]"));
            }
            match &case.payload {
                Some(ty) => self.writeln(&format!("{}({}),", case.name, Self::format_type(ty))),
                None => self.writeln(&format!("{},", case.name)),
            }
        }

        self.indent -= 1;
        self.writeln("}");
    }

    fn write_resource(&mut self, resource: &WadoResource) {
        self.write_doc_comment(resource.doc_comment.as_ref());
        self.writeln(&format!("#[cm(\"{}\")]", resource.cm_attr));

        if resource.methods.is_empty() {
            self.writeln(&format!("pub resource {};", resource.name));
        } else {
            self.writeln(&format!("pub resource {} {{", resource.name));
            self.indent += 1;

            for method in &resource.methods {
                self.write_function(method);
            }

            self.indent -= 1;
            self.writeln("}");
        }
    }

    fn write_effect(&mut self, effect: &WadoEffect) {
        self.write_doc_comment(effect.doc_comment.as_ref());
        self.writeln(&format!("#[cm(\"{}\")]", effect.cm_interface));
        self.writeln(&format!("pub effect {} {{", effect.name));
        self.indent += 1;

        for func in &effect.functions {
            self.write_function(func);
        }

        self.indent -= 1;
        self.writeln("}");
    }

    fn write_function(&mut self, func: &WadoFunction) {
        self.write_doc_comment(func.doc_comment.as_ref());
        self.writeln(&format!("#[cm(\"{}\")]", func.cm_attr));

        // Emit #[cm_params] with original WIT kebab-case parameter names
        if !func.params.is_empty() {
            let wit_names: Vec<String> = func
                .params
                .iter()
                .map(|p| format!("\"{}\"", p.wit_name))
                .collect();
            self.writeln(&format!("#[cm_params({})]", wit_names.join(", ")));
        }

        let params = Self::format_params(&func.params);

        // For CM `async func` imports, the Wado-level signature returns
        // `AsyncCall<T>` instead of `T` (or `AsyncCall<()>` for WIT-void async
        // imports). Users then call `.wait()` on the task to block on
        // the result, or compose multiple tasks with `WaitableSet`. The
        // CM canonical ABI is unchanged — the wrapping lives purely at
        // the Wado-level adapter synthesised by `cm_binding.rs`.
        //
        // The `async` keyword is retained in the effect declaration as
        // the signal to the binding synthesiser that this import should
        // be lowered with `canon lower async`. The synthesiser strips the
        // `AsyncCall<T>` wrapper to recover `T` when computing the CM ABI
        // layout for the outptr buffer.
        let return_type = if func.is_async {
            let inner = match (&func.return_type, func.never_returns) {
                (_, true) => "!".to_string(),
                (Some(ty), false) => Self::format_type(ty),
                (None, false) => "()".to_string(),
            };
            format!(" -> AsyncCall<{inner}>")
        } else {
            match (&func.return_type, func.never_returns) {
                (_, true) => " -> !".to_string(),
                (Some(ty), false) => format!(" -> {}", Self::format_type(ty)),
                (None, false) => String::new(),
            }
        };

        let async_kw = if func.is_async { "async " } else { "" };
        self.writeln(&format!(
            "{}fn {}({}){};",
            async_kw, func.name, params, return_type
        ));
    }

    fn write_world(&mut self, world: &WadoWorld) {
        self.write_doc_comment(world.doc_comment.as_ref());
        // Write the canonical WIT world name as an attribute
        self.writeln(&format!("#[cm(\"{}\")]", world.canonical_name));
        self.writeln(&format!("pub world {} {{", world.name));
        self.indent += 1;

        for import in &world.imports {
            self.writeln(&format!("import {} {{", import.effect_name));
            self.indent += 1;
            for func in &import.functions {
                self.writeln(&format!("{func},"));
            }
            self.indent -= 1;
            self.writeln("}");
            self.writeln("");
        }

        for export in &world.exports {
            // World exports keep async keyword as it's part of the ABI specification
            let async_kw = if export.is_async { "async " } else { "" };
            let params = Self::format_params(&export.params);
            let return_type = export
                .return_type
                .as_ref()
                .map(|ty| format!(" -> {}", Self::format_type(ty)))
                .unwrap_or_default();

            self.writeln(&format!(
                "export {}fn {}({}){};",
                async_kw, export.name, params, return_type
            ));
        }

        self.indent -= 1;
        self.writeln("}");
    }

    fn format_params(params: &[WadoParam]) -> String {
        params
            .iter()
            .map(|p| format!("{}: {}", p.name, Self::format_type(&p.ty)))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn format_type(ty: &WadoType) -> String {
        match ty {
            WadoType::Bool => "bool".to_string(),
            WadoType::Char => "char".to_string(),
            WadoType::I8 => "i8".to_string(),
            WadoType::I16 => "i16".to_string(),
            WadoType::I32 => "i32".to_string(),
            WadoType::I64 => "i64".to_string(),
            WadoType::I128 => "i128".to_string(),
            WadoType::U8 => "u8".to_string(),
            WadoType::U16 => "u16".to_string(),
            WadoType::U32 => "u32".to_string(),
            WadoType::U64 => "u64".to_string(),
            WadoType::U128 => "u128".to_string(),
            WadoType::F32 => "f32".to_string(),
            WadoType::F64 => "f64".to_string(),
            WadoType::String => "String".to_string(),
            WadoType::Option(inner) => format!("Option<{}>", Self::format_type(inner)),
            WadoType::Result { ok, err } => {
                let ok_ty = ok
                    .as_ref()
                    .map_or_else(|| "()".to_string(), |t| Self::format_type(t));
                let err_ty = err
                    .as_ref()
                    .map_or_else(|| "()".to_string(), |t| Self::format_type(t));
                format!("Result<{ok_ty}, {err_ty}>")
            }
            WadoType::Array(inner) => format!("Array<{}>", Self::format_type(inner)),
            WadoType::Tuple(types) => {
                if types.is_empty() {
                    "()".to_string()
                } else {
                    let inner = types
                        .iter()
                        .map(Self::format_type)
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("[{inner}]")
                }
            }
            WadoType::Stream(inner) => format!("Stream<{}>", Self::format_type(inner)),
            WadoType::Future(inner) => format!("Future<{}>", Self::format_type(inner)),
            WadoType::Named(name) => name.clone(),
            WadoType::Borrow(inner) => format!("&{}", Self::format_type(inner)),
        }
    }

    fn write_doc_comment(&mut self, doc: Option<&String>) {
        if let Some(doc) = doc {
            for line in doc.lines() {
                self.writeln(&format!("/// {line}"));
            }
        }
    }

    fn writeln(&mut self, line: &str) {
        if line.is_empty() {
            self.output.push('\n');
        } else {
            let indent = "    ".repeat(self.indent);
            writeln!(self.output, "{indent}{line}").unwrap();
        }
    }
}

impl Default for WadoCodeGenerator {
    fn default() -> Self {
        Self::new()
    }
}
