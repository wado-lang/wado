//! The answers of a recursive walk over types that cuts a cycle where it
//! reaches a node it is still inside, answering the cut with a fixed answer:
//! `false` for "holds one at any depth", `true` for "has no value". Types share
//! members, so without these answers a walk visits a member once per path to
//! it: exponentially many in a chain of types that each hold the next twice,
//! or in a parser's mutually recursive contexts.
//!
//! The answer opposite to the cut's holds wherever the walk reaches the node
//! again, and so does one whose walk cut nowhere outside itself. One that cut
//! at a node opened before it rests on what that node is still deciding. It
//! holds while each such node is still open, since more open nodes only cut
//! more, and a cut only ever gives the cut's answer. It also holds once each
//! has closed with the cut's answer, which settles what the cut assumed.

use std::hash::Hash;

use crate::hashmap::{IndexMap, IndexSet};

pub(crate) struct CutMemo<K> {
    cut_answer: bool,
    answers: IndexMap<K, Answer>,
    /// A serial for each open node, outermost first.
    open: Vec<u64>,
    next_serial: u64,
    /// The open nodes cuts have stopped at since the current node's walk
    /// began, as index and serial, by ascending index.
    cuts: Vec<(usize, u64)>,
    /// The nodes that closed with the cut's answer, each with the cuts that
    /// answer rests on.
    closed_as_cut: IndexMap<u64, Vec<(usize, u64)>>,
}

struct Answer {
    answer: bool,
    rests_on: Vec<(usize, u64)>,
}

impl<K: Eq + Hash> CutMemo<K> {
    pub(crate) fn new(cut_answer: bool) -> Self {
        Self {
            cut_answer,
            answers: IndexMap::default(),
            open: Vec::new(),
            next_serial: 0,
            cuts: Vec::new(),
            closed_as_cut: IndexMap::default(),
        }
    }

    pub(crate) fn get(&self, key: &K) -> Option<bool> {
        let answer = self.answers.get(key)?;
        self.still_holds(&answer.rests_on).then_some(answer.answer)
    }

    fn still_holds(&self, rests_on: &[(usize, u64)]) -> bool {
        rests_on.iter().all(|&(at, serial)| {
            self.open.get(at) == Some(&serial)
                || self
                    .closed_as_cut
                    .get(&serial)
                    .is_some_and(|rests| self.still_holds(rests))
        })
    }

    /// The number of open nodes.
    pub(crate) fn depth(&self) -> usize {
        self.open.len()
    }

    pub(crate) fn open(&mut self) {
        self.open.push(self.next_serial);
        self.next_serial += 1;
    }

    pub(crate) fn close(&mut self, answer: bool) {
        let serial = self.open.pop().expect("a close must follow its open");
        if answer == self.cut_answer {
            let at = self.open.len();
            let rests = self.cuts.iter().copied().filter(|&(i, _)| i < at).collect();
            self.closed_as_cut.insert(serial, rests);
        }
    }

    /// Starts a node's walk, returning what [`Self::end`] restores.
    pub(crate) fn begin(&mut self) -> Vec<(usize, u64)> {
        std::mem::take(&mut self.cuts)
    }

    /// Ends the walk of `key`, begun with `depth` nodes open, and says whether
    /// its answer rests on nothing still open. A cut at a node the walk opened
    /// itself is closed now.
    pub(crate) fn end(
        &mut self,
        key: K,
        depth: usize,
        answer: bool,
        outer_cuts: Vec<(usize, u64)>,
    ) -> bool {
        self.cuts.retain(|&(at, _)| at < depth);
        let rests_on = if answer == self.cut_answer {
            self.cuts.clone()
        } else {
            Vec::new()
        };
        let settled = rests_on.is_empty();
        self.answers.insert(key, Answer { answer, rests_on });
        self.restore(outer_cuts);
        settled
    }

    /// Ends a walk whose answer rests on something other than the cuts, such
    /// as a depth limit, so it is kept nowhere.
    fn end_unkept(&mut self, depth: usize, outer_cuts: Vec<(usize, u64)>) {
        self.cuts.retain(|&(at, _)| at < depth);
        self.restore(outer_cuts);
    }

    fn restore(&mut self, outer_cuts: Vec<(usize, u64)>) {
        for (at, _) in outer_cuts {
            self.cut(at);
        }
    }

    /// A cut at the open node at index `at`, answered with the cut's answer.
    pub(crate) fn cut(&mut self, at: usize) -> bool {
        if let Err(slot) = self.cuts.binary_search_by_key(&at, |&(i, _)| i) {
            self.cuts.insert(slot, (at, self.open[at]));
        }
        self.cut_answer
    }
}

/// A walk whose nodes are what it cuts at: a node met again inside itself is
/// cut.
pub(crate) struct CutWalk<K> {
    /// The nodes the walk is inside, outermost first.
    open: IndexSet<K>,
    memo: CutMemo<K>,
    /// How many answers have rested on something other than the cuts.
    unkept: usize,
}

/// A node's answer, and whether it rests on nothing still open, so it holds
/// outside this walk too.
pub(crate) struct Visited {
    pub(crate) answer: bool,
    pub(crate) settled: bool,
}

impl<K: Clone + Eq + Hash> CutWalk<K> {
    pub(crate) fn new(cut_answer: bool) -> Self {
        Self {
            open: IndexSet::default(),
            memo: CutMemo::new(cut_answer),
            unkept: 0,
        }
    }

    /// The answer of `key`, by `walk` where the walk has none yet.
    pub(crate) fn visit(&mut self, key: K, walk: impl FnOnce(&mut Self) -> bool) -> Visited {
        let unsettled = |answer| Visited {
            answer,
            settled: false,
        };
        if let Some(answer) = self.memo.get(&key) {
            return unsettled(answer);
        }
        if let Some(at) = self.open.get_index_of(&key) {
            return unsettled(self.memo.cut(at));
        }
        let depth = self.memo.depth();
        let outer_cuts = self.memo.begin();
        let unkept = self.unkept;
        self.open.insert(key.clone());
        self.memo.open();
        let answer = walk(self);
        self.memo.close(answer);
        self.open.pop();
        if self.unkept == unkept {
            let settled = self.memo.end(key, depth, answer, outer_cuts);
            Visited { answer, settled }
        } else {
            self.memo.end_unkept(depth, outer_cuts);
            unsettled(answer)
        }
    }

    /// Marks the answer being walked as resting on something other than the
    /// cuts, such as a depth limit: neither it nor any answer it reaches is
    /// kept.
    pub(crate) fn unkept(&mut self) {
        self.unkept += 1;
    }
}
