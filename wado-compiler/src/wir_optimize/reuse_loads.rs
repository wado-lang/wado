//! Redundant load elimination: a `struct.get` chain off a local, read again
//! where nothing between could have changed it, reuses the first read through a
//! temp. wasmtime null-checks and reloads every `struct.get` it is given.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirInstr, WirPackage, WirType, WirTypeId};

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

/// What a subtree may write that a chain reads.
#[derive(Default)]
struct Kills {
    everything: bool,
    locals: IndexSet<String>,
    fields: IndexSet<String>,
}

impl Kills {
    fn of_body(body: &[WirInstr]) -> Self {
        let mut kills = Self::default();
        for instr in body {
            kills.add_tree(instr);
        }
        kills
    }

    fn add_tree(&mut self, instr: &WirInstr) {
        self.add_node(instr);
        instr.for_each_child(&mut |child| self.add_tree(child));
    }

    fn add_node(&mut self, instr: &WirInstr) {
        match instr {
            WirInstr::LocalSet { name, .. } | WirInstr::LocalTee { name, .. } => {
                self.locals.insert(name.clone());
            }
            WirInstr::MultiValueLocalBind { locals, .. } => {
                self.locals.extend(locals.iter().flatten().cloned());
            }
            WirInstr::StructSet { field_name, .. } => {
                self.fields.insert(field_name.clone());
            }
            WirInstr::Call { .. }
            | WirInstr::CallIndirect { .. }
            | WirInstr::CallRef { .. }
            | WirInstr::ArrayClone { .. } => self.everything = true,
            _ => {}
        }
    }

    fn apply(&self, avail: &mut Avail) {
        if self.everything {
            avail.clear();
            return;
        }
        avail.retain(|key, _| {
            !self.locals.contains(&key.root)
                && !key.fields.iter().any(|(_, f)| self.fields.contains(f))
        });
    }
}

/// Chains whose value a temp can supply here, each with its defining temp.
type Avail = IndexMap<Key, usize>;

struct Reuser {
    avail: Avail,
    temps: Vec<(String, WirType)>,
    used: Vec<bool>,
    taken: IndexSet<String>,
}

impl Reuser {
    fn visit_body(&mut self, body: &mut [WirInstr]) {
        for instr in body {
            self.visit(instr);
        }
    }

    /// What is available where `arm` falls through, or `None` where it never
    /// does.
    fn visit_arm(&mut self, arm: &mut [WirInstr], entry: &Avail) -> Option<Avail> {
        self.avail.clone_from(entry);
        self.visit_body(arm);
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
                Kills::of_body(body).apply(&mut self.avail);
            }
            WirInstr::Loop { body, .. } => {
                Kills::of_body(body).apply(&mut self.avail);
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
                self.avail.retain(|key, id| {
                    [&then_end, &else_end]
                        .into_iter()
                        .flatten()
                        .all(|end| end.get(key) == Some(id))
                });
            }
            // Wasm pops a `select`'s condition last.
            WirInstr::Select {
                condition,
                if_true,
                if_false,
                ..
            } => {
                self.visit(if_true);
                self.visit(if_false);
                self.visit(condition);
            }
            _ => {
                instr.for_each_boxed_child_mut(&mut |child| self.visit(child));
                let mut kills = Kills::default();
                kills.add_node(instr);
                kills.apply(&mut self.avail);
            }
        }
    }

    fn visit_chain(&mut self, instr: &mut WirInstr, key: Key) {
        if let Some(&id) = self.avail.get(&key) {
            self.used[id] = true;
            let WirInstr::StructGet { result_ty, .. } = instr else {
                unreachable!("a chain key is read off a struct.get");
            };
            *instr = WirInstr::LocalGet {
                name: self.temps[id].0.clone(),
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
        let mut name = format!("$load_{id}");
        while self.taken.contains(&name) {
            name.push('_');
        }
        self.temps.push((name.clone(), result_ty.clone()));
        self.used.push(false);
        self.avail.insert(key, id);
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
    for func in &mut module.functions {
        let locals = func.declared_locals();
        let Some(body) = &mut func.body else {
            continue;
        };
        let mut reuser = Reuser {
            avail: Avail::default(),
            temps: Vec::new(),
            used: Vec::new(),
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
        let unused: IndexSet<String> = reuser
            .temps
            .iter()
            .zip(&reuser.used)
            .filter(|(_, used)| !**used)
            .map(|((name, _), _)| name.clone())
            .collect();
        for instr in body.iter_mut() {
            unwrap_unused(instr, &unused);
        }
        let decls = reuser
            .temps
            .into_iter()
            .zip(reuser.used)
            .filter(|(_, used)| *used)
            .map(|((name, ty), _)| WirInstr::DeclareLocal { name, ty });
        body.splice(0..0, decls);
    }
}
