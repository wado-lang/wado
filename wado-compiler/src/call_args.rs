//! The argument list of a direct call, shared by TIR and NIR.

use std::ops::{Deref, Index, IndexMut};
use std::slice::SliceIndex;

/// A call's arguments in the callee's parameter order, knowing whether the
/// first is an instance method's receiver.
///
/// A receiver is one written with dot syntax: a trait-qualified (UFCS) call
/// carries its receiver first too, but as a plain argument, since it spells the
/// receiver's mode itself (`Trait::m(&mut x, …)`).
///
/// A method call without its receiver cannot be built, so no reader has to
/// decide what one would mean. Arguments can be replaced but not reordered, and
/// only [`Self::retain_positions`] changes the length, keeping the receiver
/// flag in step with it.
#[derive(Debug, Clone)]
pub struct CallArgs<A> {
    list: Vec<A>,
    has_receiver: bool,
}

impl<A> CallArgs<A> {
    /// The arguments of a call with no receiver.
    #[must_use]
    pub fn free(list: Vec<A>) -> Self {
        Self {
            list,
            has_receiver: false,
        }
    }

    /// The arguments of `receiver.m(rest)`.
    pub fn method(receiver: A, rest: impl IntoIterator<Item = A>) -> Self {
        Self {
            list: std::iter::once(receiver).chain(rest).collect(),
            has_receiver: true,
        }
    }

    /// The same call shape over `list`, which replaces these arguments one for
    /// one.
    #[must_use]
    pub fn rebuild<B>(&self, list: Vec<B>) -> CallArgs<B> {
        assert_eq!(
            list.len(),
            self.list.len(),
            "a rebuilt argument list must replace every argument"
        );
        CallArgs {
            list,
            has_receiver: self.has_receiver,
        }
    }

    /// The same call shape with every argument mapped through `f`.
    #[must_use]
    pub fn map<B>(self, f: impl FnMut(A) -> B) -> CallArgs<B> {
        CallArgs {
            list: self.list.into_iter().map(f).collect(),
            has_receiver: self.has_receiver,
        }
    }

    /// The arguments alone, for a call to a callee with a different signature,
    /// which decides for itself whether the first is a receiver.
    #[must_use]
    pub fn into_vec(self) -> Vec<A> {
        self.list
    }

    /// The receiver, or `None` for a call without one.
    #[must_use]
    pub fn receiver(&self) -> Option<&A> {
        self.split().0
    }

    /// The receiver, if any, and the arguments after it.
    #[must_use]
    pub fn split(&self) -> (Option<&A>, &[A]) {
        if !self.has_receiver {
            return (None, &self.list);
        }
        let (receiver, rest) = self
            .list
            .split_first()
            .expect("a method call has its receiver");
        (Some(receiver), rest)
    }

    /// Every argument, mutably, in place.
    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, A> {
        self.list.iter_mut()
    }

    /// [`Self::split`], mutably.
    pub fn split_mut(&mut self) -> (Option<&mut A>, &mut [A]) {
        if !self.has_receiver {
            return (None, &mut self.list);
        }
        let (receiver, rest) = self
            .list
            .split_first_mut()
            .expect("a method call has its receiver");
        (Some(receiver), rest)
    }

    /// The arguments after the receiver, each with its position in the callee's
    /// parameter list.
    pub fn rest_positioned(&self) -> impl Iterator<Item = (usize, &A)> {
        let offset = usize::from(self.has_receiver);
        self.split()
            .1
            .iter()
            .enumerate()
            .map(move |(i, a)| (offset + i, a))
    }

    /// Keep the arguments whose position `keep` accepts. Dropping the receiver
    /// leaves a call without one.
    pub fn retain_positions(&mut self, mut keep: impl FnMut(usize) -> bool) {
        let mut position = 0;
        let mut receiver_kept = true;
        self.list.retain(|_| {
            let kept = keep(position);
            if position == 0 {
                receiver_kept = kept;
            }
            position += 1;
            kept
        });
        self.has_receiver &= receiver_kept;
    }

    /// Make the receiver an ordinary first argument, for a callee whose first
    /// parameter no longer takes `self`.
    pub fn demote_receiver(&mut self) {
        self.has_receiver = false;
    }
}

/// No arguments, as a call without a receiver has.
impl<A> Default for CallArgs<A> {
    fn default() -> Self {
        Self::free(Vec::new())
    }
}

impl<A> Deref for CallArgs<A> {
    type Target = [A];

    fn deref(&self) -> &[A] {
        &self.list
    }
}

impl<A, I: SliceIndex<[A]>> Index<I> for CallArgs<A> {
    type Output = I::Output;

    fn index(&self, index: I) -> &I::Output {
        &self.list[index]
    }
}

impl<A> IndexMut<usize> for CallArgs<A> {
    fn index_mut(&mut self, position: usize) -> &mut A {
        &mut self.list[position]
    }
}

impl<'a, A> IntoIterator for &'a CallArgs<A> {
    type Item = &'a A;
    type IntoIter = std::slice::Iter<'a, A>;

    fn into_iter(self) -> Self::IntoIter {
        self.list.iter()
    }
}

impl<'a, A> IntoIterator for &'a mut CallArgs<A> {
    type Item = &'a mut A;
    type IntoIter = std::slice::IterMut<'a, A>;

    fn into_iter(self) -> Self::IntoIter {
        self.list.iter_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_call_has_no_receiver() {
        let args = CallArgs::free(vec![1, 2]);
        assert_eq!(args.receiver(), None);
        assert_eq!(args.split(), (None, &[1, 2][..]));
        assert_eq!(
            args.rest_positioned().collect::<Vec<_>>(),
            [(0, &1), (1, &2)]
        );
    }

    #[test]
    fn method_call_splits_at_receiver() {
        let args = CallArgs::method(0, [1, 2]);
        assert_eq!(args.receiver(), Some(&0));
        assert_eq!(args.split(), (Some(&0), &[1, 2][..]));
        assert_eq!(
            args.rest_positioned().collect::<Vec<_>>(),
            [(1, &1), (2, &2)]
        );
        assert_eq!(&*args, &[0, 1, 2]);
    }

    #[test]
    fn dropping_the_receiver_leaves_a_free_call() {
        let mut args = CallArgs::method(0, [1, 2]);
        args.retain_positions(|p| p != 0);
        assert_eq!(args.split(), (None, &[1, 2][..]));
    }

    #[test]
    fn dropping_another_argument_keeps_the_receiver() {
        let mut args = CallArgs::method(0, [1, 2]);
        args.retain_positions(|p| p != 1);
        assert_eq!(args.split(), (Some(&0), &[2][..]));
    }

    #[test]
    fn retain_asks_once_per_position() {
        let mut args = CallArgs::method(0, [1, 2]);
        let mut verdicts = [false, true, true].into_iter();
        args.retain_positions(|_| verdicts.next().expect("one verdict per position"));
        assert_eq!(args.split(), (None, &[1, 2][..]));
    }

    #[test]
    fn demoted_receiver_is_a_first_argument() {
        let mut args = CallArgs::method(0, [1]);
        args.demote_receiver();
        assert_eq!(args.split(), (None, &[0, 1][..]));
    }

    #[test]
    fn map_and_rebuild_keep_the_shape() {
        let args = CallArgs::method(0, [1]);
        assert_eq!(args.rebuild(vec!["a", "b"]).receiver(), Some(&"a"));
        assert_eq!(args.map(|a| a * 10).split(), (Some(&0), &[10][..]));
    }

    #[test]
    #[should_panic(expected = "replace every argument")]
    fn rebuild_rejects_a_different_length() {
        let _ = CallArgs::method(0, [1]).rebuild(vec![0]);
    }
}
