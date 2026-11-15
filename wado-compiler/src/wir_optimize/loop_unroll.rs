//! Run several iterations of a small loop that reads GC arrays per test of its
//! condition. A loop that steps its variable once per iteration,
//!
//! ```text
//! block { loop { br_if 1 EXIT; WORK; v = STEP; REST; br 0 } }
//! ```
//!
//! gains a copy in front of it that leaves once any of the next `n` tests
//! would, `EXIT | EXIT[v := STEP] | …`, and otherwise runs its body `n` times.
//! A counted loop, `v > x` stepping `v += c`, tests only the furthest of them,
//! in `i64` (see [`furthest_exit`]).
//! The original loop then runs what is left, so every iteration runs as it
//! did: the sums keep their order, and a float result its bits. The step may
//! sit anywhere in the body: a `for` over an iterator the optimizer took apart
//! reads the element, steps, and only then runs the body.
//!
//! wasmtime checks each `array.get` against its array's length and shares part
//! of that check only between gets in one basic block, so iterations in one
//! block pay less per get than one alone. A matrix-vector product's inner loop
//! ran +32% at four, and one twice its size +9% at two, where four ran slower.
//! Gemma 3's next token came +30% sooner.
//!
//! The tests ahead are computed before the first `WORK` runs, so the pass needs
//! `EXIT` and `STEP` to read only locals `WORK` and `REST` never write, and to
//! compute nothing that traps. The body may not branch to the loop or its
//! block, which would leave the copy into the original loop rather than past
//! both.
//!
//! A loop that searches, `i < n && x[i] != needle`, reads an array in its test,
//! which may trap. Its copy runs the reads of iterations the loop might never
//! have reached, so it first tests that they lie inside their arrays (see
//! [`Search`]). A substring search ran +40% on it, level with the same loop
//! unrolled by hand.

use crate::wir::{WirInstr, WirPackage, WirType, WirTypeDef};
use crate::wir_visitor::WirMutVisitor;

use super::nullability::Nullability;
use super::peephole::i32_comparison;
use super::util::{Footprint, is_side_effect_free, may_trap_in};

/// The most nodes the copy's iterations may hold together, which picks the
/// largest of [`FACTORS`] that fits.
const UNROLLED_NODES: usize = 192;

const FACTORS: [usize; 2] = [4, 2];

pub(super) fn unroll_array_loops(module: &mut WirPackage) {
    let types = &module.types;
    for func in &mut module.functions {
        let locals = func.typed_locals(types);
        if let Some(body) = &mut func.body {
            Unroller {
                null: Nullability::new(&locals),
                types,
            }
            .visit_body(body);
        }
    }
}

struct Unroller<'a> {
    null: Nullability<'a>,
    types: &'a [WirTypeDef],
}

impl WirMutVisitor for Unroller<'_> {
    fn visit_body(&mut self, body: &mut Vec<WirInstr>) {
        self.walk_body(body);
        let mut at = 0;
        while at < body.len() {
            let before = at.checked_sub(1).map(|prev| &body[prev]);
            if let Some(unrolled) = unrolled(&body[at], before, &self.null, self.types) {
                body.insert(at, unrolled);
                at += 1;
            }
            at += 1;
        }
    }
}

/// The unrolled copy to run ahead of `instr`, when it is a loop this pass
/// unrolls. `before` is the statement ahead of it, which may give the loop
/// variable a constant the copy would leave on at once: a loop of fewer
/// iterations than the copy runs.
fn unrolled(
    instr: &WirInstr,
    before: Option<&WirInstr>,
    null: &Nullability,
    types: &[WirTypeDef],
) -> Option<WirInstr> {
    let WirInstr::Block {
        result: None,
        body: outer,
        ..
    } = instr
    else {
        return None;
    };
    let [WirInstr::Loop { body, .. }] = outer.as_slice() else {
        return None;
    };
    let [
        WirInstr::BrIf {
            depth: 1,
            condition: exit,
        },
        work @ ..,
        WirInstr::Br { depth: 0 },
    ] = body.as_slice()
    else {
        return None;
    };
    let search = Search::of(exit);
    let test = search.as_ref().map_or(exit.as_ref(), |search| search.guard);
    if !is_local_arith(test, null) {
        return None;
    }
    let (at, var, step) = (0..work.len()).rev().find_map(|at| {
        let WirInstr::LocalSet { name, value } = &work[at] else {
            return None;
        };
        let read = [Footprint::reads(exit), Footprint::reads(value)];
        let rest_keeps_them = [&work[..at], &work[at + 1..]].into_iter().all(|rest| {
            let written = Footprint::writes_all(rest, None);
            !written.has_local(name) && !read.iter().any(|r| written.overlaps(r))
        });
        (is_local_arith(value, null) && rest_keeps_them).then_some((at, name, value))
    })?;
    let rest = work[..at].iter().chain(&work[at + 1..]);
    if let Some(search) = search {
        if rest.clone().any(|instr| search.lends_temp_to(instr)) {
            return None;
        }
        let factor = unroll_factor(rest.chain(search.computed()))?;
        let leaves = search.leaves(var, step, factor, null, types)?;
        if work.len() > 1 {
            return Some(copy_of(leaves, work, factor));
        }
        let steps = WirInstr::LocalSet {
            name: var.clone(),
            value: Box::new(stepped(step, var, factor)?),
        };
        return Some(copy_of(leaves, &[steps], 1));
    }
    let factor = unroll_factor(rest)?;
    let mut leave = exit.as_ref().clone();
    let mut ahead = step.as_ref().clone();
    for _ in 1..factor {
        let mut test = exit.as_ref().clone();
        substitute(&mut test, var, &ahead);
        leave = WirInstr::I32Or(Box::new(leave), Box::new(test));
        let mut next = step.as_ref().clone();
        substitute(&mut next, var, &ahead);
        ahead = next;
    }
    if let Some(WirInstr::LocalSet { name, value }) = before
        && name == var
        && let WirInstr::I32Const(entry) = value.as_ref()
        && eval_i32(&leave, var, *entry).is_some_and(|leaves| leaves != 0)
    {
        return None;
    }
    let leave = furthest_exit(exit, var, step, factor).unwrap_or(leave);
    Some(copy_of(vec![leave], work, factor))
}

/// The copy: a loop leaving on the first of `leaves` that holds, tested in
/// order, and otherwise running `work` `factor` times.
fn copy_of(leaves: Vec<WirInstr>, work: &[WirInstr], factor: usize) -> WirInstr {
    let mut looped = Vec::with_capacity(leaves.len() + factor * work.len() + 1);
    looped.extend(leaves.into_iter().map(|leave| WirInstr::BrIf {
        depth: 1,
        condition: Box::new(leave),
    }));
    for _ in 0..factor {
        looped.extend(work.iter().cloned());
    }
    looped.push(WirInstr::Br { depth: 0 });
    WirInstr::Block {
        label: None,
        result: None,
        body: vec![WirInstr::Loop {
            label: None,
            body: looped,
        }],
    }
}

/// The test of a loop that searches: it leaves unless `guard` holds and then
/// `data` does, `(if guard { data } else { 0 }) == 0`, where `data` reads what
/// `guard` keeps in range, as `i < n && x[i] != needle` does. `temps` are the
/// locals `data` is computed through, each set ahead of it.
struct Search<'a> {
    guard: &'a WirInstr,
    temps: Vec<(&'a str, &'a WirInstr)>,
    data: WirInstr,
}

/// `data` without the temporaries it sets on the way to its value, which go to
/// `temps` in the order they are set.
fn without_temps<'a>(
    data: &'a [WirInstr],
    temps: &mut Vec<(&'a str, &'a WirInstr)>,
) -> Option<WirInstr> {
    use WirInstr as I;
    let (value, sets) = data.split_last()?;
    for set in sets {
        let I::LocalSet { name, value } = set else {
            return None;
        };
        temps.push((name, value));
    }
    // A node evaluates its first operand first, so what that operand sets on
    // the way is set ahead of the node too.
    let mut first =
        |operand: &'a WirInstr| without_temps(std::slice::from_ref(operand), temps).map(Box::new);
    Some(match value {
        I::Seq(body) => without_temps(body, temps)?,
        I::I32Eqz(tested) => I::I32Eqz(first(tested)?),
        I::I32Eq(tested, with) => I::I32Eq(first(tested)?, with.clone()),
        I::I32Ne(tested, with) => I::I32Ne(first(tested)?, with.clone()),
        value => value.clone(),
    })
}

impl<'a> Search<'a> {
    fn of(exit: &'a WirInstr) -> Option<Self> {
        use WirInstr as I;
        let tested = match exit {
            I::I32Eqz(tested) => tested,
            I::I32Eq(tested, zero) | I::I32Eq(zero, tested)
                if matches!(zero.as_ref(), I::I32Const(0)) =>
            {
                tested
            }
            _ => return None,
        };
        let I::If {
            condition: guard,
            result: Some(_),
            then_body,
            else_body: Some(else_body),
        } = tested.as_ref()
        else {
            return None;
        };
        let [I::I32Const(0)] = else_body.as_slice() else {
            return None;
        };
        let mut temps = Vec::new();
        let data = without_temps(then_body, &mut temps)?;
        Some(Self { guard, temps, data })
    }

    /// What the copy sizes itself by, beside the loop's work.
    fn computed(&self) -> impl Iterator<Item = &WirInstr> {
        let values = self.temps.iter().map(|&(_, value)| value);
        values.chain(std::iter::once(&self.data))
    }

    /// Whether `instr` reads a temporary. The copy runs every iteration's
    /// `data` ahead of the work, so by then each holds the last one's value,
    /// which only the next `data` may read.
    fn lends_temp_to(&self, instr: &WirInstr) -> bool {
        reads_any(instr, &self.temps)
    }

    /// The copy's tests, in the order it runs them: the furthest `guard`
    /// fails, a read of the `factor` iterations ahead lies outside its array,
    /// one of their `data` fails.
    ///
    /// The copy runs `data` for iterations the loop would not have reached
    /// once an earlier one left, so `data` may do nothing but read arrays, and
    /// the reads ahead are tested to lie inside their arrays first. One that
    /// does not makes the copy leave, for the loop to trap on it if it gets
    /// there.
    fn leaves(
        &self,
        var: &str,
        step: &WirInstr,
        factor: usize,
        null: &Nullability,
        types: &[WirTypeDef],
    ) -> Option<Vec<WirInstr>> {
        use WirInstr as I;
        // A temporary is set before anything reads it, so it never carries one
        // iteration's value into the next.
        let set_before_read = (0..self.temps.len()).all(|at| {
            let (temp, value) = self.temps[at];
            let read = Footprint::reads(value);
            self.temps[at..]
                .iter()
                .all(|(later, _)| !read.has_local(later))
                && self.temps[at + 1..].iter().all(|(later, _)| *later != temp)
        });
        if !set_before_read
            || self.temps.iter().any(|(temp, _)| temp == &var)
            || self.lends_temp_to(self.guard)
            || self.lends_temp_to(step)
        {
            return None;
        }
        let guard_fails = furthest_exit(&negated(self.guard)?, var, step, factor)?;
        let mut reads = Reads {
            var,
            reach: reach(step, var, factor)?,
            null,
            types,
            temps: &self.temps,
            outside: None,
        };
        for computed in self.computed() {
            let mut reads_none = computed.clone();
            reads.take(&mut reads_none)?;
            if !is_local_arith(&reads_none, null) {
                return None;
            }
        }
        let data_fails =
            negated(&self.data).unwrap_or_else(|| I::I32Eqz(Box::new(self.data.clone())));
        let mut data_fails_ahead: Option<WirInstr> = None;
        for ahead in 0..factor {
            let mut fails: Vec<WirInstr> = self
                .temps
                .iter()
                .map(|&(temp, value)| I::LocalSet {
                    name: temp.to_string(),
                    value: Box::new(value.clone()),
                })
                .chain(std::iter::once(data_fails.clone()))
                .collect();
            if ahead > 0 {
                let at = stepped(step, var, ahead)?;
                for instr in &mut fails {
                    substitute(instr, var, &at);
                }
            }
            let fails = match <[WirInstr; 1]>::try_from(fails) {
                Ok([fails]) => fails,
                Err(fails) => I::Seq(fails),
            };
            data_fails_ahead = Some(match data_fails_ahead {
                Some(nearer) => I::I32Or(Box::new(nearer), Box::new(fails)),
                None => fails,
            });
        }
        Some(vec![guard_fails, reads.outside?, data_fails_ahead?])
    }
}

fn reads_any(instr: &WirInstr, temps: &[(&str, &WirInstr)]) -> bool {
    let read = Footprint::reads(instr);
    temps.iter().any(|(temp, _)| read.has_local(temp))
}

/// The array reads of a search's `data`, and the test that one of them, over
/// the copy's iterations, lies outside its array.
struct Reads<'a> {
    var: &'a str,
    /// How far the copy's last iteration starts past its first.
    reach: i64,
    null: &'a Nullability<'a>,
    types: &'a [WirTypeDef],
    temps: &'a [(&'a str, &'a WirInstr)],
    outside: Option<WirInstr>,
}

impl Reads<'_> {
    /// Replace each array read under `data` with a constant, having added its
    /// test to `outside`. None where a read is not one the copy can test: its
    /// array must be a local known non-null, and its index either `var` plus
    /// terms that hold through the loop, or a byte.
    fn take(&mut self, data: &mut WirInstr) -> Option<()> {
        use WirInstr as I;
        if let I::ArrayGet { array, index, .. }
        | I::ArrayGetS { array, index, .. }
        | I::ArrayGetU { array, index, .. } = data
        {
            let test = self.outside_of(array, index)?;
            self.outside = Some(match self.outside.take() {
                Some(others) => I::I32Or(Box::new(others), Box::new(test)),
                None => test,
            });
            *data = I::I32Const(0);
            return Some(());
        }
        // Every read is one the loop makes whenever it gets to `data`, which
        // `outside_of` relies on for the copy's first iteration.
        if matches!(data, I::If { .. } | I::Block { .. } | I::Loop { .. }) {
            return (!Footprint::reads(data).touches_heap()).then_some(());
        }
        let mut taken = Some(());
        data.for_each_boxed_child_mut(&mut |child| {
            if taken.is_some() {
                taken = self.take(child);
            }
        });
        taken
    }

    fn outside_of(&mut self, array: &WirInstr, index: &WirInstr) -> Option<WirInstr> {
        use WirInstr as I;
        let reads_temp = |instr: &WirInstr| reads_any(instr, self.temps);
        if array.local_read().is_none() || !self.null.is_nonnull(array) || reads_temp(array) {
            return None;
        }
        let len = I::ArrayLen(Box::new(array.clone()));
        if self.is_byte(index) {
            // Whatever byte the copy reads ahead, a table of 256 holds it.
            let mut index = index.clone();
            self.take(&mut index)?;
            return is_local_arith(&index, self.null)
                .then(|| I::I32LtU(Box::new(len), Box::new(I::I32Const(256))));
        }
        let mut terms = Vec::new();
        let mut reads_var = 0;
        index_terms(index, self.var, &mut terms, &mut reads_var);
        if reads_var != 1
            || !terms
                .iter()
                .all(|term| is_local_arith(term, self.null) && !reads_temp(term))
        {
            return None;
        }
        // In `i64` the sum cannot wrap, and one inside the array is the index
        // the `i32` sum gives. Compared unsigned, a last index below zero
        // lies outside too. A first index below zero with the last inside is
        // the copy's first read, the one the loop makes and traps on.
        let first = terms
            .into_iter()
            .map(|term| I::I64ExtendI32S(Box::new(term.clone())))
            .reduce(|sum, term| I::I64Add(Box::new(sum), Box::new(term)))
            .expect("the index reads the loop variable");
        let last = I::I64Add(Box::new(first), Box::new(I::I64Const(self.reach)));
        Some(I::I64GeU(
            Box::new(last),
            Box::new(I::I64ExtendI32U(Box::new(len))),
        ))
    }

    /// Whether `index` is a byte read from an array, itself or through a
    /// temporary set to one.
    fn is_byte(&self, index: &WirInstr) -> bool {
        let reads_byte = |instr: &WirInstr| {
            matches!(
                instr,
                WirInstr::ArrayGet { type_id, .. } | WirInstr::ArrayGetU { type_id, .. }
                    if matches!(
                        &self.types[type_id.index() as usize],
                        WirTypeDef::Array(array) if array.element_type == WirType::U8
                    )
            )
        };
        reads_byte(index)
            || index.local_read().is_some_and(|name| {
                self.temps
                    .iter()
                    .any(|&(temp, value)| temp == name && reads_byte(value))
            })
    }
}

/// The comparison that holds exactly where `test` does not.
fn negated(test: &WirInstr) -> Option<WirInstr> {
    use WirInstr as I;
    let (a, b) = match test {
        I::I32Eq(a, b)
        | I::I32Ne(a, b)
        | I::I32LtS(a, b)
        | I::I32LeS(a, b)
        | I::I32GtS(a, b)
        | I::I32GeS(a, b)
        | I::I32LtU(a, b)
        | I::I32LeU(a, b)
        | I::I32GtU(a, b)
        | I::I32GeU(a, b) => (a.clone(), b.clone()),
        _ => return None,
    };
    Some(match test {
        I::I32Eq(..) => I::I32Ne(a, b),
        I::I32Ne(..) => I::I32Eq(a, b),
        I::I32LtS(..) => I::I32GeS(a, b),
        I::I32LeS(..) => I::I32GtS(a, b),
        I::I32GtS(..) => I::I32LeS(a, b),
        I::I32GeS(..) => I::I32LtS(a, b),
        I::I32LtU(..) => I::I32GeU(a, b),
        I::I32LeU(..) => I::I32GtU(a, b),
        I::I32GtU(..) => I::I32LeU(a, b),
        _ => I::I32LtU(a, b),
    })
}

/// The terms `index` adds up, and how many of them read `var` itself. A term
/// that reads `var` any other way counts twice, which no caller accepts.
fn index_terms<'a>(
    index: &'a WirInstr,
    var: &str,
    terms: &mut Vec<&'a WirInstr>,
    reads_var: &mut usize,
) {
    if let WirInstr::I32Add(l, r) = index {
        index_terms(l, var, terms, reads_var);
        index_terms(r, var, terms, reads_var);
        return;
    }
    if index.local_read() == Some(var) {
        *reads_var += 1;
    } else if Footprint::reads(index).has_local(var) {
        *reads_var += 2;
    }
    terms.push(index);
}

/// The positive constant `c` of a loop stepping `var += c`.
fn stride(step: &WirInstr, var: &str) -> Option<i32> {
    use WirInstr as I;
    let I::I32Add(l, r) = step else {
        return None;
    };
    match (l.as_ref(), r.as_ref()) {
        (I::LocalGet { name, .. }, I::I32Const(c)) | (I::I32Const(c), I::LocalGet { name, .. })
            if name == var && *c > 0 =>
        {
            Some(*c)
        }
        _ => None,
    }
}

/// What `var` holds `times` steps on, as one sum.
fn stepped(step: &WirInstr, var: &str, times: usize) -> Option<WirInstr> {
    let times = i32::try_from(times).expect("an unroll factor fits i32");
    let ahead = stride(step, var)?.wrapping_mul(times);
    let mut stepped = step.clone();
    stepped.for_each_boxed_child_mut(&mut |operand| {
        if let WirInstr::I32Const(c) = operand {
            *c = ahead;
        }
    });
    Some(stepped)
}

/// How far the copy's last iteration starts past its first, for a loop
/// stepping `var += c` with a positive constant `c`.
fn reach(step: &WirInstr, var: &str, factor: usize) -> Option<i64> {
    let ahead = i64::try_from(factor - 1).expect("an unroll factor fits i64");
    Some(ahead * i64::from(stride(step, var)?))
}

/// One test in place of the copy's `factor`, for a loop leaving on `v > x` or
/// `v >= x` (or the same written `x < v`, `x <= v`, signed or unsigned) and
/// stepping `v += c` with a positive constant `c`: the furthest one,
/// `v + (factor - 1) * c` against `x`, compared in `i64`. There the sum cannot
/// wrap, so where it stays in range every nearer `v` does too, and each test the
/// copy skips would have kept the loop going. Where it leaves early, the
/// original loop runs those iterations.
fn furthest_exit(exit: &WirInstr, var: &str, step: &WirInstr, factor: usize) -> Option<WirInstr> {
    use WirInstr as I;
    let reach = reach(step, var, factor)?;
    let (v, x, signed, strict) = match exit {
        I::I32GtS(v, x) | I::I32LtS(x, v) => (v, x, true, true),
        I::I32GeS(v, x) | I::I32LeS(x, v) => (v, x, true, false),
        I::I32GtU(v, x) | I::I32LtU(x, v) => (v, x, false, true),
        I::I32GeU(v, x) | I::I32LeU(x, v) => (v, x, false, false),
        _ => return None,
    };
    if v.local_read() != Some(var) || Footprint::reads(x).has_local(var) {
        return None;
    }
    let widen = |instr: &WirInstr| {
        let instr = Box::new(instr.clone());
        Box::new(if signed {
            I::I64ExtendI32S(instr)
        } else {
            I::I64ExtendI32U(instr)
        })
    };
    let furthest = Box::new(I::I64Add(widen(v), Box::new(I::I64Const(reach))));
    // Both sides widened fit `i64` with room, so a signed compare orders the
    // unsigned ones too.
    Some(if strict {
        I::I64GtS(furthest, widen(x))
    } else {
        I::I64GeS(furthest, widen(x))
    })
}

/// How many iterations the copy of `work` runs, when it is a loop body the copy
/// pays for: small, reading a GC array, holding no loop of its own, declaring
/// no local, calling nothing, and never branching to the loop (depth 0) or its
/// block (depth 1). A call costs more than the checks the copy shares, so the
/// copy would only add code: unrolling `json_canada`'s serializer loops left
/// `ser` flat and moved the deserializer enough to cost `de` 8%.
fn unroll_factor<'a>(work: impl IntoIterator<Item = &'a WirInstr>) -> Option<usize> {
    let mut nodes = 0;
    let mut reads_array = false;
    let mut refused = false;
    for instr in work {
        scan(instr, 0, &mut |node, nesting| {
            nodes += 1;
            reads_array |= matches!(
                node,
                WirInstr::ArrayGet { .. } | WirInstr::ArrayGetS { .. } | WirInstr::ArrayGetU { .. }
            );
            refused |= match node {
                WirInstr::Loop { .. }
                | WirInstr::DeclareLocal { .. }
                | WirInstr::Call { .. }
                | WirInstr::CallIndirect { .. }
                | WirInstr::CallRef { .. } => true,
                WirInstr::Br { depth } | WirInstr::BrIf { depth, .. } => {
                    (nesting..nesting + 2).contains(depth)
                }
                WirInstr::BrTable {
                    targets, default, ..
                } => targets
                    .iter()
                    .chain(std::iter::once(default))
                    .any(|depth| (nesting..nesting + 2).contains(depth)),
                _ => false,
            };
        });
    }
    if !reads_array || refused {
        return None;
    }
    FACTORS
        .into_iter()
        .find(|factor| factor * nodes <= UNROLLED_NODES)
}

/// Visit `instr` and each node under it with the labels it sits inside of,
/// counted from the loop body.
fn scan(instr: &WirInstr, nesting: u32, f: &mut impl FnMut(&WirInstr, u32)) {
    f(instr, nesting);
    let inner = match instr {
        WirInstr::Block { .. } | WirInstr::Loop { .. } => nesting + 1,
        WirInstr::If { condition, .. } => {
            scan(condition, nesting, f);
            nesting + 1
        }
        _ => nesting,
    };
    match instr {
        WirInstr::If {
            then_body,
            else_body,
            ..
        } => {
            for child in then_body.iter().chain(else_body.iter().flatten()) {
                scan(child, inner, f);
            }
        }
        _ => instr.for_each_child(&mut |child| scan(child, inner, f)),
    }
}

/// Whether `instr` computes from locals alone, writing nothing and trapping
/// nowhere, so evaluating it early or twice changes nothing.
fn is_local_arith(instr: &WirInstr, null: &Nullability) -> bool {
    is_side_effect_free(instr)
        && !Footprint::reads(instr).touches_heap()
        && !may_trap_in(instr, null)
}

/// What the i32 `instr` computes with `var` holding `value`, where it is
/// integer arithmetic this knows.
fn eval_i32(instr: &WirInstr, var: &str, value: i32) -> Option<i32> {
    use WirInstr as I;
    let eval = |operand: &WirInstr| eval_i32(operand, var, value);
    let both = |a: &WirInstr, b: &WirInstr| Some((eval(a)?, eval(b)?));
    if let Some((a, b, op)) = i32_comparison(instr) {
        return both(a, b).map(|(a, b)| i32::from(op(a, b)));
    }
    Some(match instr {
        I::I32Const(c) => *c,
        I::LocalGet { name, .. } if name == var => value,
        I::I32Add(a, b) => both(a, b).map(|(a, b)| a.wrapping_add(b))?,
        I::I32Sub(a, b) => both(a, b).map(|(a, b)| a.wrapping_sub(b))?,
        I::I32Mul(a, b) => both(a, b).map(|(a, b)| a.wrapping_mul(b))?,
        I::I32Or(a, b) => both(a, b).map(|(a, b)| a | b)?,
        I::I32And(a, b) => both(a, b).map(|(a, b)| a & b)?,
        I::I32Eqz(a) => i32::from(eval(a)? == 0),
        _ => return None,
    })
}

/// Replace each read of `var` under `instr` with `value`.
fn substitute(instr: &mut WirInstr, var: &str, value: &WirInstr) {
    if instr.local_read() == Some(var) {
        *instr = value.clone();
        return;
    }
    instr.for_each_boxed_child_mut(&mut |child| substitute(child, var, value));
}
