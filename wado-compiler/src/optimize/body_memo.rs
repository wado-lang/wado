//! Per-function facts kept across the optimizer's rounds and re-derived only
//! for the functions written since. A [`crate::nir::FuncCell`] counts every
//! mutable borrow taken of it, so no pass can rewrite a body behind a memo's
//! back (WEP: Parallel Optimizer).

use crate::nir::NirFunction;
use crate::nir_package::NirPackage;

/// Facts each function's body answers alone, re-derived only for the functions
/// written since the last refresh. What `of` reads beyond the function must
/// hold still while the memo lives, or the entries that read it be forgotten
/// when it moves.
pub struct BodyMemo<T> {
    /// Each entry's function's write count when the entry was derived.
    writes: Vec<u64>,
    facts: Vec<T>,
    /// Where `assert_one_settled` resumes its rotation.
    #[cfg(debug_assertions)]
    cursor: usize,
}

/// The write count a forgotten entry holds, which no function reaches.
const FORGOTTEN: u64 = u64::MAX;

impl<T> Default for BodyMemo<T> {
    fn default() -> Self {
        Self {
            writes: Vec::new(),
            facts: Vec::new(),
            #[cfg(debug_assertions)]
            cursor: 0,
        }
    }
}

impl<T: PartialEq + std::fmt::Debug> BodyMemo<T> {
    /// Forget the entries `stale` picks, for a caller whose facts read
    /// something beyond the body that has since moved.
    pub fn forget_where(&mut self, stale: impl Fn(&T) -> bool) {
        for (writes, fact) in self.writes.iter_mut().zip(&self.facts) {
            if stale(fact) {
                *writes = FORGOTTEN;
            }
        }
    }

    /// The facts as the last refresh left them.
    pub fn facts(&self) -> &[T] {
        &self.facts
    }

    /// Every function's facts as `project` stands, indexed by store position.
    pub fn refresh(&mut self, project: &NirPackage, of: impl FnMut(&NirFunction) -> T) -> &[T] {
        self.refresh_observing(project, of, |_, _, _| {})
    }

    /// [`Self::refresh`], handing `rederived` each entry it re-derives with
    /// the entry it replaces, `None` for a function seen for the first time.
    pub fn refresh_observing(
        &mut self,
        project: &NirPackage,
        mut of: impl FnMut(&NirFunction) -> T,
        mut rederived: impl FnMut(usize, Option<&T>, &T),
    ) -> &[T] {
        if self.facts.len() > project.functions.len() {
            *self = Self::default();
        }
        for (i, f) in project.functions.iter().enumerate() {
            let writes = f.writes();
            if self.writes.get(i) == Some(&writes) {
                continue;
            }
            let fact = of(&f.borrow());
            rederived(i, self.facts.get(i), &fact);
            if i < self.facts.len() {
                self.writes[i] = writes;
                self.facts[i] = fact;
            } else {
                self.writes.push(writes);
                self.facts.push(fact);
            }
        }
        #[cfg(debug_assertions)]
        self.assert_one_settled(project, of);
        &self.facts
    }

    /// An entry whose facts read something beyond the body that moved, and
    /// that nobody forgot, no longer answers for its function. One function per
    /// refresh, rotating, keeps the check cheap.
    #[cfg(debug_assertions)]
    fn assert_one_settled(&mut self, project: &NirPackage, mut of: impl FnMut(&NirFunction) -> T) {
        let Some(n) = self.cursor.checked_rem(self.facts.len()) else {
            return;
        };
        self.cursor = n + 1;
        let fresh = of(&project.functions[n].borrow());
        assert!(
            fresh == self.facts[n],
            "memoized facts of function {n} went stale without a write to it\n  \
             memo:  {:?}\n  fresh: {fresh:?}",
            self.facts[n],
        );
    }
}
