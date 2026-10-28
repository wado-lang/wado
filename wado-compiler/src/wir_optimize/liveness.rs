//! Backward liveness over a WIR body, following Wasm's branches. An
//! [`Observer`] sees each store with what is live right after it, which
//! answers both which stores no read can see ([`dead_stores`]) and which
//! locals hold values at once (`local_coalesce`).
//!
//! A forward walk flattens the tree into events, numbering each node in
//! pre-order as `WirMutVisitor` reaches it, so a rewrite finds a store by that
//! number. The backward pass then runs over the events: a branch takes the
//! liveness of its target, the end of a block or an `if`, or the head of a
//! loop, which iterates to its fixed point.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::WirInstr;
use crate::wir_visitor::WirRefVisitor;

use super::util::Bits;

/// The pre-order numbers of the stores in `body` no read can see, because
/// every path from the store overwrites the local or leaves the function first.
pub(super) fn dead_stores(body: &[WirInstr]) -> IndexSet<u32> {
    let mut dead = DeadStores::default();
    Flow::of(body).run(&mut dead);
    dead.0
}

#[derive(Default)]
struct DeadStores(IndexSet<u32>);

impl Observer for DeadStores {
    fn store(&mut self, store: &Store, live_after: &Bits) {
        if let Store::One { local, id, .. } = store
            && !live_after.contains(*local)
        {
            self.0.insert(*id);
        }
    }

    fn mark(&self) -> usize {
        self.0.len()
    }

    fn rollback(&mut self, mark: usize) {
        self.0.truncate(mark);
    }
}

/// What sees the stores of a [`Flow`].
pub(super) trait Observer {
    fn store(&mut self, store: &Store, live_after: &Bits);

    /// Where to roll back to, should what follows be observed under liveness
    /// short of final: a loop pass whose head then moves.
    fn mark(&self) -> usize;

    fn rollback(&mut self, mark: usize);
}

/// A write of locals, by the index [`Flow::locals`] gives each.
pub(super) enum Store {
    /// A `LocalSet` or `LocalTee` by the node of pre-order number `id`;
    /// `copy_of` is the local its value merely reads.
    One {
        local: usize,
        id: u32,
        copy_of: Option<usize>,
    },
    /// A multi-value bind, writing its targets at once.
    Bind(Vec<usize>),
}

/// A body flattened for liveness.
pub(super) struct Flow {
    events: Vec<Event>,
    /// The matching start of each end.
    start_of: Vec<usize>,
    /// The `else` of each `if` start that has one.
    else_of: Vec<Option<usize>>,
    /// Every local the body accesses, in the order it first does.
    pub(super) locals: IndexSet<String>,
}

impl Flow {
    pub(super) fn of(body: &[WirInstr]) -> Self {
        let mut flat = Flatten::default();
        flat.walk_body(body);
        let events = flat.events;
        let mut start_of = vec![0; events.len()];
        let mut else_of = vec![None; events.len()];
        let mut open: Vec<usize> = Vec::new();
        for (at, event) in events.iter().enumerate() {
            match event {
                Event::BlockStart | Event::LoopStart | Event::IfStart => open.push(at),
                Event::Else => {
                    else_of[*open.last().expect("an `else` is inside its `if`")] = Some(at);
                }
                Event::BlockEnd | Event::LoopEnd | Event::IfEnd => {
                    start_of[at] = open.pop().expect("an end closes a start");
                }
                Event::Read(_)
                | Event::Store(_)
                | Event::Br(_)
                | Event::BrIf(_)
                | Event::BrTable(_)
                | Event::Exit => {}
            }
        }
        Self {
            events,
            start_of,
            else_of,
            locals: flat.locals,
        }
    }

    /// Show `observer` every store, and answer what is live at the entry.
    pub(super) fn run(&self, observer: &mut impl Observer) -> Bits {
        let width = self.locals.len();
        let mut backward = Backward {
            flow: self,
            width,
            observer,
            labels: Vec::new(),
            loop_heads: IndexMap::default(),
        };
        backward.region(0, self.events.len(), Bits::new(width))
    }
}

enum Event {
    Read(usize),
    Store(Store),
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
                self.bracketed(Event::BlockStart, body, Event::BlockEnd);
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
            WirInstr::LocalSet { name, value } | WirInstr::LocalTee { name, value } => {
                self.walk_instr(instr);
                let copy_of = value.local_read().map(|source| self.local(source));
                let local = self.local(name);
                self.events
                    .push(Event::Store(Store::One { local, id, copy_of }));
            }
            _ => {
                self.walk_instr(instr);
                if let Some(name) = instr.local_read() {
                    let local = self.local(name);
                    self.events.push(Event::Read(local));
                }
                let bound: Vec<usize> = instr.local_writes().map(|name| self.local(name)).collect();
                if !bound.is_empty() {
                    self.events.push(Event::Store(Store::Bind(bound)));
                }
            }
        }
    }
}

struct Backward<'a, O> {
    flow: &'a Flow,
    width: usize,
    observer: &'a mut O,
    /// What is live where a branch to each enclosing label lands, innermost
    /// last: after a block or an `if`, at the head of a loop.
    labels: Vec<Bits>,
    /// Each loop's head, by its start event, kept across the passes of an
    /// enclosing loop. Liveness only grows from pass to pass, so the head
    /// found last is a sound start for the next fixed point.
    loop_heads: IndexMap<usize, Bits>,
}

impl<O: Observer> Backward<'_, O> {
    /// What is live at `start`, given `live` at `end`.
    fn region(&mut self, start: usize, end: usize, mut live: Bits) -> Bits {
        let flow = self.flow;
        let mut at = end;
        while at > start {
            at -= 1;
            match &flow.events[at] {
                Event::Read(local) => live.insert(*local),
                Event::Store(store) => {
                    self.observer.store(store, &live);
                    match store {
                        Store::One { local, .. } => live.remove(*local),
                        Store::Bind(locals) => locals.iter().for_each(|&local| live.remove(local)),
                    }
                }
                Event::BlockEnd => {
                    let open = flow.start_of[at];
                    live = self.labeled(live.clone(), open + 1, at, live);
                    at = open;
                }
                Event::IfEnd => {
                    let open = flow.start_of[at];
                    let after = live;
                    let split = flow.else_of[open];
                    // An `if` without an `else` falls through on false.
                    let else_live = match split {
                        Some(split) => self.labeled(after.clone(), split + 1, at, after.clone()),
                        None => after.clone(),
                    };
                    live = self.labeled(after.clone(), open + 1, split.unwrap_or(at), after);
                    live.union(&else_live);
                    at = open;
                }
                Event::LoopEnd => {
                    let open = flow.start_of[at];
                    live = self.looped(open, at, live);
                    at = open;
                }
                Event::Br(depth) => {
                    live = match self.target(*depth) {
                        Some(target) => target.clone(),
                        None => Bits::new(self.width),
                    }
                }
                Event::BrIf(depth) => {
                    if let Some(target) = self.target(*depth) {
                        live.union(target);
                    }
                }
                Event::BrTable(depths) => {
                    live = Bits::new(self.width);
                    for target in depths.iter().filter_map(|&depth| self.target(depth)) {
                        live.union(target);
                    }
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
    fn labeled(&mut self, landing: Bits, start: usize, end: usize, live: Bits) -> Bits {
        self.labels.push(landing);
        let live = self.region(start, end, live);
        self.labels.pop();
        live
    }

    /// The loop's head at its fixed point, from the liveness after it. A pass
    /// whose head moves observed under a head short of final, so the observer
    /// rolls it back.
    fn looped(&mut self, open: usize, close: usize, after: Bits) -> Bits {
        let mut head = self
            .loop_heads
            .get(&open)
            .cloned()
            .unwrap_or_else(|| Bits::new(self.width));
        loop {
            let mark = self.observer.mark();
            let mut next = self.labeled(head.clone(), open + 1, close, after.clone());
            next.union(&head);
            if next == head {
                break;
            }
            self.observer.rollback(mark);
            head = next;
        }
        self.loop_heads.insert(open, head.clone());
        head
    }

    /// What is live where a branch of `depth` lands, `None` past the function
    /// body: a branch out of it returns, and nothing is live.
    fn target(&self, depth: u32) -> Option<&Bits> {
        let at = self.labels.len().checked_sub(depth as usize + 1)?;
        Some(&self.labels[at])
    }
}
