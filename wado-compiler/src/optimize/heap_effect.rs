//! Which GC-heap objects a call may read or write: [`HeapEffectsCache`]
//! summarises each function over the call graph, [`HeapFrame`] one body's objects.

use std::cell::{OnceCell, RefCell};
use std::rc::Rc;

use cranelift_entity::EntityRef;

use crate::graph::strongly_connected_components;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::name::is_closure_call_name;
use crate::nir::{FuncId, FunctionRef, NirFunction, NirUnaryOp};
use crate::nir_arena::{Body, ExprId, ExprKind, NodeRef, Operand, PatId, PatKind, StmtKind};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::{OpaqueSource, ValueId, ValueKind};
use crate::tir::{
    BuiltinDeclaration, ResolvedType, RetainSpec, ReturnConvention, TypeId, TypeKey, TypeTable,
};

use super::arena_query::holds_reference;
use super::gate::FunctionGate;

/// A set of heap object types, keyed by [`TypeTable::type_key`]; `any` stands
/// for every type.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub(super) struct TypeSet {
    any: bool,
    keys: IndexSet<TypeKey>,
}

impl TypeSet {
    fn everything() -> Self {
        Self {
            any: true,
            keys: IndexSet::default(),
        }
    }

    fn one(key: TypeKey) -> Self {
        Self {
            any: false,
            keys: std::iter::once(key).collect(),
        }
    }

    pub(super) fn contains(&self, key: TypeKey) -> bool {
        self.any || self.keys.contains(&key)
    }

    pub(super) fn is_empty(&self) -> bool {
        !self.any && self.keys.is_empty()
    }

    fn insert(&mut self, key: TypeKey) -> bool {
        !self.any && self.keys.insert(key)
    }

    fn set_any(&mut self) -> bool {
        let changed = !self.any;
        self.any = true;
        self.keys.clear();
        changed
    }

    fn union(&mut self, other: &TypeSet) -> bool {
        if other.any {
            return self.set_any();
        }
        let mut changed = false;
        for &k in &other.keys {
            changed |= self.insert(k);
        }
        changed
    }

    /// Whether `self ∩ other` is non-empty.
    fn meets(&self, other: &TypeSet) -> bool {
        match (self.any, other.any) {
            (true, true) => true,
            (true, false) => !other.keys.is_empty(),
            (false, true) => !self.keys.is_empty(),
            (false, false) => self.keys.iter().any(|k| other.keys.contains(k)),
        }
    }

    /// `self ∪= a ∩ b`.
    fn union_meet(&mut self, a: &TypeSet, b: &TypeSet) {
        match (a.any, b.any) {
            (true, true) => {
                self.set_any();
            }
            (true, false) => {
                self.union(b);
            }
            (false, true) => {
                self.union(a);
            }
            (false, false) => {
                for &k in &a.keys {
                    if b.keys.contains(&k) {
                        self.insert(k);
                    }
                }
            }
        }
    }
}

/// Where a class's objects may come from: a bit per parameter (the last one
/// standing for every later one) or a global. None means allocated here.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Prov {
    elsewhere: bool,
    params: u64,
}

impl Prov {
    const ELSEWHERE: Self = Self {
        elsewhere: true,
        params: 0,
    };

    fn param(index: usize) -> Self {
        Self {
            elsewhere: false,
            params: 1 << index.min(63),
        }
    }

    fn has_param(self, index: usize) -> bool {
        self.params & (1 << index.min(63)) != 0
    }

    fn is_fresh(self) -> bool {
        !self.elsewhere && self.params == 0
    }

    fn join(&mut self, other: Self) {
        self.elsewhere |= other.elsewhere;
        self.params |= other.params;
    }
}

/// The object types an access reaches through the arguments, and elsewhere.
#[derive(Clone, Default, PartialEq, Debug)]
struct Access {
    through_args: TypeSet,
    elsewhere: TypeSet,
}

impl Access {
    fn record(&mut self, prov: Prov, keys: &TypeSet) {
        if prov.params != 0 {
            self.through_args.union(keys);
        }
        if prov.elsewhere {
            self.elsewhere.union(keys);
        }
    }

    fn record_meet(&mut self, prov: Prov, a: &TypeSet, b: &TypeSet) {
        if prov.params != 0 {
            self.through_args.union_meet(a, b);
        }
        if prov.elsewhere {
            self.elsewhere.union_meet(a, b);
        }
    }

    fn union(&mut self, other: &Access) -> bool {
        let a = self.through_args.union(&other.through_args);
        let b = self.elsewhere.union(&other.elsewhere);
        a || b
    }
}

/// What one function does to objects that existed before it was called, and
/// where the references it was given may end up.
#[derive(Clone, Default, PartialEq, Debug)]
struct Summary {
    reads: Access,
    writes: Access,
    /// What the result may share.
    ret: Prov,
    /// What each parameter's objects may come to hold.
    into_params: Vec<Prov>,
    /// What globals may come to hold.
    into_elsewhere: Prov,
}

impl Summary {
    fn join(&mut self, other: &Summary) -> bool {
        let before = self.clone();
        self.reads.union(&other.reads);
        self.writes.union(&other.writes);
        self.ret.join(other.ret);
        if self.into_params.len() < other.into_params.len() {
            self.into_params
                .resize(other.into_params.len(), Prov::default());
        }
        for (mine, theirs) in self.into_params.iter_mut().zip(&other.into_params) {
            mine.join(*theirs);
        }
        self.into_elsewhere.join(other.into_elsewhere);
        *self != before
    }

    fn access(&self, effect: Effect) -> &Access {
        match effect {
            Effect::Read => &self.reads,
            Effect::Write => &self.writes,
        }
    }
}

/// How a callee is summarised.
enum Callee {
    Body,
    /// A `core:builtin` declaration; `array` marks the array intrinsics, which
    /// write only the array they are handed.
    Builtin {
        declaration: Box<BuiltinDeclaration>,
        array: bool,
    },
    /// No body and nothing declared: it may read, write and keep anything it is
    /// handed.
    Opaque,
}

/// One function as the solver sees it, taken when its gate edit count was
/// `edit`.
struct FunctionEntry {
    edit: u64,
    callee: Callee,
    closure: bool,
    /// The functions its reachable calls name.
    calls: IndexSet<usize>,
    calls_indirect: bool,
}

impl FunctionEntry {
    fn of(f: &NirFunction, project: &NirPackage, edit: u64) -> Self {
        let mut calls = IndexSet::default();
        let mut calls_indirect = false;
        if let Some(body) = &f.body {
            body.for_each_reachable_node(|node| {
                let NodeRef::Expr(e) = node else { return };
                if let ExprKind::Call { func_id, .. } = &body.exprs[e].kind {
                    calls.insert(func_id.index());
                }
                calls_indirect |= matches!(body.exprs[e].kind, ExprKind::IndirectCall { .. });
            });
        }
        Self {
            edit,
            callee: classify_callee(f, project),
            closure: f.body.is_some() && is_closure_call_name(&f.name),
            calls,
            calls_indirect,
        }
    }

    /// What a caller reads of this entry besides its summary.
    fn shape(&self) -> (std::mem::Discriminant<Callee>, bool) {
        (std::mem::discriminant(&self.callee), self.closure)
    }
}

/// What the summaries read of the types beyond the type table, which only
/// grows while they are kept.
#[derive(Default, PartialEq)]
struct Layouts {
    struct_fields: IndexMap<(String, ModuleSource), Vec<TypeId>>,
    /// Every array type by its element, for the backing array a `List<T>` hides.
    arrays_of: IndexMap<TypeKey, Vec<TypeKey>>,
}

impl Layouts {
    fn of(project: &NirPackage, type_table: &TypeTable) -> Self {
        let struct_fields = project
            .structs
            .iter()
            .map(|s| {
                (
                    (s.name.clone(), s.module_source.clone()),
                    s.fields.iter().map(|f| f.type_id).collect(),
                )
            })
            .collect();
        let mut arrays_of: IndexMap<TypeKey, Vec<TypeKey>> = IndexMap::default();
        for (id, ty) in type_table.all_types() {
            if let ResolvedType::BuiltinArray(element) = ty {
                arrays_of
                    .entry(type_table.type_key(*element))
                    .or_default()
                    .push(type_table.type_key(id));
            }
        }
        Self {
            struct_fields,
            arrays_of,
        }
    }
}

/// The call-graph-wide summaries, kept across passes and re-solved only where
/// a body changed since: entries are keyed by one [`FunctionGate`]'s edit counts.
#[derive(Default)]
pub(super) struct HeapEffectsCache {
    gate: Option<u64>,
    layouts: Layouts,
    functions: Vec<FunctionEntry>,
    summaries: Vec<Summary>,
    /// The join over every closure body, which is what an indirect call runs.
    indirect: Summary,
    reach_memo: RefCell<IndexMap<TypeKey, Rc<TypeSet>>>,
    /// Where `assert_one_settled` resumes its rotation.
    #[cfg(debug_assertions)]
    cursor: usize,
}

impl HeapEffectsCache {
    /// The summaries of `project` as it stands. Every rewrite since the last
    /// call must have been reported to `gate`.
    pub(super) fn effects<'t>(
        &'t mut self,
        project: &NirPackage,
        type_table: &'t TypeTable,
        gate: &FunctionGate,
    ) -> HeapEffects<'t> {
        self.refresh(project, type_table, gate);
        HeapEffects {
            type_table,
            cache: self,
        }
    }

    fn refresh(&mut self, project: &NirPackage, type_table: &TypeTable, gate: &FunctionGate) {
        let layouts = Layouts::of(project, type_table);
        let len = project.functions.len();
        if self.gate != Some(gate.id()) || self.layouts != layouts || self.functions.len() > len {
            *self = Self {
                gate: Some(gate.id()),
                layouts,
                ..Self::default()
            };
        }
        let mut edited = vec![false; len];
        // Whether a node's answer to its callers moved; the last is the indirect one.
        let mut moved = vec![false; len + 1];
        for i in 0..len {
            let edit = gate.edits(FuncId::new(i));
            if self.functions.get(i).is_some_and(|e| e.edit == edit) {
                continue;
            }
            edited[i] = true;
            let entry = FunctionEntry::of(&project.functions[i].borrow(), project, edit);
            if let Some(old) = self.functions.get_mut(i) {
                moved[i] = old.shape() != entry.shape();
                *old = entry;
            } else {
                moved[i] = true;
                self.functions.push(entry);
                self.summaries.push(Summary::default());
            }
        }
        if edited.contains(&true) {
            self.solve(project, type_table, &edited, &mut moved);
        }
        #[cfg(debug_assertions)]
        self.assert_one_settled(project, type_table);
    }

    /// The least fixpoint over the call graph, one component at a time, callees
    /// first; a component whose inputs all stand is the fixpoint already.
    fn solve(
        &mut self,
        project: &NirPackage,
        type_table: &TypeTable,
        edited: &[bool],
        moved: &mut [bool],
    ) {
        let indirect = self.functions.len();
        let successors = self.successors();
        let components = strongly_connected_components(&successors);
        let mut component_of = vec![0; successors.len()];
        for (c, members) in components.iter().enumerate() {
            for &n in members {
                component_of[n] = c;
            }
        }
        let mut callers_within: Vec<Vec<usize>> = vec![Vec::new(); successors.len()];
        for (n, callees) in successors.iter().enumerate() {
            for &s in callees {
                if component_of[s] == component_of[n] {
                    callers_within[s].push(n);
                }
            }
        }
        let mut queued = vec![false; successors.len()];
        for members in &components {
            let stale = members
                .iter()
                .any(|&n| (n != indirect && edited[n]) || successors[n].iter().any(|&s| moved[s]));
            if !stale {
                continue;
            }
            let before: Vec<Summary> = members
                .iter()
                .map(|&n| std::mem::take(self.summary_mut(n)))
                .collect();
            let mut worklist = members.clone();
            for &n in members {
                queued[n] = true;
            }
            while let Some(n) = worklist.pop() {
                queued[n] = false;
                let summary = self.summarize(project, type_table, n);
                if summary == *self.summary_mut(n) {
                    continue;
                }
                *self.summary_mut(n) = summary;
                for &m in &callers_within[n] {
                    if !queued[m] {
                        queued[m] = true;
                        worklist.push(m);
                    }
                }
            }
            for (&n, before) in members.iter().zip(before) {
                moved[n] |= *self.summary_mut(n) != before;
            }
        }
    }

    /// Each node's callees, the node past the last function standing for what
    /// every indirect call runs.
    fn successors(&self) -> Vec<Vec<usize>> {
        let indirect = self.functions.len();
        let mut successors: Vec<Vec<usize>> = self
            .functions
            .iter()
            .map(|f| match f.callee {
                Callee::Body => f
                    .calls
                    .iter()
                    .copied()
                    .filter(|&c| c < indirect)
                    .chain(f.calls_indirect.then_some(indirect))
                    .collect(),
                Callee::Builtin { .. } | Callee::Opaque => Vec::new(),
            })
            .collect();
        successors.push(self.closure_bodies().collect());
        successors
    }

    fn closure_bodies(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.functions.len()).filter(|&i| self.functions[i].closure)
    }

    fn summary_mut(&mut self, n: usize) -> &mut Summary {
        if n == self.functions.len() {
            &mut self.indirect
        } else {
            &mut self.summaries[n]
        }
    }

    /// What node `n` gives its callers, read against the summaries as they stand.
    fn summarize(&self, project: &NirPackage, type_table: &TypeTable, n: usize) -> Summary {
        if n == self.functions.len() {
            let mut indirect = Summary::default();
            for c in self.closure_bodies() {
                indirect.join(&self.summaries[c]);
            }
            return indirect;
        }
        match self.functions[n].callee {
            Callee::Body => {
                let effects = HeapEffects {
                    type_table,
                    cache: self,
                };
                let f = project.functions[n].borrow();
                let body = f.body.as_ref().expect("a `Callee::Body` has a body");
                let params: Vec<u32> = f.params.iter().map(|p| p.local_index).collect();
                HeapFrame::new(&effects, body, &params).summary(&effects, body)
            }
            Callee::Builtin { .. } | Callee::Opaque => Summary::default(),
        }
    }

    /// A body rewritten without a report to the gate keeps a summary that no
    /// longer answers for it. One node per refresh, rotating, keeps the check cheap.
    #[cfg(debug_assertions)]
    fn assert_one_settled(&mut self, project: &NirPackage, type_table: &TypeTable) {
        let nodes = self.functions.len() + 1;
        self.cursor %= nodes;
        let n = self.cursor;
        self.cursor += 1;
        let summary = self.summarize(project, type_table, n);
        assert!(
            summary == *self.summary_mut(n),
            "a heap-effect summary went stale: function {n} was rewritten without \
             `FunctionGate::mark_changed`"
        );
    }
}

/// The call-graph-wide summaries every [`HeapFrame`] reads.
#[derive(Clone, Copy)]
pub(super) struct HeapEffects<'t> {
    type_table: &'t TypeTable,
    cache: &'t HeapEffectsCache,
}

impl HeapEffects<'_> {
    /// The heap object a value of `ty` is, or reaches first through its
    /// references: the key every access to it records.
    pub(super) fn object_key(&self, ty: TypeId) -> Option<TypeKey> {
        let tt = self.type_table;
        let ty = strip_handles(ty, tt);
        match tt.get(ty) {
            ResolvedType::Struct { .. }
            | ResolvedType::BuiltinArray(_)
            | ResolvedType::Variant { .. }
            | ResolvedType::Function { .. }
            | ResolvedType::Reactive(_) => Some(tt.type_key(ty)),
            ResolvedType::GenericInstance { .. } => Some(tt.type_key(tt.monomorphized_or_self(ty))),
            ResolvedType::Primitive(_)
            | ResolvedType::Unit
            | ResolvedType::Never
            | ResolvedType::Enum { .. }
            | ResolvedType::Resource { .. }
            | ResolvedType::GenericResource { .. }
            | ResolvedType::Flags { .. }
            | ResolvedType::TypeParam { .. }
            | ResolvedType::TypePack { .. }
            | ResolvedType::AssocTypeProjection { .. }
            | ResolvedType::Unknown
            | ResolvedType::Error => None,
            ResolvedType::Ref(_) | ResolvedType::MutRef(_) | ResolvedType::Newtype { .. } => {
                unreachable!("strip_handles removes every reference and newtype")
            }
            ResolvedType::InferVar(var) => panic!("{var} reached heap-effect analysis"),
        }
    }

    /// Whether what `ty` holds may be a reference: an array's elements, or
    /// anything else's contents.
    fn elements_hold_reference(&self, ty: TypeId) -> bool {
        let tt = self.type_table;
        let ResolvedType::BuiltinArray(element) = tt.get(strip_handles(ty, tt)) else {
            return true;
        };
        holds_reference(tt, *element)
    }

    /// Whether the retain `r` may carry a reference out of `args`.
    fn retains_reference(&self, body: &Body, r: &RetainSpec<usize>, args: &[Operand]) -> bool {
        !r.elements
            || args
                .get(r.source)
                .is_none_or(|&a| self.elements_hold_reference(body.operand_type(a)))
    }

    /// Each argument of `call`, in the callee's parameter order, with what the
    /// call keeps of it.
    pub(super) fn kept_args(&self, body: &Body, call: ExprId) -> Vec<(Operand, Kept)> {
        let (target, args) = call_parts(self, body, call);
        args.iter()
            .enumerate()
            .map(|(j, &a)| {
                let kept = match target {
                    Target::Summary(s) => Kept {
                        in_result: s.ret.has_param(j),
                        stored: s.into_elsewhere.has_param(j)
                            || s.into_params
                                .iter()
                                .enumerate()
                                .any(|(k, into)| k != j && into.has_param(j)),
                    },
                    Target::Builtin { declaration, .. } => {
                        let retains = || {
                            declaration
                                .retains
                                .iter()
                                .filter(|r| r.source == j && self.retains_reference(body, r, &args))
                        };
                        Kept {
                            in_result: returns_part_of(declaration, j)
                                || retains().any(|r| r.into.is_none()),
                            stored: retains().any(|r| r.into.is_some_and(|q| q != j)),
                        }
                    }
                    Target::Opaque => Kept {
                        in_result: true,
                        stored: true,
                    },
                };
                (a, kept)
            })
            .collect()
    }

    /// Every object type an element of the list `ty` may reach, or `None` where
    /// `ty` is no list.
    pub(super) fn element_reach(&self, ty: TypeId) -> Option<Rc<TypeSet>> {
        let tt = self.type_table;
        let ty = strip_handles(ty, tt);
        let ResolvedType::GenericInstance { type_args, .. } = tt.get(ty) else {
            return None;
        };
        let [element] = type_args.as_slice() else {
            return None;
        };
        tt.is_list(ty).then(|| self.reach(*element))
    }

    /// Every object type a value of `ty` may reach, itself included.
    pub(super) fn reach(&self, ty: TypeId) -> Rc<TypeSet> {
        let key = self.type_table.type_key(ty);
        if let Some(hit) = self.cache.reach_memo.borrow().get(&key) {
            return Rc::clone(hit);
        }
        let mut out = TypeSet::default();
        let mut seen = IndexSet::default();
        self.reach_into(ty, &mut seen, &mut out);
        let out = Rc::new(out);
        self.cache
            .reach_memo
            .borrow_mut()
            .insert(key, Rc::clone(&out));
        out
    }

    fn reach_into(&self, ty: TypeId, seen: &mut IndexSet<TypeKey>, out: &mut TypeSet) {
        if out.any {
            return;
        }
        let tt = self.type_table;
        let ty = strip_handles(ty, tt);
        if !seen.insert(tt.type_key(ty)) {
            return;
        }
        match tt.get(ty) {
            ResolvedType::Struct { .. } => {
                out.insert(tt.type_key(ty));
                match self.struct_field_types(ty) {
                    Some(fields) => {
                        for &f in fields {
                            self.reach_into(f, seen, out);
                        }
                    }
                    None => {
                        out.set_any();
                    }
                }
            }
            ResolvedType::GenericInstance { def, type_args } => {
                if let Some(mono) = tt.monomorphized_struct(ty) {
                    self.reach_into(mono, seen, out);
                } else if let Some(fields) = self.struct_field_types(ty) {
                    out.insert(tt.type_key(ty));
                    for &f in fields {
                        self.reach_into(f, seen, out);
                    }
                } else if let [element] = type_args.as_slice()
                    && tt.is_list(ty)
                {
                    out.insert(tt.type_key(ty));
                    for &array in self
                        .cache
                        .layouts
                        .arrays_of
                        .get(&tt.type_key(*element))
                        .into_iter()
                        .flatten()
                    {
                        out.insert(array);
                    }
                    self.reach_into(*element, seen, out);
                } else if let Some(elements) = tt.as_tuple(ty) {
                    out.insert(tt.type_key(ty));
                    for e in elements {
                        self.reach_into(e, seen, out);
                    }
                } else if let Some(cases) = tt.variant_template_cases(*def) {
                    out.insert(tt.type_key(ty));
                    for &(_, _, payload) in cases {
                        self.reach_template(payload, type_args, seen, out);
                    }
                } else {
                    out.set_any();
                }
            }
            ResolvedType::Variant { def } => {
                out.insert(tt.type_key(ty));
                match tt.variant_template_cases(*def) {
                    Some(cases) => {
                        for &(_, _, payload) in cases {
                            self.reach_template(payload, &[], seen, out);
                        }
                    }
                    None => {
                        out.set_any();
                    }
                }
            }
            ResolvedType::BuiltinArray(element) => {
                out.insert(tt.type_key(ty));
                self.reach_into(*element, seen, out);
            }
            // A closure's environment holds whatever it captured.
            ResolvedType::Function { .. }
            | ResolvedType::Reactive(_)
            | ResolvedType::TypeParam { .. }
            | ResolvedType::TypePack { .. }
            | ResolvedType::AssocTypeProjection { .. }
            | ResolvedType::Unknown
            | ResolvedType::Error => {
                out.set_any();
            }
            ResolvedType::Primitive(_)
            | ResolvedType::Unit
            | ResolvedType::Never
            | ResolvedType::Enum { .. }
            | ResolvedType::Resource { .. }
            | ResolvedType::GenericResource { .. }
            | ResolvedType::Flags { .. } => {}
            ResolvedType::Ref(_) | ResolvedType::MutRef(_) | ResolvedType::Newtype { .. } => {
                unreachable!("strip_handles removes every reference and newtype")
            }
            ResolvedType::InferVar(var) => panic!("{var} reached heap-effect analysis"),
        }
    }

    /// A variant case's payload, written over the declaration's parameters.
    fn reach_template(
        &self,
        payload: TypeId,
        args: &[TypeId],
        seen: &mut IndexSet<TypeKey>,
        out: &mut TypeSet,
    ) {
        let tt = self.type_table;
        if let ResolvedType::TypeParam { index, .. } = tt.get(payload) {
            match args.get(*index as usize) {
                Some(&arg) => self.reach_into(arg, seen, out),
                None => {
                    out.set_any();
                }
            }
        } else if let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) = tt.get(payload) {
            self.reach_template(*inner, args, seen, out);
        } else if tt.contains_type_param(payload) {
            out.set_any();
        } else {
            self.reach_into(payload, seen, out);
        }
    }

    /// A struct's field types, the instance of a generic one included.
    fn struct_field_types(&self, ty: TypeId) -> Option<&[TypeId]> {
        let tt = self.type_table;
        let key = if let ResolvedType::Struct { def, type_args } = tt.get(ty) {
            (
                tt.struct_rendered_name(*def, type_args),
                tt.struct_head_module(*def).clone(),
            )
        } else if let ResolvedType::GenericInstance { def, type_args } = tt.get(ty) {
            (
                tt.generic_rendered_name(*def, type_args),
                tt.def_module(*def).clone(),
            )
        } else {
            return None;
        };
        self.cache
            .layouts
            .struct_fields
            .get(&key)
            .map(Vec::as_slice)
    }

    /// The type of field `field` of the struct `ty` names, where it is known.
    fn pattern_field_type(&self, ty: Option<TypeId>, field: usize) -> Option<TypeId> {
        let ty = self
            .type_table
            .monomorphized_or_self(strip_handles(ty?, self.type_table));
        self.struct_field_types(ty)?.get(field).copied()
    }

    fn tuple_element_type(&self, ty: Option<TypeId>, index: usize) -> Option<TypeId> {
        let ty = strip_handles(ty?, self.type_table);
        self.type_table.as_tuple(ty)?.get(index).copied()
    }
}

fn classify_callee(f: &NirFunction, project: &NirPackage) -> Callee {
    if f.body.is_some() {
        return Callee::Body;
    }
    let reference = FunctionRef::from_resolved(f, f.module_source.clone());
    let Some(intrinsic) = reference.intrinsic() else {
        return Callee::Opaque;
    };
    let Some(declaration) = project.builtin_declarations.declaration(&reference) else {
        return Callee::Opaque;
    };
    let array = intrinsic.starts_with("array_");
    Callee::Builtin {
        declaration: Box::new(declaration.clone()),
        array,
    }
}

/// `ty` without its references and newtypes: the object a handle names.
fn strip_handles(mut ty: TypeId, tt: &TypeTable) -> TypeId {
    loop {
        if let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) = tt.get(ty) {
            ty = *inner;
        } else if let ResolvedType::Newtype { .. } = tt.get(ty) {
            ty = tt.representation_head(ty);
        } else {
            return ty;
        }
    }
}

/// Whether a call is asked about what it reads or what it writes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Effect {
    Read,
    Write,
}

/// The node an operand's objects belong to, where it has one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OperandNode {
    /// A scalar, or an object allocated where it is read: nothing else shares it.
    None,
    Node(u32),
    /// Built after the frame, so no class answers for it.
    Unknown,
}

enum Keys {
    One(TypeKey),
    Set(Rc<TypeSet>),
}

impl Keys {
    fn of(effects: &HeapEffects, ty: Option<TypeId>) -> Self {
        ty.and_then(|t| effects.object_key(t))
            .map_or_else(|| Keys::Set(Rc::new(TypeSet::everything())), Keys::One)
    }

    fn contains(&self, key: TypeKey) -> bool {
        match self {
            Keys::One(k) => *k == key,
            Keys::Set(set) => set.contains(key),
        }
    }

    fn meets(&self, set: &TypeSet) -> bool {
        match self {
            Keys::One(k) => set.contains(*k),
            Keys::Set(mine) => mine.meets(set),
        }
    }

    /// Whether some key is in `self`, `a` and `b` alike.
    fn meets_both(&self, a: &TypeSet, b: &TypeSet) -> bool {
        match self {
            Keys::One(k) => a.contains(*k) && b.contains(*k),
            Keys::Set(mine) => {
                let mut both = TypeSet::default();
                both.union_meet(a, b);
                mine.meets(&both)
            }
        }
    }
}

/// What a call keeps of one argument.
#[derive(Clone, Copy, Default, Debug)]
pub(super) struct Kept {
    /// The result may hold what the argument holds.
    pub(super) in_result: bool,
    /// The call may store it in a global or another argument.
    pub(super) stored: bool,
}

/// One read or write of an object: where it happens, through which operand,
/// and which field, where it names one.
struct AccessSite {
    effect: Effect,
    node: u32,
    keys: Keys,
    site: NodeRef,
    receiver: Option<Operand>,
    field: Option<u32>,
}

/// One body's objects as a union-find over locals and expressions, where a
/// class holds what flows together and carries the origins reaching it.
pub(super) struct HeapFrame {
    parent: Vec<u32>,
    prov: Vec<Prov>,
    local_nodes: Vec<u32>,
    /// Each parameter's node, in parameter order.
    params: Vec<u32>,
    expr_base: u32,
    expr_count: usize,
    /// Accesses to classify once every class is final.
    accesses: Vec<AccessSite>,
    calls: Vec<ExprId>,
    /// Assignment targets: written, not read.
    targets: IndexSet<ExprId>,
}

/// A body's [`HeapFrame`], built on the first query against the body as it
/// stands then.
pub(super) struct LazyHeapFrame<'e, 't> {
    pub(super) effects: &'e HeapEffects<'t>,
    params: Vec<u32>,
    frame: OnceCell<HeapFrame>,
}

impl<'e, 't> LazyHeapFrame<'e, 't> {
    /// For a body whose parameters are the locals `params`.
    pub(super) fn new(effects: &'e HeapEffects<'t>, params: Vec<u32>) -> Self {
        Self {
            effects,
            params,
            frame: OnceCell::new(),
        }
    }

    pub(super) fn get(&self, body: &Body) -> &HeapFrame {
        self.frame
            .get_or_init(|| HeapFrame::new(self.effects, body, &self.params))
    }
}

const NO_NODE: u32 = u32::MAX;
const ELSEWHERE: u32 = 0;
const RET: u32 = 1;

impl HeapFrame {
    /// The classes of `body`, whose parameters are the locals `params`.
    pub(super) fn new(effects: &HeapEffects, body: &Body, params: &[u32]) -> Self {
        let expr_count = body.exprs.len();
        let expr_base = 2;
        let mut frame = Self {
            parent: (0..expr_base + expr_count as u32).collect(),
            prov: vec![Prov::default(); expr_base as usize + expr_count],
            local_nodes: Vec::new(),
            params: Vec::with_capacity(params.len()),
            expr_base,
            expr_count,
            accesses: Vec::new(),
            calls: Vec::new(),
            targets: IndexSet::default(),
        };
        frame.prov[ELSEWHERE as usize] = Prov::ELSEWHERE;
        for (k, &local) in params.iter().enumerate() {
            let n = frame.local_node(local);
            frame.prov[n as usize].join(Prov::param(k));
            frame.params.push(n);
        }
        let mut nodes = Vec::new();
        body.for_each_reachable_node(|node| nodes.push(node));
        for node in nodes {
            let mut seen_values = IndexSet::default();
            body.for_each_operand(node, |op| {
                if let Some(v) = op.as_value() {
                    frame.value_reads(effects, body, node, v, &mut seen_values);
                }
            });
            match node {
                NodeRef::Expr(e) => frame.expr(effects, body, e),
                NodeRef::Stmt(s) => match &body.stmts[s].kind {
                    StmtKind::Let {
                        local_index, value, ..
                    } => {
                        let l = frame.local_node(*local_index);
                        frame.unify_op(effects, body, l, *value);
                    }
                    StmtKind::Return { value: Some(v) } => frame.unify_op(effects, body, RET, *v),
                    StmtKind::LetDestructure { pattern, value, .. } => {
                        let scrutinee = frame.operand_node(effects, body, *value);
                        let ty = body.operand_type(*value);
                        frame.pattern(effects, body, *pattern, scrutinee, Some(ty));
                    }
                    StmtKind::Expr(_)
                    | StmtKind::Return { value: None }
                    | StmtKind::If { .. }
                    | StmtKind::Loop { .. }
                    | StmtKind::Break { .. }
                    | StmtKind::Continue
                    | StmtKind::LabeledBlock { .. } => {}
                },
                NodeRef::Block(_) | NodeRef::Pat(_) => {}
            }
        }
        if let Some(tail) = body.block_tail(body.root) {
            frame.unify_op(effects, body, RET, tail);
        }
        frame.finish();
        frame
    }

    fn local_node(&mut self, local: u32) -> u32 {
        let i = local as usize;
        if i >= self.local_nodes.len() {
            self.local_nodes.resize(i + 1, NO_NODE);
        }
        if self.local_nodes[i] == NO_NODE {
            let n = self.parent.len() as u32;
            self.parent.push(n);
            self.prov.push(Prov::default());
            self.local_nodes[i] = n;
        }
        self.local_nodes[i]
    }

    fn expr_node(&self, e: ExprId) -> Option<u32> {
        (e.index() < self.expr_count).then(|| self.expr_base + e.index() as u32)
    }

    fn find(&mut self, mut n: u32) -> u32 {
        while self.parent[n as usize] != n {
            let grand = self.parent[self.parent[n as usize] as usize];
            self.parent[n as usize] = grand;
            n = grand;
        }
        n
    }

    fn unify(&mut self, a: u32, b: u32) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        let pb = self.prov[rb as usize];
        self.parent[rb as usize] = ra;
        self.prov[ra as usize].join(pb);
    }

    fn unify_op(&mut self, effects: &HeapEffects, body: &Body, n: u32, op: Operand) {
        if let OperandNode::Node(m) = self.operand_node(effects, body, op) {
            self.unify(n, m);
        }
    }

    fn unify_nodes(&mut self, a: OperandNode, b: OperandNode) {
        if let (OperandNode::Node(a), OperandNode::Node(b)) = (a, b) {
            self.unify(a, b);
        }
    }

    /// The node for an operand while building: a promoted value joins every
    /// local and expression it reads.
    fn operand_node(&mut self, effects: &HeapEffects, body: &Body, op: Operand) -> OperandNode {
        let tt = effects.type_table;
        match op {
            Operand::Expr(e) => {
                if !holds_reference(tt, body.exprs[e].type_id) {
                    return OperandNode::None;
                }
                self.expr_node(e)
                    .map_or(OperandNode::Unknown, OperandNode::Node)
            }
            Operand::Value(v) => {
                if body
                    .values
                    .type_of(v)
                    .is_some_and(|ty| !holds_reference(tt, ty))
                {
                    return OperandNode::None;
                }
                let mut leaves = Vec::new();
                for_each_value_source(body, v, &mut |source| {
                    leaves.push(match source {
                        Some(OpaqueSource::Local(l)) => self.local_node(l),
                        Some(OpaqueSource::Expr(e)) => self.expr_node(e).unwrap_or(ELSEWHERE),
                        None => ELSEWHERE,
                    });
                });
                let Some((&first, rest)) = leaves.split_first() else {
                    return OperandNode::None;
                };
                for &n in rest {
                    self.unify(first, n);
                }
                OperandNode::Node(first)
            }
        }
    }

    /// Record the field reads inside a promoted value.
    fn value_reads(
        &mut self,
        effects: &HeapEffects,
        body: &Body,
        site: NodeRef,
        v: ValueId,
        seen: &mut IndexSet<ValueId>,
    ) {
        if !seen.insert(v) {
            return;
        }
        match body.values.kind(v) {
            ValueKind::FieldAccess {
                receiver,
                field_index,
                ..
            } => {
                let (receiver, field) = (*receiver, *field_index);
                let keys = Keys::of(effects, body.values.type_of(receiver));
                self.record(
                    effects,
                    body,
                    Effect::Read,
                    keys,
                    site,
                    Operand::Value(receiver),
                    Some(field),
                );
                self.value_reads(effects, body, site, receiver, seen);
            }
            ValueKind::Unary { operand, .. } | ValueKind::Cast { operand, .. } => {
                self.value_reads(effects, body, site, *operand, seen);
            }
            ValueKind::Binary { lhs, rhs, .. } => {
                let (lhs, rhs) = (*lhs, *rhs);
                self.value_reads(effects, body, site, lhs, seen);
                self.value_reads(effects, body, site, rhs, seen);
            }
            ValueKind::Select { cond, then, else_ } => {
                let (cond, then, else_) = (*cond, *then, *else_);
                self.value_reads(effects, body, site, cond, seen);
                self.value_reads(effects, body, site, then, seen);
                self.value_reads(effects, body, site, else_, seen);
            }
            ValueKind::LoopPhi { entry, body_iter } => {
                let (entry, body_iter) = (*entry, *body_iter);
                self.value_reads(effects, body, site, entry, seen);
                self.value_reads(effects, body, site, body_iter, seen);
            }
            ValueKind::Opaque(_)
            | ValueKind::Int(..)
            | ValueKind::Float(..)
            | ValueKind::Bool(_)
            | ValueKind::Char(_)
            | ValueKind::Null
            | ValueKind::Unit
            | ValueKind::Const(..) => {}
        }
    }

    /// Record an access at `site` to the object `receiver` names.
    fn access(
        &mut self,
        effects: &HeapEffects,
        body: &Body,
        effect: Effect,
        site: ExprId,
        receiver: Operand,
        field: Option<u32>,
    ) {
        let keys = Keys::of(effects, Some(body.operand_type(receiver)));
        self.record(
            effects,
            body,
            effect,
            keys,
            NodeRef::Expr(site),
            receiver,
            field,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        effects: &HeapEffects,
        body: &Body,
        effect: Effect,
        keys: Keys,
        site: NodeRef,
        receiver: Operand,
        field: Option<u32>,
    ) {
        if let OperandNode::Node(node) = self.operand_node(effects, body, receiver) {
            self.accesses.push(AccessSite {
                effect,
                node,
                keys,
                site,
                receiver: Some(receiver),
                field,
            });
        }
    }

    fn expr(&mut self, effects: &HeapEffects, body: &Body, e: ExprId) {
        let node = self.expr_node(e).expect("built over this body");
        let yields = holds_reference(effects.type_table, body.exprs[e].type_id);
        match &body.exprs[e].kind {
            ExprKind::Local { index, .. } => {
                if yields {
                    let l = self.local_node(*index);
                    self.unify(node, l);
                }
            }
            ExprKind::GlobalVarGet { .. } => {
                if yields {
                    self.unify(node, ELSEWHERE);
                }
            }
            ExprKind::GlobalVarSet { value, .. } => self.unify_op(effects, body, ELSEWHERE, *value),
            ExprKind::Unary { op, expr: inner } => {
                if *op == NirUnaryOp::Deref && !self.targets.contains(&e) {
                    self.access(effects, body, Effect::Read, e, *inner, None);
                }
                if yields {
                    self.unify_op(effects, body, node, *inner);
                }
            }
            ExprKind::Binary { left, right, .. } => {
                if yields {
                    self.unify_op(effects, body, node, *left);
                    self.unify_op(effects, body, node, *right);
                }
            }
            ExprKind::Cast { expr: inner, .. }
            | ExprKind::ClosureToCanonical { functor: inner, .. } => {
                if yields {
                    self.unify_op(effects, body, node, *inner);
                }
            }
            ExprKind::Assign { target, value } => self.assign(effects, body, *target, *value),
            ExprKind::FieldAccess {
                expr: inner,
                field_index,
                ..
            } => {
                if !self.targets.contains(&e) {
                    self.access(effects, body, Effect::Read, e, *inner, Some(*field_index));
                }
                if yields {
                    self.unify_op(effects, body, node, *inner);
                }
            }
            ExprKind::Index { expr: inner, .. } | ExprKind::VariantPayload { expr: inner, .. } => {
                if !self.targets.contains(&e) {
                    self.access(effects, body, Effect::Read, e, *inner, None);
                }
                if yields {
                    self.unify_op(effects, body, node, *inner);
                }
            }
            ExprKind::VariantTag { expr: inner } | ExprKind::VariantTest { expr: inner, .. } => {
                self.access(effects, body, Effect::Read, e, *inner, None);
            }
            ExprKind::StructLiteral { fields, .. } => {
                for f in fields {
                    self.unify_op(effects, body, node, f.value);
                }
            }
            ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                for &el in elements {
                    self.unify_op(effects, body, node, el);
                }
            }
            ExprKind::VariantConstruct {
                payload: Some(p), ..
            } => self.unify_op(effects, body, node, *p),
            ExprKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                if yields {
                    for b in std::iter::once(*then_branch).chain(*else_branch) {
                        if let Some(tail) = body.block_tail(b) {
                            self.unify_op(effects, body, node, tail);
                        }
                    }
                }
            }
            ExprKind::Switch { arms, default, .. } => {
                if yields {
                    for &b in arms.iter().chain(std::iter::once(default)) {
                        if let Some(tail) = body.block_tail(b) {
                            self.unify_op(effects, body, node, tail);
                        }
                    }
                }
            }
            ExprKind::Match { expr, arms } => {
                let scrutinee = self.operand_node(effects, body, *expr);
                let ty = body.operand_type(*expr);
                for arm in arms {
                    self.pattern(effects, body, arm.pattern, scrutinee, Some(ty));
                    if yields {
                        self.unify_op(effects, body, node, arm.body);
                    }
                }
            }
            ExprKind::LabeledBlock { .. } => {
                if yields {
                    for exit in body.block_exits(e).into_iter().flatten().flatten() {
                        self.unify_op(effects, body, node, exit);
                    }
                }
            }
            ExprKind::Call { .. } | ExprKind::IndirectCall { .. } => {
                self.calls.push(e);
                self.call_flows(effects, body, e);
            }
            ExprKind::CmRawCall { .. }
            | ExprKind::VariantConstruct { payload: None, .. }
            | ExprKind::EnumConstruct { .. }
            | ExprKind::PackedArray(_)
            | ExprKind::Dead => {}
        }
    }

    fn assign(&mut self, effects: &HeapEffects, body: &Body, target: ExprId, value: Operand) {
        self.targets.insert(target);
        let target_node = self.operand_node(effects, body, Operand::Expr(target));
        let value_node = self.operand_node(effects, body, value);
        self.unify_nodes(target_node, value_node);
        let place = &body.exprs[target].kind;
        if let ExprKind::Local { .. } = place {
            return;
        }
        let field = if let ExprKind::FieldAccess { field_index, .. } = place {
            Some(*field_index)
        } else {
            None
        };
        if let ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::Index { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. }
        | ExprKind::Unary {
            op: NirUnaryOp::Deref,
            expr: inner,
        } = place
        {
            let receiver = self.operand_node(effects, body, *inner);
            self.unify_nodes(receiver, value_node);
            self.access(effects, body, Effect::Write, target, *inner, field);
        } else if let OperandNode::Node(node) = target_node {
            self.accesses.push(AccessSite {
                effect: Effect::Write,
                node,
                keys: Keys::of(effects, None),
                site: NodeRef::Expr(target),
                receiver: None,
                field: None,
            });
        }
    }

    fn pattern(
        &mut self,
        effects: &HeapEffects,
        body: &Body,
        pat: PatId,
        scrutinee: OperandNode,
        ty: Option<TypeId>,
    ) {
        let read = |frame: &mut Self, ty: Option<TypeId>| {
            if let OperandNode::Node(node) = scrutinee {
                frame.accesses.push(AccessSite {
                    effect: Effect::Read,
                    node,
                    keys: Keys::of(effects, ty),
                    site: NodeRef::Pat(pat),
                    receiver: None,
                    field: None,
                });
            }
        };
        match &body.pats[pat].kind {
            PatKind::Binding { local_index, .. } => {
                if let OperandNode::Node(n) = scrutinee {
                    let l = self.local_node(*local_index);
                    self.unify(l, n);
                }
            }
            PatKind::Tuple(elements, _) => {
                read(self, ty);
                for (i, &p) in elements.iter().enumerate() {
                    let sub = effects.tuple_element_type(ty, i);
                    self.pattern(effects, body, p, scrutinee, sub);
                }
            }
            PatKind::Struct {
                struct_type,
                fields,
                ..
            } => {
                read(self, Some(*struct_type));
                for f in fields {
                    let sub =
                        effects.pattern_field_type(Some(*struct_type), f.field_index as usize);
                    self.pattern(effects, body, f.pattern, scrutinee, sub);
                }
            }
            PatKind::Variant {
                bindings,
                payload_type,
                ..
            } => {
                read(self, ty);
                if let [one] = bindings.as_slice() {
                    self.pattern(effects, body, *one, scrutinee, Some(*payload_type));
                } else {
                    for (i, &p) in bindings.iter().enumerate() {
                        let sub = effects.tuple_element_type(Some(*payload_type), i);
                        self.pattern(effects, body, p, scrutinee, sub);
                    }
                }
            }
            PatKind::Or(alternatives) => {
                for &p in alternatives {
                    self.pattern(effects, body, p, scrutinee, ty);
                }
            }
            PatKind::Wildcard
            | PatKind::Literal(_)
            | PatKind::Enum { .. }
            | PatKind::ConstantValue { .. }
            | PatKind::Range { .. } => {}
        }
    }

    /// Join what a call makes share: its result with what the callee returns,
    /// and each argument with what the callee stores into it.
    fn call_flows(&mut self, effects: &HeapEffects, body: &Body, e: ExprId) {
        let result = self.operand_node(effects, body, Operand::Expr(e));
        let (target, args) = call_parts(effects, body, e);
        let nodes: Vec<OperandNode> = args
            .iter()
            .map(|&a| self.operand_node(effects, body, a))
            .collect();
        match target {
            Target::Summary(s) => {
                for (j, &n) in nodes.iter().enumerate() {
                    if s.ret.has_param(j) {
                        self.unify_nodes(result, n);
                    }
                    if s.into_elsewhere.has_param(j) {
                        self.unify_nodes(n, OperandNode::Node(ELSEWHERE));
                    }
                }
                if s.ret.elsewhere {
                    self.unify_nodes(result, OperandNode::Node(ELSEWHERE));
                }
                for (k, into) in s.into_params.iter().enumerate() {
                    let Some(&nk) = nodes.get(k) else { continue };
                    if into.elsewhere {
                        self.unify_nodes(nk, OperandNode::Node(ELSEWHERE));
                    }
                    for (j, &nj) in nodes.iter().enumerate() {
                        if j != k && into.has_param(j) {
                            self.unify_nodes(nk, nj);
                        }
                    }
                }
            }
            Target::Builtin { declaration, .. } => {
                for r in &declaration.retains {
                    if !effects.retains_reference(body, r, &args) {
                        continue;
                    }
                    let source = nodes.get(r.source).copied().unwrap_or(OperandNode::None);
                    let into = match r.into {
                        Some(q) => nodes.get(q).copied().unwrap_or(OperandNode::None),
                        None => result,
                    };
                    self.unify_nodes(into, source);
                }
                for (j, &n) in nodes.iter().enumerate() {
                    if returns_part_of(declaration, j) {
                        self.unify_nodes(result, n);
                    }
                }
            }
            Target::Opaque => {
                for &n in &nodes {
                    self.unify_nodes(result, n);
                }
                if let OperandNode::None | OperandNode::Unknown = result {
                    for pair in nodes.windows(2) {
                        self.unify_nodes(pair[0], pair[1]);
                    }
                }
            }
        }
    }

    fn finish(&mut self) {
        for n in 0..self.parent.len() as u32 {
            let r = self.find(n);
            self.parent[n as usize] = r;
        }
    }

    fn class_prov(&self, n: u32) -> Prov {
        self.prov[self.parent[n as usize] as usize]
    }

    /// The summary this frame's body gives its callers.
    fn summary(&self, effects: &HeapEffects, body: &Body) -> Summary {
        let mut s = Summary {
            ret: self.class_prov(RET),
            into_params: self.params.iter().map(|&n| self.class_prov(n)).collect(),
            into_elsewhere: self.class_prov(ELSEWHERE),
            ..Summary::default()
        };
        for AccessSite {
            effect, node, keys, ..
        } in &self.accesses
        {
            let prov = self.class_prov(*node);
            let access = match effect {
                Effect::Read => &mut s.reads,
                Effect::Write => &mut s.writes,
            };
            match keys {
                Keys::One(k) => access.record(prov, &TypeSet::one(*k)),
                Keys::Set(set) => access.record(prov, set),
            }
        }
        for &call in &self.calls {
            self.call_summary(effects, body, call, &mut s);
        }
        s
    }

    /// Add what `call` does to objects the caller did not allocate.
    fn call_summary(&self, effects: &HeapEffects, body: &Body, call: ExprId, s: &mut Summary) {
        let (target, args) = call_parts(effects, body, call);
        if let Target::Summary(t) = target {
            s.reads.elsewhere.union(&t.reads.elsewhere);
            s.writes.elsewhere.union(&t.writes.elsewhere);
        }
        for (j, &a) in args.iter().enumerate() {
            let OperandNode::Node(n) = self.lookup(effects, body, a) else {
                continue;
            };
            let prov = self.prov[n as usize];
            if prov.is_fresh() {
                continue;
            }
            let ty = body.operand_type(a);
            match target {
                Target::Summary(t) => {
                    let reach = effects.reach(ty);
                    s.reads.record_meet(prov, &t.reads.through_args, &reach);
                    s.writes.record_meet(prov, &t.writes.through_args, &reach);
                }
                Target::Builtin { declaration, array } => {
                    let touched = builtin_touches(effects, ty, array);
                    s.reads.record(prov, &touched);
                    if declaration.mut_params.contains(&j) {
                        s.writes.record(prov, &touched);
                    }
                }
                Target::Opaque => {
                    let reach = effects.reach(ty);
                    s.reads.record(prov, &reach);
                    s.writes.record(prov, &reach);
                }
            }
        }
    }

    /// The class root an operand's objects belong to, without joining anything.
    fn lookup(&self, effects: &HeapEffects, body: &Body, op: Operand) -> OperandNode {
        let tt = effects.type_table;
        match op {
            Operand::Expr(e) => {
                if !holds_reference(tt, body.exprs[e].type_id) {
                    return OperandNode::None;
                }
                self.expr_node(e).map_or(OperandNode::Unknown, |n| {
                    OperandNode::Node(self.parent[n as usize])
                })
            }
            Operand::Value(v) => {
                if body
                    .values
                    .type_of(v)
                    .is_some_and(|ty| !holds_reference(tt, ty))
                {
                    return OperandNode::None;
                }
                let mut roots = IndexSet::default();
                let mut unknown = false;
                for_each_value_source(body, v, &mut |source| {
                    let root = match source {
                        Some(OpaqueSource::Local(l)) => self.local_root(l),
                        Some(OpaqueSource::Expr(e)) => {
                            self.expr_node(e).map(|n| self.parent[n as usize])
                        }
                        None => Some(self.parent[ELSEWHERE as usize]),
                    };
                    match root {
                        Some(r) => {
                            roots.insert(r);
                        }
                        None => unknown = true,
                    }
                });
                if unknown || roots.len() > 1 {
                    return OperandNode::Unknown;
                }
                roots
                    .first()
                    .map_or(OperandNode::None, |&r| OperandNode::Node(r))
            }
        }
    }

    fn local_root(&self, local: u32) -> Option<u32> {
        let n = *self.local_nodes.get(local as usize)?;
        (n != NO_NODE).then(|| self.parent[n as usize])
    }

    /// Whether objects in class `n` may be ones the local's class `h` holds:
    /// the same class, or two that both reach outside the body.
    fn shares(&self, h: Option<u32>, n: OperandNode) -> bool {
        match (h, n) {
            (_, OperandNode::None) => false,
            (None, _) | (_, OperandNode::Unknown) => true,
            (Some(h), OperandNode::Node(n)) => {
                n == h || (!self.prov[h as usize].is_fresh() && !self.prov[n as usize].is_fresh())
            }
        }
    }

    /// Whether `call` may read or write an object of type `key` that the
    /// object `local` holds is, or reaches.
    pub(super) fn call_may(
        &self,
        effects: &HeapEffects,
        body: &Body,
        call: ExprId,
        effect: Effect,
        key: TypeKey,
        local: u32,
    ) -> bool {
        self.call_may_besides(effects, body, call, effect, key, local, |_| false)
    }

    /// [`Self::call_may`], leaving out the arguments `answered` accepts: the
    /// ones a caller accounts for on its own.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn call_may_besides(
        &self,
        effects: &HeapEffects,
        body: &Body,
        call: ExprId,
        effect: Effect,
        key: TypeKey,
        local: u32,
        answered: impl Fn(Operand) -> bool,
    ) -> bool {
        self.call_may_keys(
            effects,
            body,
            call,
            effect,
            &Keys::One(key),
            local,
            &answered,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn call_may_keys(
        &self,
        effects: &HeapEffects,
        body: &Body,
        call: ExprId,
        effect: Effect,
        keys: &Keys,
        local: u32,
        answered: &impl Fn(Operand) -> bool,
    ) -> bool {
        let h = self.local_root(local);
        let h_escapes = h.is_none_or(|r| !self.prov[r as usize].is_fresh());
        let (target, args) = call_parts(effects, body, call);
        if let Target::Summary(t) = target
            && h_escapes
            && keys.meets(&t.access(effect).elsewhere)
        {
            return true;
        }
        args.iter().enumerate().any(|(j, &a)| {
            if answered(a) || !self.shares(h, self.lookup(effects, body, a)) {
                return false;
            }
            let ty = body.operand_type(a);
            match target {
                Target::Summary(t) => {
                    keys.meets_both(&t.access(effect).through_args, &effects.reach(ty))
                }
                Target::Builtin { declaration, array } => {
                    (effect == Effect::Read || declaration.mut_params.contains(&j))
                        && keys.meets(&builtin_touches(effects, ty, array))
                }
                Target::Opaque => keys.meets(&effects.reach(ty)),
            }
        })
    }

    /// Whether a store, or a call through an argument `answered` rejects, may
    /// write a `keys` object that `local` holds or reaches.
    pub(super) fn written(
        &self,
        effects: &HeapEffects,
        body: &Body,
        keys: &Rc<TypeSet>,
        local: u32,
        answered: impl Fn(Operand) -> bool,
    ) -> bool {
        let h = self.local_root(local);
        let wanted = Keys::Set(Rc::clone(keys));
        self.accesses.iter().any(|a| {
            a.effect == Effect::Write
                && a.keys.meets(keys)
                && self.shares(h, OperandNode::Node(self.parent[a.node as usize]))
        }) || self.calls.iter().any(|&call| {
            self.call_may_keys(
                effects,
                body,
                call,
                Effect::Write,
                &wanted,
                local,
                &answered,
            )
        })
    }

    /// Whether objects `local` holds stay reachable after the body returns:
    /// through its result, a global, or a parameter they did not come from.
    pub(super) fn outlives(&self, local: u32) -> bool {
        let Some(h) = self.local_root(local) else {
            return true;
        };
        let prov = self.prov[h as usize];
        !prov.is_fresh()
            && (h == self.parent[RET as usize]
                || h == self.parent[ELSEWHERE as usize]
                || prov.params.count_ones() > 1)
    }

    /// Whether an access at a site `at` accepts, other than one through a
    /// receiver `own` accepts, may reach `field` of the `key` object `local` holds.
    pub(super) fn accessed_besides(
        &self,
        writes_only: bool,
        at: impl Fn(NodeRef) -> bool,
        (key, field): (TypeKey, u32),
        local: u32,
        own: impl Fn(Operand) -> bool,
    ) -> bool {
        let h = self.local_root(local);
        self.accesses.iter().any(|a| {
            (!writes_only || a.effect == Effect::Write)
                && at(a.site)
                && a.keys.contains(key)
                && a.field.is_none_or(|f| f == field)
                && !(a.field.is_some() && a.receiver.is_some_and(&own))
                && self.shares(h, OperandNode::Node(self.parent[a.node as usize]))
        })
    }

    /// Whether a store or a call at a site `at` accepts may put a new object on
    /// the place `local` then `path` names (see [`field_path`]).
    pub(super) fn place_replaced(
        &self,
        effects: &HeapEffects,
        body: &Body,
        local: u32,
        path: &[(TypeId, u32)],
        at: impl Fn(NodeRef) -> bool,
    ) -> bool {
        path.iter().any(|&(holder, field)| {
            let Some(key) = effects.object_key(holder) else {
                return true;
            };
            self.accessed_besides(true, &at, (key, field), local, |_| false)
                || self.calls.iter().any(|&call| {
                    at(NodeRef::Expr(call))
                        && self.call_may(effects, body, call, Effect::Write, key, local)
                })
        })
    }
}

/// The expression a field chain starts from, and each object on the chain with
/// the field taken from it.
pub(super) fn field_path(body: &Body, mut e: ExprId) -> Option<(ExprId, Vec<(TypeId, u32)>)> {
    let mut path = Vec::new();
    while let ExprKind::FieldAccess {
        expr: inner,
        field_index,
        ..
    } = &body.exprs[e].kind
    {
        let inner = inner.as_expr()?;
        path.push((body.exprs[inner].type_id, *field_index));
        e = inner;
    }
    Some((e, path))
}

/// What a builtin touches through an argument of type `ty`: an array intrinsic
/// only the array, anything else all it reaches.
fn builtin_touches(effects: &HeapEffects, ty: TypeId, array: bool) -> Rc<TypeSet> {
    if !array {
        return effects.reach(ty);
    }
    Rc::new(
        effects
            .object_key(ty)
            .map_or_else(TypeSet::everything, TypeSet::one),
    )
}

/// Visit where each opaque leaf of the promoted value `v` came from; `None` is
/// a leaf with no recorded source.
fn for_each_value_source(body: &Body, v: ValueId, f: &mut impl FnMut(Option<OpaqueSource>)) {
    let mut stack = vec![v];
    let mut seen = IndexSet::default();
    while let Some(v) = stack.pop() {
        if !seen.insert(v) {
            continue;
        }
        match body.values.kind(v) {
            ValueKind::Opaque(oid) => f(body.values.opaque_source(*oid)),
            ValueKind::FieldAccess { receiver, .. } => stack.push(*receiver),
            ValueKind::Unary { operand, .. } | ValueKind::Cast { operand, .. } => {
                stack.push(*operand);
            }
            ValueKind::Binary { lhs, rhs, .. } => stack.extend([*lhs, *rhs]),
            ValueKind::Select { then, else_, .. } => stack.extend([*then, *else_]),
            ValueKind::LoopPhi { entry, body_iter } => stack.extend([*entry, *body_iter]),
            ValueKind::Int(..)
            | ValueKind::Float(..)
            | ValueKind::Bool(_)
            | ValueKind::Char(_)
            | ValueKind::Null
            | ValueKind::Unit
            | ValueKind::Const(..) => {}
        }
    }
}

/// Whether a builtin's result may be part of its argument `j`.
fn returns_part_of(declaration: &BuiltinDeclaration, j: usize) -> bool {
    match declaration.returns {
        Some(ReturnConvention::Owned) => false,
        Some(ReturnConvention::PartOf(p)) => p == j,
        None => true,
    }
}

/// What a call runs, as far as its heap effects go.
enum Target<'s> {
    Summary(&'s Summary),
    Builtin {
        declaration: &'s BuiltinDeclaration,
        array: bool,
    },
    Opaque,
}

/// The call's target and its arguments in the callee's parameter order; an
/// indirect call's closure environment is parameter 0.
fn call_parts<'s>(effects: &'s HeapEffects, body: &Body, e: ExprId) -> (Target<'s>, Vec<Operand>) {
    match &body.exprs[e].kind {
        ExprKind::Call { func_id, args, .. } => {
            let cache = effects.cache;
            let target = match cache.functions.get(func_id.index()).map(|f| &f.callee) {
                Some(Callee::Body) => Target::Summary(&cache.summaries[func_id.index()]),
                Some(Callee::Builtin { declaration, array }) => Target::Builtin {
                    declaration,
                    array: *array,
                },
                Some(Callee::Opaque) | None => Target::Opaque,
            };
            (target, args.iter().map(|a| a.expr).collect())
        }
        ExprKind::IndirectCall { callee, args } => (
            Target::Summary(&effects.cache.indirect),
            std::iter::once(*callee)
                .chain(args.iter().copied())
                .collect(),
        ),
        other => panic!("call_parts on a non-call {other:?}"),
    }
}
