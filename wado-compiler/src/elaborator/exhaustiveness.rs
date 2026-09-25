//! Match coverage: which values no arm's pattern takes, found by specializing
//! the arm matrix one constructor at a time (Maranget, "Warnings for pattern matching").

use std::rc::Rc;

use crate::tir::TypeId;

/// An arm pattern as coverage reads it. A constructor carries the set its
/// column draws from, so the check reads no types.
pub(super) enum Pat {
    Wild,
    /// A case of an enum (no payload) or a variant (one payload).
    Case {
        cases: Rc<[Case]>,
        index: usize,
        payload: Option<Box<Pat>>,
    },
    Bool(bool),
    /// An inclusive integer range; `domain` is its type's, where it has one.
    Int {
        lo: i128,
        hi: i128,
        domain: Option<IntDomain>,
    },
    /// A tuple, or a struct's fields in declaration order.
    Product {
        fields: Option<Rc<[String]>>,
        elements: Vec<Pat>,
    },
    /// A type pattern the host decides, naming the resource it narrows to.
    Narrow(TypeId),
    /// A value only a runtime comparison matches: a string or a constant.
    Opaque,
    Or(Vec<Pat>),
}

pub(super) struct Case {
    pub(super) name: String,
    pub(super) has_payload: bool,
}

#[derive(Clone, Copy)]
pub(super) struct IntDomain {
    pub(super) min: i128,
    pub(super) max: i128,
    pub(super) is_char: bool,
}

/// A value no arm takes, as a pattern that would take it.
pub(super) enum Witness {
    Wild,
    /// `payload` is `None` where no arm names the case at all.
    Case {
        name: String,
        payload: Option<Box<Witness>>,
    },
    Bool(bool),
    Int {
        lo: i128,
        hi: i128,
        is_char: bool,
    },
    Product {
        fields: Option<Rc<[String]>>,
        elements: Vec<Witness>,
    },
}

enum Ctor {
    Case(Rc<[Case]>, usize),
    Bool(bool),
    Int(i128, i128, bool),
    Product(Option<Rc<[String]>>, usize),
}

impl Ctor {
    fn arity(&self) -> usize {
        match self {
            Ctor::Case(cases, index) => usize::from(cases[*index].has_payload),
            Ctor::Bool(_) | Ctor::Int(..) => 0,
            Ctor::Product(_, arity) => *arity,
        }
    }
}

type Row<'p> = Vec<&'p Pat>;

/// A row of the pattern matrix: its patterns, and what it carries along.
trait MatrixRow<'p>: Sized {
    fn pats(&self) -> &[&'p Pat];
    fn with_pats(&self, pats: Row<'p>) -> Self;
    /// This row behind a guard: it may take a value but covers none.
    fn guarded(&self, pats: Row<'p>) -> Option<Self>;
}

impl<'p> MatrixRow<'p> for Row<'p> {
    fn pats(&self) -> &[&'p Pat] {
        self
    }

    fn with_pats(&self, pats: Row<'p>) -> Self {
        pats
    }

    fn guarded(&self, _: Row<'p>) -> Option<Self> {
        None
    }
}

/// A row standing for one arm.
struct ArmRow<'p> {
    pats: Row<'p>,
    arm: usize,
    guardless: bool,
}

impl<'p> MatrixRow<'p> for ArmRow<'p> {
    fn pats(&self) -> &[&'p Pat] {
        &self.pats
    }

    fn with_pats(&self, pats: Row<'p>) -> Self {
        Self {
            pats,
            arm: self.arm,
            guardless: self.guardless,
        }
    }

    fn guarded(&self, pats: Row<'p>) -> Option<Self> {
        Some(Self {
            pats,
            arm: self.arm,
            guardless: false,
        })
    }
}

/// The values the guardless arms `patterns` leave uncovered, one per head
/// constructor they miss; empty when the match is exhaustive.
pub(super) fn uncovered(patterns: &[&Pat]) -> Vec<Witness> {
    let rows = expand_or(patterns.iter().map(|p| vec![*p]).collect());
    let Some(ctors) = signature(&rows) else {
        return first_uncovered(default_rows(&rows), 0)
            .map(|_| Witness::Wild)
            .into_iter()
            .collect();
    };
    let mut missing: Vec<Witness> = Vec::new();
    let specialized = specialize_each(&rows, &ctors);
    for (ctor, sub) in ctors.into_iter().zip(specialized) {
        let arity = ctor.arity();
        if let Some(args) = first_uncovered(sub, arity) {
            let witness = build(&rows, ctor, args);
            match (missing.last_mut(), &witness) {
                (
                    Some(Witness::Int { hi, .. }),
                    Witness::Int {
                        lo: next_lo,
                        hi: next_hi,
                        ..
                    },
                ) if *hi + 1 == *next_lo => *hi = *next_hi,
                _ => missing.push(witness),
            }
        }
    }
    missing
}

/// Whether some value reaches each arm, past the guardless arms before it. A
/// guarded arm covers nothing but may be unreachable.
pub(super) fn reached_arms(arms: &[(bool, Pat)]) -> Vec<bool> {
    let rows = arms
        .iter()
        .enumerate()
        .map(|(arm, (guardless, pattern))| ArmRow {
            pats: vec![pattern],
            arm,
            guardless: *guardless,
        })
        .collect();
    let mut reached = vec![false; arms.len()];
    mark_reached(rows, &mut reached);
    reached
}

/// Marks each arm some value reaches: the first row taking it, and every
/// guarded row ahead of that one.
fn mark_reached(rows: Vec<ArmRow<'_>>, reached: &mut [bool]) {
    let Some(first) = rows.first() else {
        return;
    };
    if first.pats.is_empty() {
        for row in &rows {
            reached[row.arm] = true;
            if row.guardless {
                break;
            }
        }
        return;
    }
    let rows = expand_or(rows);
    let groups = match signature(&rows) {
        Some(ctors) => specialize_each(&rows, &ctors),
        None => open_groups(&rows),
    };
    for group in groups {
        mark_reached(group, reached);
    }
}

/// A column no constructor set describes, split by value: each row naming one
/// with the wildcard rows around it, then the wildcard rows alone.
fn open_groups<'p>(rows: &[ArmRow<'p>]) -> Vec<Vec<ArmRow<'p>>> {
    let is_wild = |i: &usize| matches!(rows[*i].pats[0], Pat::Wild);
    let wild: Vec<usize> = (0..rows.len()).filter(is_wild).collect();
    let tail = |i: usize| rows[i].with_pats(rows[i].pats[1..].to_vec());
    let mut groups: Vec<Vec<ArmRow<'p>>> = (0..rows.len())
        .filter(|i| !is_wild(i))
        .map(|named| {
            let (before, after) = wild.split_at(wild.partition_point(|&w| w < named));
            before
                .iter()
                .copied()
                .chain([named])
                .chain(after.iter().copied())
                .map(tail)
                .collect()
        })
        .collect();
    groups.push(default_rows(rows));
    groups
}

/// One uncovered value vector for `rows` of `width` columns, if any.
fn first_uncovered(rows: Vec<Row<'_>>, width: usize) -> Option<Vec<Witness>> {
    if width == 0 {
        return rows.is_empty().then(Vec::new);
    }
    let rows = expand_or(rows);
    let Some(ctors) = signature(&rows) else {
        let mut rest = first_uncovered(default_rows(&rows), width - 1)?;
        rest.insert(0, Witness::Wild);
        return Some(rest);
    };
    let specialized = specialize_each(&rows, &ctors);
    for (ctor, sub) in ctors.into_iter().zip(specialized) {
        let arity = ctor.arity();
        if let Some(mut values) = first_uncovered(sub, arity + width - 1) {
            let rest = values.split_off(arity);
            let mut out = vec![build(&rows, ctor, values)];
            out.extend(rest);
            return Some(out);
        }
    }
    None
}

fn expand_or<'p, R: MatrixRow<'p>>(rows: Vec<R>) -> Vec<R> {
    let mut out = Vec::new();
    for row in rows {
        push_expanded(row, &mut out);
    }
    out
}

fn push_expanded<'p, R: MatrixRow<'p>>(row: R, out: &mut Vec<R>) {
    if let Some(&head) = row.pats().first()
        && let Pat::Or(alternatives) = head
    {
        for alternative in alternatives {
            let mut expanded = vec![alternative];
            expanded.extend_from_slice(&row.pats()[1..]);
            push_expanded(row.with_pats(expanded), out);
        }
        return;
    }
    out.push(row);
}

/// Every constructor of the head column's type, or `None` where the set is
/// open (or no row names one) and only a wildcard covers it.
fn signature<'p, R: MatrixRow<'p>>(rows: &[R]) -> Option<Vec<Ctor>> {
    let head = rows
        .iter()
        .map(|row| row.pats()[0])
        .find(|p| !matches!(p, Pat::Wild | Pat::Opaque))?;
    match head {
        Pat::Case { cases, .. } => Some(
            (0..cases.len())
                .map(|index| Ctor::Case(Rc::clone(cases), index))
                .collect(),
        ),
        Pat::Bool(_) => Some(vec![Ctor::Bool(true), Ctor::Bool(false)]),
        Pat::Int {
            domain: Some(domain),
            ..
        } => Some(split_domain(rows, *domain)),
        Pat::Product { fields, elements } => {
            Some(vec![Ctor::Product(fields.clone(), elements.len())])
        }
        Pat::Int { domain: None, .. } | Pat::Narrow(_) => None,
        Pat::Wild | Pat::Opaque | Pat::Or(_) => {
            unreachable!("a head is expanded and names a constructor")
        }
    }
}

/// The domain cut at every range boundary in the column, so each piece lies
/// wholly inside or outside every range.
fn split_domain<'p, R: MatrixRow<'p>>(rows: &[R], domain: IntDomain) -> Vec<Ctor> {
    let mut cuts = vec![domain.min, domain.max + 1];
    for row in rows {
        if let Pat::Int { lo, hi, .. } = row.pats()[0] {
            cuts.extend([
                (*lo).max(domain.min),
                hi.saturating_add(1).min(domain.max + 1),
            ]);
        }
    }
    cuts.sort_unstable();
    cuts.dedup();
    cuts.windows(2)
        .map(|w| Ctor::Int(w[0], w[1] - 1, domain.is_char))
        .collect()
}

/// `rows` specialized to each of `ctors`, which [`signature`] and
/// [`split_domain`] list in order. A row reaches only the constructors its head
/// can take, so a long match of literals costs its length rather than its square.
fn specialize_each<'p, R: MatrixRow<'p>>(rows: &[R], ctors: &[Ctor]) -> Vec<Vec<R>> {
    let mut out: Vec<Vec<R>> = std::iter::repeat_with(Vec::new).take(ctors.len()).collect();
    for row in rows {
        if let Pat::Opaque = row.pats()[0] {
            for (group, ctor) in out.iter_mut().zip(ctors) {
                let mut pats = vec![&Pat::Wild; ctor.arity()];
                pats.extend_from_slice(&row.pats()[1..]);
                group.extend(row.guarded(pats));
            }
            continue;
        }
        let reach = match row.pats()[0] {
            Pat::Case { index, .. } => {
                let at = |c: &Ctor| match c {
                    Ctor::Case(_, i) => *i,
                    _ => unreachable!("a column holds one kind of constructor"),
                };
                ctors.partition_point(|c| at(c) < *index)
                    ..ctors.partition_point(|c| at(c) <= *index)
            }
            Pat::Int { lo, hi, .. } => {
                let piece = |c: &Ctor| match c {
                    Ctor::Int(a, b, _) => (*a, *b),
                    _ => unreachable!("a column holds one kind of constructor"),
                };
                ctors.partition_point(|c| piece(c).1 < *lo)
                    ..ctors.partition_point(|c| piece(c).0 <= *hi)
            }
            _ => 0..ctors.len(),
        };
        for i in reach {
            out[i].extend(specialize_row(row.pats(), &ctors[i]).map(|pats| row.with_pats(pats)));
        }
    }
    out
}

fn specialize_row<'p>(row: &[&'p Pat], ctor: &Ctor) -> Option<Row<'p>> {
    const WILD: &Pat = &Pat::Wild;
    let arity = ctor.arity();
    let mut out: Row<'p> = match (row[0], ctor) {
        (Pat::Wild, _) => vec![WILD; arity],
        (Pat::Case { index, payload, .. }, Ctor::Case(_, wanted)) if index == wanted => {
            match (payload, arity) {
                (_, 0) => Vec::new(),
                (Some(payload), _) => vec![&**payload],
                (None, _) => vec![WILD],
            }
        }
        (Pat::Bool(value), Ctor::Bool(wanted)) if value == wanted => Vec::new(),
        (Pat::Int { lo, hi, .. }, Ctor::Int(a, b, _)) if lo <= a && b <= hi => Vec::new(),
        (Pat::Product { elements, .. }, Ctor::Product(..)) => elements.iter().collect(),
        _ => return None,
    };
    out.extend_from_slice(&row[1..]);
    Some(out)
}

fn default_rows<'p, R: MatrixRow<'p>>(rows: &[R]) -> Vec<R> {
    rows.iter()
        .filter(|row| matches!(row.pats()[0], Pat::Wild))
        .map(|row| row.with_pats(row.pats()[1..].to_vec()))
        .collect()
}

fn build(rows: &[Row<'_>], ctor: Ctor, args: Vec<Witness>) -> Witness {
    match ctor {
        Ctor::Case(cases, index) => {
            let named = rows
                .iter()
                .any(|row| matches!(row[0], Pat::Case { index: i, .. } if *i == index));
            Witness::Case {
                name: cases[index].name.clone(),
                payload: named
                    .then(|| args.into_iter().next().map(Box::new))
                    .flatten(),
            }
        }
        Ctor::Bool(value) => Witness::Bool(value),
        Ctor::Int(lo, hi, is_char) => Witness::Int { lo, hi, is_char },
        Ctor::Product(fields, _) => Witness::Product {
            fields,
            elements: args,
        },
    }
}

impl std::fmt::Display for Witness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Witness::Wild => write!(f, "_"),
            Witness::Case {
                name,
                payload: None,
            } => write!(f, "{name}"),
            Witness::Case {
                name,
                payload: Some(payload),
            } => write!(f, "{name}({payload})"),
            Witness::Bool(value) => write!(f, "{value}"),
            Witness::Int { lo, hi, is_char } => {
                let show = |f: &mut std::fmt::Formatter<'_>, v: i128| match u32::try_from(v)
                    .ok()
                    .and_then(char::from_u32)
                {
                    Some(c) if *is_char => write!(f, "{c:?}"),
                    _ => write!(f, "{v}"),
                };
                show(f, *lo)?;
                if lo != hi {
                    write!(f, "..=")?;
                    show(f, *hi)?;
                }
                Ok(())
            }
            Witness::Product {
                fields: None,
                elements,
            } => {
                write!(f, "[")?;
                for (i, element) in elements.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{element}")?;
                }
                write!(f, "]")
            }
            Witness::Product {
                fields: Some(fields),
                elements,
            } => {
                write!(f, "{{ ")?;
                for (name, element) in fields.iter().zip(elements) {
                    if !matches!(element, Witness::Wild) {
                        write!(f, "{name}: {element}, ")?;
                    }
                }
                write!(f, ".. }}")
            }
        }
    }
}
