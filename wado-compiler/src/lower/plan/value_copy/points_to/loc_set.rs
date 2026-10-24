//! A set of locations. Most sets hold a few locations, but one a
//! context-insensitive parameter gathers from every caller holds thousands,
//! and the solver unions such sets along every copy: words of 64 locations make
//! that a merge of words rather than a hash insert per location.

/// The size past which a set holds words.
const SMALL_MAX: usize = 32;

/// A location by its dense index, which is not the id of its node: the solver
/// numbers locations apart so that a set packs into few words.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Loc(pub u32);

#[derive(Clone, Debug)]
pub struct LocSet {
    repr: Repr,
}

#[derive(Clone, Debug)]
enum Repr {
    /// Ascending.
    Small(Vec<u32>),
    /// The non-empty words, by ascending index: word `i` holds locations
    /// `64 * i ..< 64 * (i + 1)`.
    Words { words: Vec<(u32, u64)>, len: usize },
}

impl Default for LocSet {
    fn default() -> Self {
        Self {
            repr: Repr::Small(Vec::new()),
        }
    }
}

impl LocSet {
    pub fn len(&self) -> usize {
        match &self.repr {
            Repr::Small(locs) => locs.len(),
            Repr::Words { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, Loc(loc): Loc) -> bool {
        match &self.repr {
            Repr::Small(locs) => locs.binary_search(&loc).is_ok(),
            Repr::Words { words, .. } => words
                .binary_search_by_key(&(loc / 64), |&(i, _)| i)
                .is_ok_and(|at| words[at].1 & (1 << (loc % 64)) != 0),
        }
    }

    /// Ascending.
    pub fn iter(&self) -> impl Iterator<Item = Loc> + '_ {
        let (small, words) = self.forms();
        small
            .iter()
            .copied()
            .chain(
                words
                    .iter()
                    .flat_map(|&(i, w)| Bits(w).map(move |b| i * 64 + b)),
            )
            .map(Loc)
    }

    /// The locations of a small set and the words of a large one: the other
    /// is empty.
    fn forms(&self) -> (&[u32], &[(u32, u64)]) {
        match &self.repr {
            Repr::Small(locs) => (locs, &[]),
            Repr::Words { words, .. } => (&[], words),
        }
    }

    pub fn insert(&mut self, Loc(loc): Loc) -> bool {
        self.insert_word(loc / 64, 1 << (loc % 64)) != 0
    }

    /// Add each location of `other`, and each one new here to `added`.
    pub fn union_into(&mut self, other: &LocSet, added: &mut LocSet) {
        if let (Repr::Words { words: ours, len }, Repr::Words { words: theirs, .. }) =
            (&mut self.repr, &other.repr)
            && theirs.len() > SMALL_MAX
        {
            *len += merge_words(ours, theirs, added);
            return;
        }
        for (i, w) in other.words() {
            let new = self.insert_word(i, w);
            if new != 0 {
                added.insert_word(i, new);
            }
        }
    }

    /// The non-empty words, by ascending index.
    fn words(&self) -> impl Iterator<Item = (u32, u64)> + '_ {
        let (small, words) = self.forms();
        let mut small = small.iter().copied().peekable();
        std::iter::from_fn(move || {
            let first = small.next()?;
            let (i, mut w) = (first / 64, 1 << (first % 64));
            while let Some(loc) = small.next_if(|&loc| loc / 64 == i) {
                w |= 1 << (loc % 64);
            }
            Some((i, w))
        })
        .chain(words.iter().copied())
    }

    /// Add to `out` each location here that `other` lacks.
    pub fn difference_into(&self, other: &LocSet, out: &mut LocSet) {
        let mut theirs = other.words().peekable();
        for (i, w) in self.words() {
            while theirs.next_if(|&(j, _)| j < i).is_some() {}
            let missing = match theirs.peek() {
                Some(&(j, v)) if j == i => w & !v,
                Some(_) | None => w,
            };
            if missing != 0 {
                out.insert_word(i, missing);
            }
        }
    }

    /// Set the bits `bits` of word `at`, answering those that were clear.
    fn insert_word(&mut self, at: u32, bits: u64) -> u64 {
        assert_ne!(bits, 0, "a set holds no empty word");
        match &mut self.repr {
            Repr::Small(locs) => {
                let mut new = 0;
                for b in Bits(bits) {
                    let loc = at * 64 + b;
                    if let Err(i) = locs.binary_search(&loc) {
                        locs.insert(i, loc);
                        new |= 1 << b;
                    }
                }
                if locs.len() > SMALL_MAX {
                    let len = locs.len();
                    let mut words: Vec<(u32, u64)> = Vec::new();
                    for &loc in locs.iter() {
                        match words.last_mut() {
                            Some((i, w)) if *i == loc / 64 => *w |= 1 << (loc % 64),
                            _ => words.push((loc / 64, 1 << (loc % 64))),
                        }
                    }
                    self.repr = Repr::Words { words, len };
                }
                new
            }
            Repr::Words { words, len } => {
                let new = match words.binary_search_by_key(&at, |&(i, _)| i) {
                    Ok(k) => {
                        let new = bits & !words[k].1;
                        words[k].1 |= new;
                        new
                    }
                    Err(k) => {
                        words.insert(k, (at, bits));
                        bits
                    }
                };
                *len += new.count_ones() as usize;
                new
            }
        }
    }
}

/// Merge `theirs` into `ours`, both ascending, recording each new bit in
/// `added`; answers how many bits were new.
///
/// Merges in place: a set that has settled gains few words, so the words it
/// already holds are updated where they stand and only the new ones move.
fn merge_words(ours: &mut Vec<(u32, u64)>, theirs: &[(u32, u64)], added: &mut LocSet) -> usize {
    let mut gained = 0;
    let mut fresh = 0;
    let mut a = 0;
    for &(i, w) in theirs {
        while a < ours.len() && ours[a].0 < i {
            a += 1;
        }
        let new = match ours.get_mut(a) {
            Some((j, old)) if *j == i => {
                let new = w & !*old;
                *old |= new;
                new
            }
            Some(_) | None => {
                fresh += 1;
                w
            }
        };
        if new != 0 {
            gained += new.count_ones() as usize;
            added.insert_word(i, new);
        }
    }
    if fresh > 0 {
        // Fill from the back: each word lands at or past where it stood, so
        // none is overwritten before it moves, and once the last new word is
        // placed the rest already stand where they belong.
        let mut a = ours.len();
        ours.resize(a + fresh, (0, 0));
        let mut out = ours.len();
        for &(i, w) in theirs.iter().rev() {
            if out == a {
                break;
            }
            while a > 0 && ours[a - 1].0 > i {
                a -= 1;
                out -= 1;
                ours[out] = ours[a];
            }
            out -= 1;
            if a > 0 && ours[a - 1].0 == i {
                a -= 1;
                ours[out] = ours[a];
            } else {
                ours[out] = (i, w);
            }
        }
        assert_eq!(out, a, "the merge fills every slot it opened");
    }
    gained
}

impl FromIterator<Loc> for LocSet {
    fn from_iter<I: IntoIterator<Item = Loc>>(locs: I) -> Self {
        let mut set = Self::default();
        for loc in locs {
            set.insert(loc);
        }
        set
    }
}

impl PartialEq for LocSet {
    fn eq(&self, other: &Self) -> bool {
        // A set holds words exactly when it has grown past `SMALL_MAX`, so two
        // sets of one size share a form, and each form is canonical.
        match (&self.repr, &other.repr) {
            (Repr::Small(a), Repr::Small(b)) => a == b,
            (Repr::Words { words: a, len: m }, Repr::Words { words: b, len: n }) => {
                m == n && a == b
            }
            _ => false,
        }
    }
}

impl Eq for LocSet {}

/// The set bits of a word, ascending.
struct Bits(u64);

impl Iterator for Bits {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        if self.0 == 0 {
            return None;
        }
        let b = self.0.trailing_zeros();
        self.0 &= self.0 - 1;
        Some(b)
    }
}

#[cfg(test)]
mod tests {
    use super::{Loc, LocSet};

    fn set(locs: impl IntoIterator<Item = u32>) -> LocSet {
        locs.into_iter().map(Loc).collect()
    }

    fn indices(set: &LocSet) -> Vec<u32> {
        set.iter().map(|Loc(index)| index).collect()
    }

    #[test]
    fn union_reports_only_new_locations_across_every_form() {
        // Small into small, words into words by insertion, and words into
        // words by merge: into every word already held, and interleaving new
        // words with held ones from either side.
        for (step_a, step_b, end) in [
            (37, 41, 1000),
            (3, 2, 100),
            (3, 2, 100_000),
            (200, 130, 100_000),
            (130, 200, 100_000),
        ] {
            let mut a = set((0..end).step_by(step_a));
            let b = set((0..end).step_by(step_b));
            let mut added = LocSet::default();
            a.union_into(&b, &mut added);
            let is_a = |n: &u32| n.is_multiple_of(step_a as u32);
            let is_b = |n: &u32| n.is_multiple_of(step_b as u32);
            let expected: Vec<u32> = (0..end).filter(|n| is_a(n) || is_b(n)).collect();
            assert_eq!(indices(&a), expected);
            assert_eq!(a.len(), expected.len());
            let new: Vec<u32> = (0..end).filter(|n| is_b(n) && !is_a(n)).collect();
            assert_eq!(indices(&added), new);
            assert_eq!(added.len(), new.len());
        }
    }

    #[test]
    fn difference_adds_what_the_other_lacks_across_every_form() {
        // Small less small, small less words, words less small, words less words.
        for (step_a, step_b, end) in [(3, 2, 60), (50, 3, 1000), (3, 50, 1000), (2, 3, 100_000)] {
            let a = set((0..end).step_by(step_a));
            let b = set((0..end).step_by(step_b));
            let mut out = set([1, end + 1]);
            a.difference_into(&b, &mut out);
            let is_b = |n: &u32| n.is_multiple_of(step_b as u32);
            let mut expected: Vec<u32> = (0..end)
                .step_by(step_a)
                .filter(|n| !is_b(n))
                .chain([1, end + 1])
                .collect();
            expected.sort_unstable();
            expected.dedup();
            assert_eq!(indices(&out), expected);
            assert_eq!(out.len(), expected.len());
        }
    }

    #[test]
    fn membership_and_equality_ignore_the_form() {
        let small = set(0..20);
        let mut words = set(0..40);
        assert!(words.contains(Loc(39)) && !words.contains(Loc(40)));
        assert!(!words.contains(Loc(100_000)));
        assert!(!words.insert(Loc(6)));
        assert_ne!(small, words);
        assert_eq!(set((0..40).rev()), words);
        words.insert(Loc(100_000));
        assert!(words.contains(Loc(100_000)));
    }
}
