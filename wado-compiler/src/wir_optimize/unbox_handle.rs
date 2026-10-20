//! Represent an affine resource by its bare handle where no reference reaches
//! its `$Resource` box (WEP: Affine Types), as a primitive is kept unboxed.
//!
//! The places a resource flows through — locals, parameters and results — are
//! joined into classes by the copies and calls between them. A class keeps its
//! box when any of its places escapes: into a field, an element, a global, an
//! indirect or pinned call, or a store of a handle other than 0, which every
//! reference to the box must see. Otherwise each place becomes an `i32`, with 0
//! for `null`: a box is created only to be read, tested for `null`, zeroed, and
//! moved between places of its class.
//!
//! Zeroing a box only keeps a reference that outlives a move or drop from
//! reaching the slot the host reuses. No such reference reaches a class that
//! keeps no box, so the store of 0 becomes one into the place it names.

use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::name::{RESOURCE_OBJECT_STRUCT, wir_type_key};
use crate::wir::{WirFuncId, WirFuncType, WirInstr, WirPackage, WirType, WirTypeDef, WirTypeId};

use super::util::{branches_at_or_beyond, collect_pinned_func_ids};

/// What the expression under analysis hands its value to.
#[derive(Clone, Copy)]
enum Sink {
    /// Reads the handle, tests for `null`, zeroes the box, or discards it.
    Read,
    /// Keeps the box where no class follows it.
    Escape,
    /// Stores it into the place with this id.
    Flow(usize),
}

pub(super) fn unbox_handles(module: &mut WirPackage) {
    let Some(handle_box) = handle_box_type(module) else {
        return;
    };
    let places = Places::collect(module, handle_box);
    if places.count == 0 {
        return;
    }
    let mut scan = Scan {
        module,
        places: &places,
        parent: (0..places.count).collect(),
        escapes: vec![false; places.count],
        func: 0,
    };
    for id in places.pinned(module) {
        scan.escapes[id] = true;
    }
    for (func, function) in module.functions.iter().enumerate() {
        scan.func = func;
        for instr in function.body.as_deref().unwrap_or(&[]) {
            scan.consume(instr, Sink::Escape);
        }
    }
    let unboxed = scan.unboxed();
    if !unboxed.contains(&true) {
        return;
    }
    retype_signatures(module, &places, &unboxed);
    let mut bodies: Vec<_> = module.functions.iter_mut().map(|f| f.body.take()).collect();
    for (func, body) in bodies.iter_mut().enumerate() {
        let mut rewrite = Rewrite {
            module,
            places: &places,
            unboxed: &unboxed,
            func,
        };
        for instr in body.iter_mut().flatten() {
            rewrite.rewrite(instr);
        }
    }
    for (function, body) in module.functions.iter_mut().zip(bodies) {
        function.body = body;
    }
}

fn handle_box_type(module: &WirPackage) -> Option<u32> {
    let fq = wir_type_key(&ModuleSource::prelude(), RESOURCE_OBJECT_STRUCT);
    module
        .types
        .iter()
        .position(|def| matches!(def, WirTypeDef::Struct(s) if s.name.fq == fq))
        .map(|index| u32::try_from(index).expect("type index fits u32"))
}

/// Every place of `$Resource` type, numbered: each function's locals and
/// parameters by name, and its result.
struct Places {
    handle_box: u32,
    count: usize,
    locals: Vec<IndexMap<String, usize>>,
    results: Vec<Option<usize>>,
}

impl Places {
    fn collect(module: &WirPackage, handle_box: u32) -> Self {
        let is_box = |ty: &WirType| matches!(ty, WirType::Ref { type_id, .. } if type_id.index() == handle_box);
        let mut places = Self {
            handle_box,
            count: 0,
            locals: Vec::with_capacity(module.functions.len()),
            results: Vec::with_capacity(module.functions.len()),
        };
        for function in &module.functions {
            let ft = module.types[function.type_id.index() as usize].expect_func();
            let mut locals = IndexMap::default();
            for (name, ty) in function.param_names.iter().zip(&ft.params) {
                if is_box(ty) {
                    let id = places.mint();
                    locals.insert(name.clone(), id);
                }
            }
            let result = match ft.results.as_slice() {
                [result] if is_box(result) => Some(places.mint()),
                _ => None,
            };
            for (name, ty) in function.declared_locals().iter() {
                if is_box(ty) {
                    let id = places.mint();
                    assert!(
                        locals.insert(name.to_string(), id).is_none(),
                        "a function names each local once"
                    );
                }
            }
            places.locals.push(locals);
            places.results.push(result);
        }
        places
    }

    fn mint(&mut self) -> usize {
        let id = self.count;
        self.count += 1;
        id
    }

    /// The parameters and results of the functions a caller outside this
    /// package's direct calls reaches.
    fn pinned(&self, module: &WirPackage) -> Vec<usize> {
        let pinned_funcs = collect_pinned_func_ids(module);
        let mut pinned = Vec::new();
        for (func, function) in module.functions.iter().enumerate() {
            if function.body.is_some() && !pinned_funcs.contains(&module.defined_func_index(func)) {
                continue;
            }
            pinned.extend(
                function
                    .param_names
                    .iter()
                    .filter_map(|name| self.local(func, name)),
            );
            pinned.extend(self.result(func));
        }
        pinned
    }

    fn is_box(&self, type_id: &WirTypeId) -> bool {
        type_id.index() == self.handle_box
    }

    fn local(&self, func: usize, name: &str) -> Option<usize> {
        self.locals[func].get(name).copied()
    }

    fn result(&self, func: usize) -> Option<usize> {
        self.results[func]
    }

    /// The place a direct call to `func_id` passes its argument `index` into.
    fn param(&self, module: &WirPackage, func_id: &WirFuncId, index: usize) -> Option<usize> {
        let callee = module.defined_func_position(func_id.index())?;
        self.local(callee, module.functions[callee].param_names.get(index)?)
    }

    /// The place a direct call to `func_id` returns from.
    fn call_result(&self, module: &WirPackage, func_id: &WirFuncId) -> Option<usize> {
        self.result(module.defined_func_position(func_id.index())?)
    }
}

/// Joins places into classes by what flows between them, and marks the classes
/// a box escapes from.
struct Scan<'a> {
    module: &'a WirPackage,
    places: &'a Places,
    parent: Vec<usize>,
    escapes: Vec<bool>,
    func: usize,
}

impl Scan<'_> {
    fn find(&mut self, id: usize) -> usize {
        let mut root = id;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut at = id;
        while self.parent[at] != root {
            at = std::mem::replace(&mut self.parent[at], root);
        }
        root
    }

    fn join(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.parent[a] = b;
            self.escapes[b] |= self.escapes[a];
        }
    }

    fn escape(&mut self, id: usize) {
        let root = self.find(id);
        self.escapes[root] = true;
    }

    /// The place `id`'s value goes to `sink`.
    fn place_to(&mut self, id: usize, sink: Sink) {
        match sink {
            Sink::Read => {}
            Sink::Escape => self.escape(id),
            Sink::Flow(to) => self.join(id, to),
        }
    }

    /// A box no class follows goes to `sink`.
    fn opaque_to(&mut self, sink: Sink) {
        if let Sink::Flow(to) = sink {
            self.escape(to);
        }
    }

    fn consume(&mut self, instr: &WirInstr, sink: Sink) {
        let func = self.func;
        match instr {
            WirInstr::LocalGet { name, .. } => match self.places.local(func, name) {
                Some(id) => self.place_to(id, sink),
                None => self.opaque_to(sink),
            },
            WirInstr::LocalTee { name, value } => {
                if let Some(id) = self.places.local(func, name) {
                    self.consume(value, Sink::Flow(id));
                    self.place_to(id, sink);
                } else {
                    self.consume(value, Sink::Escape);
                    self.opaque_to(sink);
                }
            }
            WirInstr::LocalSet { name, value } => {
                let to = self
                    .places
                    .local(func, name)
                    .map_or(Sink::Escape, Sink::Flow);
                self.consume(value, to);
            }
            WirInstr::MultiValueLocalBind { instr, locals } => {
                self.consume(instr, Sink::Escape);
                for name in locals.iter().flatten() {
                    if let Some(id) = self.places.local(func, name) {
                        self.escape(id);
                    }
                }
            }
            WirInstr::StructNew { type_id, fields } if self.places.is_box(type_id) => {
                for field in fields {
                    self.consume(field, Sink::Escape);
                }
            }
            WirInstr::RefNull { .. } => {}
            WirInstr::StructGet { type_id, expr, .. } if self.places.is_box(type_id) => {
                self.consume(expr, Sink::Read);
            }
            WirInstr::StructSet {
                type_id,
                expr,
                value,
                ..
            } if self.places.is_box(type_id) => {
                let zeroes_place = matches!(**value, WirInstr::I32Const(0))
                    && matches!(**expr, WirInstr::LocalGet { .. });
                self.consume(
                    expr,
                    if zeroes_place {
                        Sink::Read
                    } else {
                        Sink::Escape
                    },
                );
                self.consume(value, Sink::Escape);
            }
            WirInstr::RefIsNull(expr) | WirInstr::Drop(expr) => self.consume(expr, Sink::Read),
            WirInstr::Seq(items) => self.consume_tail(items, sink),
            WirInstr::If {
                condition,
                result: Some(_),
                then_body,
                else_body: Some(else_body),
            } if matches!(sink, Sink::Flow(_)) && !exits(then_body) && !exits(else_body) => {
                self.consume(condition, Sink::Escape);
                self.consume_tail(then_body, sink);
                self.consume_tail(else_body, sink);
            }
            WirInstr::Block {
                result: Some(_),
                body,
                ..
            } if matches!(sink, Sink::Flow(_)) && !exits(body) => self.consume_tail(body, sink),
            WirInstr::Select {
                condition,
                if_true,
                if_false,
                ..
            } if matches!(sink, Sink::Flow(_)) => {
                self.consume(if_true, sink);
                self.consume(if_false, sink);
                self.consume(condition, Sink::Escape);
            }
            WirInstr::Call { func_id, args } => {
                for (index, arg) in args.iter().enumerate() {
                    let to = self.places.param(self.module, func_id, index);
                    self.consume(arg, to.map_or(Sink::Escape, Sink::Flow));
                }
                match self.places.call_result(self.module, func_id) {
                    Some(id) => self.place_to(id, sink),
                    None => self.opaque_to(sink),
                }
            }
            WirInstr::Return { value: Some(value) } => {
                let to = self.places.result(func).map_or(Sink::Escape, Sink::Flow);
                self.consume(value, to);
            }
            _ => {
                instr.for_each_child(&mut |child| self.consume(child, Sink::Escape));
                self.opaque_to(sink);
            }
        }
    }

    /// A body whose last instruction, unless it diverges, is its value.
    fn consume_tail(&mut self, body: &[WirInstr], sink: Sink) {
        let Some((last, init)) = body.split_last() else {
            return;
        };
        for item in init {
            self.consume(item, Sink::Escape);
        }
        let last_sink = if last.always_diverges() {
            Sink::Escape
        } else {
            sink
        };
        self.consume(last, last_sink);
    }

    /// Whether each place's class keeps no box.
    fn unboxed(&mut self) -> Vec<bool> {
        (0..self.parent.len())
            .map(|id| {
                let root = self.find(id);
                !self.escapes[root]
            })
            .collect()
    }
}

/// Whether a branch leaves `body`, a labeled body, carrying a value its tail
/// does not.
fn exits(body: &[WirInstr]) -> bool {
    body.iter().any(|instr| branches_at_or_beyond(instr, 0))
}

/// Give each function whose parameter or result is unboxed a signature taking
/// or returning the handle.
fn retype_signatures(module: &mut WirPackage, places: &Places, unboxed: &[bool]) {
    for func in 0..module.functions.len() {
        let function = &module.functions[func];
        let ft = module.types[function.type_id.index() as usize].expect_func();
        let mut params = ft.params.clone();
        for (param, name) in params.iter_mut().zip(&function.param_names) {
            if places.local(func, name).is_some_and(|id| unboxed[id]) {
                *param = WirType::I32;
            }
        }
        let mut results = ft.results.clone();
        if places.result(func).is_some_and(|id| unboxed[id]) {
            results = vec![WirType::I32];
        }
        if params == ft.params && results == ft.results {
            continue;
        }
        let new_ft = WirFuncType {
            name: ft.name.clone(),
            params,
            results,
        };
        let index = u32::try_from(module.types.len()).expect("type index fits u32");
        module.types.push(WirTypeDef::Func(new_ft));
        let function = &mut module.functions[func];
        function.type_id = WirTypeId::new(index, function.type_id.fq().into());
    }
}

struct Rewrite<'a> {
    module: &'a WirPackage,
    places: &'a Places,
    unboxed: &'a [bool],
    func: usize,
}

impl Rewrite<'_> {
    fn unboxed_local(&self, name: &str) -> bool {
        self.unboxed_id(self.places.local(self.func, name))
    }

    fn unboxed_id(&self, id: Option<usize>) -> bool {
        id.is_some_and(|id| self.unboxed[id])
    }

    /// Whether `instr` yields a handle rather than a box once rewritten.
    fn yields_handle(&self, instr: &WirInstr) -> bool {
        match instr {
            WirInstr::LocalGet { name, .. } | WirInstr::LocalTee { name, .. } => {
                self.unboxed_local(name)
            }
            WirInstr::Call { func_id, .. } => {
                self.unboxed_id(self.places.call_result(self.module, func_id))
            }
            WirInstr::Seq(items) => items.last().is_some_and(|last| self.yields_handle(last)),
            _ => false,
        }
    }

    fn rewrite(&mut self, instr: &mut WirInstr) {
        match instr {
            WirInstr::DeclareLocal { name, ty } if self.unboxed_local(name) => {
                *ty = WirType::I32;
            }
            WirInstr::LocalGet { name, result_ty } if self.unboxed_local(name) => {
                *result_ty = WirType::I32;
            }
            WirInstr::LocalSet { name, value } | WirInstr::LocalTee { name, value }
                if self.unboxed_local(name) =>
            {
                self.lower(value);
            }
            WirInstr::StructGet { type_id, expr, .. }
                if self.places.is_box(type_id) && self.yields_handle(expr) =>
            {
                self.rewrite(expr);
                let handle = std::mem::replace(&mut **expr, WirInstr::Nop);
                *instr = handle;
            }
            WirInstr::RefIsNull(expr) if self.yields_handle(expr) => {
                self.rewrite(expr);
                let handle = std::mem::replace(&mut **expr, WirInstr::Nop);
                *instr = WirInstr::I32Eqz(Box::new(handle));
            }
            WirInstr::StructSet { type_id, expr, .. }
                if self.places.is_box(type_id) && self.yields_handle(expr) =>
            {
                let WirInstr::LocalGet { name, .. } = &**expr else {
                    unreachable!("an unboxed class zeroes only a place");
                };
                *instr = WirInstr::LocalSet {
                    name: name.clone(),
                    value: Box::new(WirInstr::I32Const(0)),
                };
            }
            WirInstr::Call { func_id, args } => {
                for (index, arg) in args.iter_mut().enumerate() {
                    if self.unboxed_id(self.places.param(self.module, func_id, index)) {
                        self.lower(arg);
                    } else {
                        self.rewrite(arg);
                    }
                }
            }
            WirInstr::Return { value: Some(value) }
                if self.unboxed_id(self.places.result(self.func)) =>
            {
                self.lower(value);
            }
            _ => instr.for_each_boxed_child_mut(&mut |child| self.rewrite(child)),
        }
    }

    /// Rewrite `value`, which a place of an unboxed class receives, into the
    /// handle it holds.
    fn lower(&mut self, value: &mut WirInstr) {
        match value {
            WirInstr::StructNew { type_id, fields } if self.places.is_box(type_id) => {
                let [handle] = std::mem::take(fields)
                    .try_into()
                    .unwrap_or_else(|_| unreachable!("`$Resource` has one field"));
                *value = handle;
                self.rewrite(value);
            }
            WirInstr::RefNull { .. } => *value = WirInstr::I32Const(0),
            WirInstr::Seq(items) => self.lower_tail(items),
            WirInstr::If {
                condition,
                result,
                then_body,
                else_body: Some(else_body),
            } => {
                self.rewrite(condition);
                *result = Some(WirType::I32);
                self.lower_tail(then_body);
                self.lower_tail(else_body);
            }
            WirInstr::Block { result, body, .. } => {
                *result = Some(WirType::I32);
                self.lower_tail(body);
            }
            WirInstr::Select {
                condition,
                if_true,
                if_false,
                ty,
            } => {
                self.lower(if_true);
                self.lower(if_false);
                self.rewrite(condition);
                if ty.is_some() {
                    *ty = Some(WirType::I32);
                }
            }
            _ => {
                assert!(
                    self.yields_handle(value),
                    "an unboxed class receives only boxes it creates and handles"
                );
                self.rewrite(value);
            }
        }
    }

    /// Lower the value a body ends in, as `Scan::consume_tail` reads it.
    fn lower_tail(&mut self, body: &mut [WirInstr]) {
        let (last, init) = body.split_last_mut().expect("a value's body ends in it");
        for item in init {
            self.rewrite(item);
        }
        if last.always_diverges() {
            self.rewrite(last);
        } else {
            self.lower(last);
        }
    }
}
