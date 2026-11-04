//! Redundant load elimination: a `struct.get` chain off a local, read again
//! where nothing between could have changed it, reuses the first read through a
//! temp. wasmtime null-checks and reloads every `struct.get` it is given.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirInstr, WirPackage, WirType, WirTypeId};

use super::util::{CalleeWrites, Footprint, arm_callees};

/// A field chain read off a local: `root.f0.f1…`, outermost field last.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    root: String,
    fields: Vec<(WirTypeId, String)>,
}

impl Key {
    fn of(instr: &WirInstr) -> Option<Self> {
        let mut fields = Vec::new();
        let mut cur = instr;
        while let WirInstr::StructGet {
            type_id,
            field_name,
            expr,
            ..
        } = cur
        {
            fields.push((type_id.clone(), field_name.clone()));
            cur = expr;
        }
        let WirInstr::LocalGet { name, .. } = cur else {
            return None;
        };
        if fields.is_empty() {
            return None;
        }
        fields.reverse();
        Some(Self {
            root: name.clone(),
            fields,
        })
    }
}

/// Chains whose value a temp can supply here, each with its defining temp.
///
/// A chain is named by its position in the function's [`Key`] table, so the
/// clone each `Block`, `Loop` and `If` takes copies no `Key`.
#[derive(Clone, Default)]
struct Avail {
    chains: IndexMap<usize, usize>,
    /// The chains read off each local, and through each field, so a write
    /// reaches the chains it ends and no others. They hold exactly the keys of
    /// `chains`, as every `If` clones them.
    by_root: IndexMap<String, IndexSet<usize>>,
    by_field: IndexMap<String, IndexSet<usize>>,
}

impl Avail {
    fn get(&self, chain: usize) -> Option<usize> {
        self.chains.get(&chain).copied()
    }

    fn insert(&mut self, keys: &IndexSet<Key>, chain: usize, id: usize) {
        let key = &keys[chain];
        self.by_root
            .entry(key.root.clone())
            .or_default()
            .insert(chain);
        for (_, field) in &key.fields {
            self.by_field
                .entry(field.clone())
                .or_default()
                .insert(chain);
        }
        self.chains.insert(chain, id);
    }

    fn retain(&mut self, keys: &IndexSet<Key>, mut keep: impl FnMut(usize, usize) -> bool) {
        let dropped: Vec<usize> = self
            .chains
            .iter()
            .filter(|(chain, id)| !keep(**chain, **id))
            .map(|(chain, _)| *chain)
            .collect();
        for chain in dropped {
            self.forget(keys, chain);
        }
    }

    fn forget_written(&mut self, keys: &IndexSet<Key>, writes: &Footprint) {
        let Some(fields) = writes.fields() else {
            *self = Self::default();
            return;
        };
        let ended: Vec<usize> = writes
            .locals()
            .filter_map(|local| self.by_root.get(local))
            .chain(fields.filter_map(|field| self.by_field.get(field)))
            .flatten()
            .copied()
            .collect();
        for chain in ended {
            self.forget(keys, chain);
        }
    }

    fn forget(&mut self, keys: &IndexSet<Key>, chain: usize) {
        if self.chains.swap_remove(&chain).is_none() {
            return;
        }
        let key = &keys[chain];
        if let Some(chains) = self.by_root.get_mut(&key.root) {
            chains.swap_remove(&chain);
        }
        for (_, field) in &key.fields {
            if let Some(chains) = self.by_field.get_mut(field) {
                chains.swap_remove(&chain);
            }
        }
    }
}

struct Temp {
    name: String,
    ty: WirType,
    used: bool,
}

struct Reuser<'c> {
    /// What the arm being visited consults (`arm_callees`).
    callees: Option<&'c CalleeWrites>,
    /// Every chain the function reads, which [`Avail`] names by position.
    keys: IndexSet<Key>,
    avail: Avail,
    temps: Vec<Temp>,
    taken: IndexSet<String>,
}

impl Reuser<'_> {
    fn visit_body(&mut self, body: &mut [WirInstr]) {
        for instr in body {
            self.visit(instr);
        }
    }

    /// What is available where `arm` falls through, or `None` where it never
    /// does.
    fn visit_arm(&mut self, arm: &mut [WirInstr], entry: &Avail) -> Option<Avail> {
        self.avail.clone_from(entry);
        let callees = self.callees;
        self.callees = arm_callees(arm, callees);
        self.visit_body(arm);
        self.callees = callees;
        let falls_through = !arm.last().is_some_and(WirInstr::always_diverges);
        falls_through.then(|| std::mem::take(&mut self.avail))
    }

    /// Walk `instr` in evaluation order, reusing what is available and
    /// recording what becomes so.
    fn visit(&mut self, instr: &mut WirInstr) {
        if let Some(key) = Key::of(instr) {
            self.visit_chain(instr, key);
            return;
        }
        match instr {
            WirInstr::Block { body, .. } => {
                let before = self.avail.clone();
                self.visit_body(body);
                self.avail = before;
                self.avail
                    .forget_written(&self.keys, &Footprint::writes_all(body, self.callees));
            }
            WirInstr::Loop { body, .. } => {
                self.avail
                    .forget_written(&self.keys, &Footprint::writes_all(body, self.callees));
                let entry = self.avail.clone();
                self.visit_body(body);
                self.avail = entry;
            }
            WirInstr::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                self.visit(condition);
                let before = self.avail.clone();
                let then_end = self.visit_arm(then_body, &before);
                let else_end = match else_body {
                    Some(else_body) => self.visit_arm(else_body, &before),
                    None => Some(before.clone()),
                };
                self.avail = before;
                self.avail.retain(&self.keys, |chain, id| {
                    [&then_end, &else_end]
                        .into_iter()
                        .flatten()
                        .all(|end| end.get(chain) == Some(id))
                });
            }
            _ => {
                instr.for_each_boxed_child_mut(&mut |child| self.visit(child));
                self.avail
                    .forget_written(&self.keys, &Footprint::writes_of_node(instr, self.callees));
            }
        }
    }

    fn visit_chain(&mut self, instr: &mut WirInstr, key: Key) {
        let (chain, _) = self.keys.insert_full(key);
        if let Some(id) = self.avail.get(chain) {
            let temp = &mut self.temps[id];
            temp.used = true;
            let WirInstr::StructGet { result_ty, .. } = instr else {
                unreachable!("a chain key is read off a struct.get");
            };
            *instr = WirInstr::LocalGet {
                name: temp.name.clone(),
                result_ty: result_ty.clone(),
            };
            return;
        }
        let WirInstr::StructGet {
            expr, result_ty, ..
        } = instr
        else {
            unreachable!("a chain key is read off a struct.get");
        };
        self.visit(expr);
        let id = self.temps.len();
        let name = format!("$load_{id}");
        assert!(
            !self.taken.contains(&name),
            "only this pass mints `$load_` locals, once per function"
        );
        self.temps.push(Temp {
            name: name.clone(),
            ty: result_ty.clone(),
            used: false,
        });
        self.avail.insert(&self.keys, chain, id);
        let get = std::mem::replace(instr, WirInstr::Nop);
        *instr = WirInstr::LocalTee {
            name,
            value: Box::new(get),
        };
    }
}

/// Strip the tee off every candidate no later read reused.
fn unwrap_unused(instr: &mut WirInstr, unused: &IndexSet<String>) {
    instr.for_each_boxed_child_mut(&mut |child| unwrap_unused(child, unused));
    if let WirInstr::LocalTee { name, value } = instr
        && unused.contains(name)
    {
        let get = std::mem::replace(&mut **value, WirInstr::Nop);
        *instr = get;
    }
}

pub(super) fn reuse_struct_loads(module: &mut WirPackage) {
    let callees = CalleeWrites::of_module(module);
    for func in &mut module.functions {
        let locals = func.declared_locals();
        let Some(body) = &mut func.body else {
            continue;
        };
        let mut reuser = Reuser {
            callees: Some(&callees),
            keys: IndexSet::default(),
            avail: Avail::default(),
            temps: Vec::new(),
            taken: locals
                .iter()
                .map(|(name, _)| name)
                .chain(func.param_names.iter().map(String::as_str))
                .map(str::to_string)
                .collect(),
        };
        reuser.visit_body(body);
        if reuser.temps.is_empty() {
            continue;
        }
        let (used, unused): (Vec<Temp>, Vec<Temp>) =
            reuser.temps.into_iter().partition(|temp| temp.used);
        let unused: IndexSet<String> = unused.into_iter().map(|temp| temp.name).collect();
        for instr in body.iter_mut() {
            unwrap_unused(instr, &unused);
        }
        let decls = used
            .into_iter()
            .map(|Temp { name, ty, .. }| WirInstr::DeclareLocal { name, ty });
        body.splice(0..0, decls);
    }
}
