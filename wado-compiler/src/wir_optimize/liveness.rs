//! Backward liveness over a WIR body, following Wasm's branches: which
//! `LocalSet` / `LocalTee` stores no read can see, because every path from the
//! store overwrites the local or leaves the function first.
//!
//! A forward walk flattens the tree into events, numbering each node in
//! pre-order as `WirMutVisitor` reaches it, so the rewrite finds a dead store
//! by that number. The backward pass then runs over the events: a branch takes
//! the liveness of its target, the end of a block or an `if`, or the head of a
//! loop, which iterates to its fixed point.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::WirInstr;
use crate::wir_visitor::WirRefVisitor;

use super::util::Bits;

/// The pre-order number of each dead store in `body`, mapped to the local it
/// writes.
pub(super) fn dead_stores(body: &[WirInstr]) -> IndexMap<u32, String> {
    let mut flat = Flatten::default();
    flat.walk_body(body);
    let events = flat.events;
    let locals = flat.locals;
    let mut start_of = vec![0; events.len()];
    let mut else_of = IndexMap::default();
    let mut open: Vec<usize> = Vec::new();
    for (at, event) in events.iter().enumerate() {
        match event {
            Event::BlockStart | Event::LoopStart | Event::IfStart => open.push(at),
            Event::Else => {
                else_of.insert(*open.last().expect("an `else` is inside its `if`"), at);
            }
            Event::BlockEnd | Event::LoopEnd | Event::IfEnd => {
                start_of[at] = open.pop().expect("an end closes a start");
            }
            _ => {}
        }
    }
    let mut backward = Backward {
        events: &events,
        start_of: &start_of,
        else_of: &else_of,
        width: locals.len(),
        labels: Vec::new(),
        loop_heads: IndexMap::default(),
        dead: IndexMap::default(),
    };
    backward.region(0, events.len(), Bits::new(locals.len()), true);
    backward
        .dead
        .into_iter()
        .map(|(id, local)| (id, locals[local].clone()))
        .collect()
}

enum Event {
    Read(usize),
    /// A store of the local by the node of that pre-order number.
    Write(usize, u32),
    /// A write by a multi-value bind, which is never removed.
    Kill(usize),
    BlockStart,
    BlockEnd,
    LoopStart,
    LoopEnd,
    /// After the condition.
    IfStart,
    Else,
    IfEnd,
    Br(u32),
    /// After the condition.
    BrIf(u32),
    /// After the index: the targets, the default among them.
    BrTable(Vec<u32>),
    /// A `return` after its value, or an `unreachable`.
    Exit,
}

#[derive(Default)]
struct Flatten {
    events: Vec<Event>,
    locals: IndexSet<String>,
    preorder: u32,
}

impl Flatten {
    fn local(&mut self, name: &str) -> usize {
        match self.locals.get_index_of(name) {
            Some(i) => i,
            None => self.locals.insert_full(name.to_string()).0,
        }
    }

    fn bracketed(&mut self, start: Event, body: &[WirInstr], end: Event) {
        self.events.push(start);
        self.walk_body(body);
        self.events.push(end);
    }
}

impl WirRefVisitor for Flatten {
    fn visit_instr(&mut self, instr: &WirInstr) {
        let id = self.preorder;
        self.preorder += 1;
        match instr {
            WirInstr::Block { body, .. } => {
                self.bracketed(Event::BlockStart, body, Event::BlockEnd)
            }
            WirInstr::Loop { body, .. } => self.bracketed(Event::LoopStart, body, Event::LoopEnd),
            WirInstr::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                self.visit_instr(condition);
                self.events.push(Event::IfStart);
                self.walk_body(then_body);
                if let Some(else_body) = else_body {
                    self.events.push(Event::Else);
                    self.walk_body(else_body);
                }
                self.events.push(Event::IfEnd);
            }
            WirInstr::Br { depth } => self.events.push(Event::Br(*depth)),
            WirInstr::BrIf { depth, condition } => {
                self.visit_instr(condition);
                self.events.push(Event::BrIf(*depth));
            }
            WirInstr::BrTable {
                index,
                targets,
                default,
            } => {
                self.visit_instr(index);
                let all = targets.iter().chain([default]).copied().collect();
                self.events.push(Event::BrTable(all));
            }
            WirInstr::Return { .. } | WirInstr::Unreachable => {
                self.walk_instr(instr);
                self.events.push(Event::Exit);
            }
            WirInstr::LocalSet { name, .. } | WirInstr::LocalTee { name, .. } => {
                self.walk_instr(instr);
                let local = self.local(name);
                self.events.push(Event::Write(local, id));
            }
            _ => {
                self.walk_instr(instr);
                if let Some(name) = instr.local_read() {
                    let local = self.local(name);
                    self.events.push(Event::Read(local));
                }
                for name in instr.local_writes() {
                    let local = self.local(name);
                    self.events.push(Event::Kill(local));
                }
            }
        }
    }
}

struct Backward<'a> {
    events: &'a [Event],
    /// The matching start of each end.
    start_of: &'a [usize],
    /// The `else` of each `if` start that has one.
    else_of: &'a IndexMap<usize, usize>,
    width: usize,
    /// What is live where a branch to each enclosing label lands, innermost
    /// last: after a block or an `if`, at the head of a loop.
    labels: Vec<Bits>,
    /// Each loop's head, by its start event, kept across the passes of an
    /// enclosing loop. Liveness only grows from pass to pass, so the head
    /// found last is a sound start for the next fixed point.
    loop_heads: IndexMap<usize, Bits>,
    dead: IndexMap<u32, usize>,
}

impl Backward<'_> {
    /// What is live at `start`, given `live` at `end`. Records the dead stores
    /// where `record` says the labels hold their final liveness.
    fn region(&mut self, start: usize, end: usize, mut live: Bits, record: bool) -> Bits {
        let mut at = end;
        while at > start {
            at -= 1;
            match &self.events[at] {
                Event::Read(local) => live.insert(*local),
                Event::Write(local, id) => {
                    if record && !live.contains(*local) {
                        self.dead.insert(*id, *local);
                    }
                    live.remove(*local);
                }
                Event::Kill(local) => live.remove(*local),
                Event::BlockEnd => {
                    let open = self.start_of[at];
                    live = self.labeled(live.clone(), open + 1, at, live, record);
                    at = open;
                }
                Event::IfEnd => {
                    let open = self.start_of[at];
                    let after = live;
                    let split = self.else_of.get(&open).copied();
                    // An `if` without an `else` falls through on false.
                    let else_live = match split {
                        Some(split) => {
                            self.labeled(after.clone(), split + 1, at, after.clone(), record)
                        }
                        None => after.clone(),
                    };
                    live =
                        self.labeled(after.clone(), open + 1, split.unwrap_or(at), after, record);
                    live.union(&else_live);
                    at = open;
                }
                Event::LoopEnd => {
                    let open = self.start_of[at];
                    live = self.looped(open, at, live, record);
                    at = open;
                }
                Event::Br(depth) => live = self.target(*depth),
                Event::BrIf(depth) => live.union(&self.target(*depth)),
                Event::BrTable(depths) => {
                    let mut union = Bits::new(self.width);
                    for depth in depths {
                        union.union(&self.target(*depth));
                    }
                    live = union;
                }
                Event::Exit => live = Bits::new(self.width),
                Event::BlockStart | Event::LoopStart | Event::IfStart | Event::Else => {
                    unreachable!("a start is skipped from its end")
                }
            }
        }
        live
    }

    /// What is live at `start`, given `live` at `end`, under a label where
    /// what is `landing` is live.
    fn labeled(
        &mut self,
        landing: Bits,
        start: usize,
        end: usize,
        live: Bits,
        record: bool,
    ) -> Bits {
        self.labels.push(landing);
        let live = self.region(start, end, live, record);
        self.labels.pop();
        live
    }

    /// The loop's head at its fixed point, from the liveness after it. A pass
    /// whose head moves records under a head short of final, so its records
    /// go; the loop's stores are recorded by no pass before its first.
    fn looped(&mut self, open: usize, close: usize, after: Bits, record: bool) -> Bits {
        let mut head = self
            .loop_heads
            .get(&open)
            .cloned()
            .unwrap_or_else(|| Bits::new(self.width));
        loop {
            let recorded = self.dead.len();
            let mut next = self.labeled(head.clone(), open + 1, close, after.clone(), record);
            next.union(&head);
            if next == head {
                break;
            }
            self.dead.truncate(recorded);
            head = next;
        }
        self.loop_heads.insert(open, head.clone());
        head
    }

    /// What is live where a branch of `depth` lands: nothing past the
    /// function body, which a branch out of returns.
    fn target(&self, depth: u32) -> Bits {
        match self.labels.len().checked_sub(depth as usize + 1) {
            Some(at) => self.labels[at].clone(),
            None => Bits::new(self.width),
        }
    }
}
