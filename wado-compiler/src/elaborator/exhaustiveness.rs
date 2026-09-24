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
    for ctor in ctors {
        let arity = ctor.arity();
        if let Some(args) = first_uncovered(specialize(&rows, &ctor), arity) {
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

/// The arms no value reaches: each index whose pattern the guardless arms
/// before it already cover. A guarded arm covers nothing but may be unreachable.
pub(super) fn unreachable_arms(arms: &[(bool, &Pat)]) -> Vec<usize> {
    let mut covering: Vec<Row<'_>> = Vec::new();
    let mut out = Vec::new();
    for (index, &(guardless, pattern)) in arms.iter().enumerate() {
        if !useful(covering.clone(), vec![pattern]) {
            out.push(index);
        }
        if guardless {
            covering.push(vec![pattern]);
        }
    }
    out
}

/// Whether some value `q` matches escapes every row of `rows`. A head no
/// constructor describes (a string, a narrowing) is covered only by a wildcard.
fn useful<'p>(rows: Vec<Row<'p>>, q: Row<'p>) -> bool {
    let Some(&head) = q.first() else {
        return rows.is_empty();
    };
    if let Pat::Or(alternatives) = head {
        return alternatives.iter().any(|alternative| {
            let mut row = vec![alternative];
            row.extend_from_slice(&q[1..]);
            useful(rows.clone(), row)
        });
    }
    let rows = expand_or(rows);
    let ctors = match head {
        Pat::Wild => signature(&rows),
        Pat::Case { cases, index, .. } => Some(vec![Ctor::Case(Rc::clone(cases), *index)]),
        Pat::Bool(value) => Some(vec![Ctor::Bool(*value)]),
        Pat::Int {
            lo,
            hi,
            domain: Some(domain),
        } => {
            let mut cut_by = rows.clone();
            cut_by.push(q.clone());
            Some(
                split_domain(&cut_by, *domain)
                    .into_iter()
                    .filter(|ctor| matches!(ctor, Ctor::Int(a, b, _) if lo <= a && b <= hi))
                    .collect(),
            )
        }
        Pat::Product { fields, elements } => {
            Some(vec![Ctor::Product(fields.clone(), elements.len())])
        }
        Pat::Int { domain: None, .. } | Pat::Narrow(_) | Pat::Opaque => None,
        Pat::Or(_) => unreachable!("an or-pattern head is expanded above"),
    };
    let Some(ctors) = ctors else {
        return useful(default_rows(&rows), q[1..].to_vec());
    };
    ctors.iter().any(|ctor| {
        specialize(std::slice::from_ref(&q), ctor)
            .pop()
            .is_some_and(|specialized| useful(specialize(&rows, ctor), specialized))
    })
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
    for ctor in ctors {
        let arity = ctor.arity();
        if let Some(mut values) = first_uncovered(specialize(&rows, &ctor), arity + width - 1) {
            let rest = values.split_off(arity);
            let mut out = vec![build(&rows, ctor, values)];
            out.extend(rest);
            return Some(out);
        }
    }
    None
}

fn expand_or(rows: Vec<Row<'_>>) -> Vec<Row<'_>> {
    let mut out = Vec::new();
    for row in rows {
        push_expanded(row, &mut out);
    }
    out
}

fn push_expanded<'p>(row: Row<'p>, out: &mut Vec<Row<'p>>) {
    match row.first() {
        Some(Pat::Or(alternatives)) => {
            for alternative in alternatives {
                let mut expanded = vec![alternative];
                expanded.extend_from_slice(&row[1..]);
                push_expanded(expanded, out);
            }
        }
        _ => out.push(row),
    }
}

/// Every constructor of the head column's type, or `None` where the set is
/// open (or no row names one) and only a wildcard covers it.
fn signature(rows: &[Row<'_>]) -> Option<Vec<Ctor>> {
    let head = rows
        .iter()
        .map(|row| row[0])
        .find(|p| !matches!(p, Pat::Wild))?;
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
        Pat::Int { domain: None, .. } | Pat::Narrow(_) | Pat::Opaque => None,
        Pat::Wild | Pat::Or(_) => unreachable!("a head is expanded and not a wildcard"),
    }
}

/// The domain cut at every range boundary in the column, so each piece lies
/// wholly inside or outside every range.
fn split_domain(rows: &[Row<'_>], domain: IntDomain) -> Vec<Ctor> {
    let mut cuts = vec![domain.min, domain.max + 1];
    for row in rows {
        if let Pat::Int { lo, hi, .. } = row[0] {
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

fn specialize<'p>(rows: &[Row<'p>], ctor: &Ctor) -> Vec<Row<'p>> {
    const WILD: &Pat = &Pat::Wild;
    let arity = ctor.arity();
    rows.iter()
        .filter_map(|row| {
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
        })
        .collect()
}

fn default_rows<'p>(rows: &[Row<'p>]) -> Vec<Row<'p>> {
    rows.iter()
        .filter(|row| matches!(row[0], Pat::Wild))
        .map(|row| row[1..].to_vec())
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
