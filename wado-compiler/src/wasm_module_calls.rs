//! A `#![wasm_module("name")]` module compiles to a separate core module that
//! holds its own module's code and nothing else, which keeps it small. A call
//! out of it reaches no function there, so it is rejected where it is written.

use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::flat_package::FlatPackage;
use crate::module_source::ModuleSource;
use crate::tir::{FunctionRef, TirExpr, TirExprKind};
use crate::tir_visitor::TirRefVisitor;
use crate::token::Span;

/// Every call a `#![wasm_module]` function makes to a function its core module
/// does not hold, with the module that call is written in.
pub fn calls_out_of_wasm_modules(flat: &FlatPackage) -> Vec<(ModuleSource, Diagnostic)> {
    let mut out = Vec::new();
    for func in &flat.functions {
        let func = func.borrow();
        let Some(core_module) = flat.wasm_module_sources.get(&func.module_source) else {
            continue;
        };
        let Some(body) = &func.body else {
            continue;
        };
        let mut calls = CallsOut {
            flat,
            core_module,
            found: Vec::new(),
        };
        calls.visit_block(body);
        out.extend(calls.found.into_iter().map(|(callee, span)| {
            let diagnostic = Diagnostic {
                severity: Severity::Error,
                code: Code::UnsupportedFeature,
                message: format!(
                    "`{}` calls `{callee}`, which the core module `{core_module}` does not hold: \
                     a `#![wasm_module]` module calls only its own functions and builtins",
                    func.name
                ),
                span: Some(DiagnosticSpan::from_span(&span, None)),
            };
            (func.module_source.clone(), diagnostic)
        }));
    }
    out
}

struct CallsOut<'a> {
    flat: &'a FlatPackage,
    core_module: &'a str,
    found: Vec<(String, Span)>,
}

impl CallsOut<'_> {
    fn is_held(&self, callee: &FunctionRef) -> bool {
        callee.module_source.is_builtin()
            || self
                .flat
                .wasm_module_sources
                .get(&callee.module_source)
                .is_some_and(|name| name == self.core_module)
    }
}

impl TirRefVisitor for CallsOut<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if let TirExprKind::Call { func, .. } = &expr.kind
            && !self.is_held(func)
        {
            self.found.push((func.name.clone(), expr.span.clone()));
        }
        self.walk_expr(expr);
    }
}
