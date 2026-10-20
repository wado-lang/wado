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

use crate::module_source::ModuleSource;
use crate::name::{RESOURCE_OBJECT_STRUCT, wir_type_key};
use crate::wir::{WirFuncType, WirInstr, WirPackage, WirType, WirTypeDef, WirTypeId};

use indexmap::{IndexMap, IndexSet};

use super::util::collect_pinned_func_ids;

/// A place a resource lives in: a local or parameter of the function at a
/// position, or that function's result.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Place {
    Local(usize, String),
    Result(usize),
}

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
    if places.ids.is_empty() {
        return;
    }
    let mut scan = Scan {
        module,
        handle_box,
        places: &places,
        parent: (0..places.ids.len()).collect(),
        escapes: vec![false; places.ids.len()],
        func: 0,
    };
    for id in places.pinned.iter().copied() {
        scan.escapes[id] = true;
    }
    for (func, function) in module.functions.iter().enumerate() {
        scan.func = func;
        for instr in function.body.as_deref().unwrap_or(&[]) {
            scan.consume(instr, Sink::Escape);
        }
    }
    let unboxed = scan.unboxed();
    if unboxed.is_empty() {
        return;
    }
    retype_signatures(module, &places, &unboxed);
    let mut bodies: Vec<_> = module.functions.iter_mut().map(|f| f.body.take()).collect();
    for (func, body) in bodies.iter_mut().enumerate() {
        let mut rewrite = Rewrite {
            module,
            handle_box,
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
    module.types.iter().position(|def| {
        matches!(def, WirTypeDef::Struct(s) if s.name.fq == fq)
    }).map(|index| u32::try_from(index).expect("type index fits u32"))
}

fn is_handle_box(ty: &WirType, handle_box: u32) -> bool {
    matches!(ty, WirType::Ref { type_id, .. } if type_id.index() == handle_box)
}

/// Every place of `$Resource` type, numbered, and the ones whose function a
/// caller outside this package's direct calls reaches.
struct Places {
    ids: IndexMap<Place, usize>,
    pinned: Vec<usize>,
}

impl Places {
    fn collect(module: &WirPackage, handle_box: u32) -> Self {
        let pinned_funcs = collect_pinned_func_ids(module);
        let mut ids: IndexMap<Place, usize> = IndexMap::default();
        let mut pinned = Vec::new();
        for (func, function) in module.functions.iter().enumerate() {
            let ft = module.types[function.type_id.index() as usize].expect_func();
            let is_pinned = function.body.is_none()
                || pinned_funcs.contains(&module.defined_func_index(func));
            let mut add = |place: Place, ids: &mut IndexMap<Place, usize>| {
                let id = ids.len();
                ids.insert(place, id);
                if is_pinned {
                    pinned.push(id);
                }
            };
            for (name, ty) in function.param_names.iter().zip(&ft.params) {
                if is_handle_box(ty, handle_box) {
                    add(Place::Local(func, name.clone()), &mut ids);
                }
            }
            if let [result] = ft.results.as_slice()
                && is_handle_box(result, handle_box)
            {
                add(Place::Result(func), &mut ids);
            }
            for (name, ty) in function.declared_locals().iter() {
                if is_handle_box(ty, handle_box) {
                    let id = ids.len();
                    ids.insert(Place::Local(func, name.to_string()), id);
                }
            }
        }
        Self { ids, pinned }
    }

    fn local(&self, func: usize, name: &str) -> Option<usize> {
        self.ids.get(&Place::Local(func, name.to_string())).copied()
    }

    fn result(&self, func: usize) -> Option<usize> {
        self.ids.get(&Place::Result(func)).copied()
    }

    fn param(&self, module: &WirPackage, callee: usize, index: usize) -> Option<usize> {
        let name = module.functions[callee].param_names.get(index)?;
        self.local(callee, name)
    }
}

/// Joins places into classes by what flows between them, and marks the classes
/// a box escapes from.
struct Scan<'a> {
    module: &'a WirPackage,
    handle_box: u32,
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

    fn callee(&self, func_id: u32) -> Option<usize> {
        self.module.defined_func_position(func_id)
    }

    fn is_box(&self, type_id: &WirTypeId) -> bool {
        type_id.index() == self.handle_box
    }

    fn consume(&mut self, instr: &WirInstr, sink: Sink) {
        let func = self.func;
        match instr {
            WirInstr::LocalGet { name, .. } => match self.places.local(func, name) {
                Some(id) => self.place_to(id, sink),
                None => self.opaque_to(sink),
            },
            WirInstr::LocalTee { name, value } => match self.places.local(func, name) {
                Some(id) => {
                    self.consume(value, Sink::Flow(id));
                    self.place_to(id, sink);
                }
                None => {
                    self.consume(value, Sink::Escape);
                    self.opaque_to(sink);
                }
            },
            WirInstr::LocalSet { name, value } => {
                let to = self.places.local(func, name).map_or(Sink::Escape, Sink::Flow);
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
            WirInstr::StructNew { type_id, fields } if self.is_box(type_id) => {
                for field in fields {
                    self.consume(field, Sink::Escape);
                }
            }
            WirInstr::RefNull { .. } => {}
            WirInstr::StructGet { type_id, expr, .. } if self.is_box(type_id) => {
                self.consume(expr, Sink::Read);
            }
            WirInstr::StructSet {
                type_id,
                expr,
                value,
                ..
            } if self.is_box(type_id) => {
                let zeroes_place = matches!(**value, WirInstr::I32Const(0))
                    && matches!(**expr, WirInstr::LocalGet { .. });
                self.consume(expr, if zeroes_place { Sink::Read } else { Sink::Escape });
                self.consume(value, Sink::Escape);
            }
            WirInstr::RefIsNull(expr) | WirInstr::Drop(expr) => self.consume(expr, Sink::Read),
            WirInstr::Seq(items) => {
                if let Some((last, init)) = items.split_last() {
                    for item in init {
                        self.consume(item, Sink::Escape);
                    }
                    self.consume(last, sink);
                }
            }
            WirInstr::Call { func_id, args } => {
                let callee = self.callee(func_id.index());
                for (index, arg) in args.iter().enumerate() {
                    let to = callee.and_then(|c| self.places.param(self.module, c, index));
                    self.consume(arg, to.map_or(Sink::Escape, Sink::Flow));
                }
                match callee.and_then(|c| self.places.result(c)) {
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

    /// The places whose class no box escapes from.
    fn unboxed(&mut self) -> IndexSet<usize> {
        (0..self.parent.len())
            .filter(|&id| {
                let root = self.find(id);
                !self.escapes[root]
            })
            .collect()
    }
}

/// Give each function whose parameter or result is unboxed a signature taking
/// or returning the handle.
fn retype_signatures(module: &mut WirPackage, places: &Places, unboxed: &IndexSet<usize>) {
    for func in 0..module.functions.len() {
        let function = &module.functions[func];
        let ft = module.types[function.type_id.index() as usize].expect_func();
        let mut params = ft.params.clone();
        for (param, name) in params.iter_mut().zip(&function.param_names) {
            if places.local(func, name).is_some_and(|id| unboxed.contains(&id)) {
                *param = WirType::I32;
            }
        }
        let mut results = ft.results.clone();
        if places.result(func).is_some_and(|id| unboxed.contains(&id)) {
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
    handle_box: u32,
    places: &'a Places,
    unboxed: &'a IndexSet<usize>,
    func: usize,
}

impl Rewrite<'_> {
    fn unboxed_local(&self, name: &str) -> bool {
        self.places
            .local(self.func, name)
            .is_some_and(|id| self.unboxed.contains(&id))
    }

    fn unboxed_id(&self, id: Option<usize>) -> bool {
        id.is_some_and(|id| self.unboxed.contains(&id))
    }

    fn callee(&self, func_id: u32) -> Option<usize> {
        self.module.defined_func_position(func_id)
    }

    /// Whether `instr` yields a handle rather than a box once rewritten.
    fn yields_handle(&self, instr: &WirInstr) -> bool {
        match instr {
            WirInstr::LocalGet { name, .. } | WirInstr::LocalTee { name, .. } => {
                self.unboxed_local(name)
            }
            WirInstr::Call { func_id, .. } => {
                self.unboxed_id(self.callee(func_id.index()).and_then(|c| self.places.result(c)))
            }
            WirInstr::Seq(items) => items.last().is_some_and(|last| self.yields_handle(last)),
            _ => false,
        }
    }

    fn is_box(&self, type_id: &WirTypeId) -> bool {
        type_id.index() == self.handle_box
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
                if self.is_box(type_id) && self.yields_handle(expr) =>
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
                if self.is_box(type_id) && self.yields_handle(expr) =>
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
                let callee = self.callee(func_id.index());
                for (index, arg) in args.iter_mut().enumerate() {
                    let param = callee.and_then(|c| self.places.param(self.module, c, index));
                    if self.unboxed_id(param) {
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
            WirInstr::StructNew { type_id, fields } if self.is_box(type_id) => {
                let [handle] = std::mem::take(fields)
                    .try_into()
                    .unwrap_or_else(|_| unreachable!("`$Resource` has one field"));
                *value = handle;
                self.rewrite(value);
            }
            WirInstr::RefNull { .. } => *value = WirInstr::I32Const(0),
            WirInstr::Seq(items) => {
                let (last, init) = items.split_last_mut().expect("a value's sequence ends in it");
                for item in init {
                    self.rewrite(item);
                }
                self.lower(last);
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
}
