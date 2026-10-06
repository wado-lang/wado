//! What a body-less declaration states about every call to it, read from
//! `#[storage(...)]` and `#[side_effect(...)]`. See
//! [WEP: Builtin Storage and Side-Effect Attributes](../../docs/wep-2026-10-04-builtin-storage-side-effect.md).

use crate::ast::{AttrArg, Attribute};
use crate::attribute::{SIDE_EFFECT, STORAGE};
use crate::hashmap::IndexSet;
use crate::lower::plan::value_copy::place::{is_reference, may_carry_storage};
use crate::tir::{ResolvedType, TypeId, TypeTable};

/// `#[storage(...)]`: what a call's result shares with its arguments, and what
/// the call keeps of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Storage {
    /// Shares no storage and keeps none.
    None,
    /// The result is new storage holding nothing the call was handed.
    Fresh,
    /// The result is an argument's storage, or part of it.
    PartOfArgs,
    /// The result is new storage holding what the arguments hold.
    HoldsArgs,
    /// The result is new storage holding value copies of what the arguments
    /// hold, so making it reads everything they reach.
    CopiesArgs,
    /// The call stores what the arguments hold into its one `&mut` argument.
    StoresArgs,
    /// Nothing is known.
    Opaque,
}

impl Storage {
    const WRITTEN: [(&'static str, Self); 7] = [
        ("none", Self::None),
        ("fresh", Self::Fresh),
        ("part_of_args", Self::PartOfArgs),
        ("holds_args", Self::HoldsArgs),
        ("copies_args", Self::CopiesArgs),
        ("stores_args", Self::StoresArgs),
        ("opaque", Self::Opaque),
    ];

    fn parse(word: &str) -> Option<Self> {
        Self::WRITTEN
            .iter()
            .find_map(|(name, value)| (*name == word).then_some(*value))
    }

    /// Whether the result is new storage, the values `len` goes with.
    pub fn returns_new_storage(self) -> bool {
        matches!(self, Self::Fresh | Self::HoldsArgs | Self::CopiesArgs)
    }

    /// Whether the value says something about the result, which a `()`
    /// return has none of.
    fn describes_result(self) -> bool {
        matches!(
            self,
            Self::Fresh | Self::PartOfArgs | Self::HoldsArgs | Self::CopiesArgs
        )
    }

    /// Whether the value relates the call to the arguments' storage.
    fn shares_args(self) -> bool {
        matches!(
            self,
            Self::PartOfArgs | Self::HoldsArgs | Self::CopiesArgs | Self::StoresArgs
        )
    }
}

/// One condition under which a call traps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrapCheck<P> {
    /// `negative = p`: traps when `p < 0`.
    Negative(P),
    /// One `outside` entry: traps unless `[at, at + count)` lies within
    /// `array`. `at` defaults to 0 and `count` to 1.
    Outside {
        array: P,
        at: Option<P>,
        count: Option<P>,
    },
    /// `unset = a`: traps when the element of `a` it reaches holds no value.
    Unset(P),
}

/// When a call may trap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trap<P> {
    /// No `trap`.
    Never,
    /// `trap` with no condition key.
    Anywhere,
    /// `trap` with condition keys: these are the only ways it traps.
    Only(Vec<TrapCheck<P>>),
}

/// `#[side_effect(...)]`: what a call does beyond what its signature states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideEffect<P> {
    /// `none`, or any of `trap`, `read`, `write` and `hint`.
    Listed {
        trap: Trap<P>,
        /// Reads linear memory.
        read: bool,
        /// Writes linear memory.
        write: bool,
        /// Computes nothing, but its position is what it means.
        hint: bool,
    },
    /// Nothing is known.
    Opaque,
    /// `builtin::black_box`: the optimizer may assume nothing about the
    /// operand or the result.
    BlackBox,
}

/// Everything a body-less declaration states about its calls, naming
/// parameters as `P`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinFacts<P> {
    /// `#[storage]`'s value.
    pub storage: Storage,
    /// `len = p`: the returned array holds `p` elements.
    pub len: Option<P>,
    /// `#[side_effect]`.
    pub side_effect: SideEffect<P>,
    /// `suspend` in `#[side_effect]`: other tasks may run before the call
    /// returns.
    pub suspend: bool,
}

impl<P> BuiltinFacts<P> {
    /// The same facts with every parameter renamed by `f`.
    pub fn map<Q>(&self, mut f: impl FnMut(&P) -> Q) -> BuiltinFacts<Q> {
        let mut check = |check: &TrapCheck<P>| match check {
            TrapCheck::Negative(p) => TrapCheck::Negative(f(p)),
            TrapCheck::Outside { array, at, count } => TrapCheck::Outside {
                array: f(array),
                at: at.as_ref().map(&mut f),
                count: count.as_ref().map(&mut f),
            },
            TrapCheck::Unset(array) => TrapCheck::Unset(f(array)),
        };
        let side_effect = match &self.side_effect {
            SideEffect::Listed {
                trap,
                read,
                write,
                hint,
            } => SideEffect::Listed {
                trap: match trap {
                    Trap::Never => Trap::Never,
                    Trap::Anywhere => Trap::Anywhere,
                    Trap::Only(checks) => Trap::Only(checks.iter().map(&mut check).collect()),
                },
                read: *read,
                write: *write,
                hint: *hint,
            },
            SideEffect::Opaque => SideEffect::Opaque,
            SideEffect::BlackBox => SideEffect::BlackBox,
        };
        BuiltinFacts {
            storage: self.storage,
            len: self.len.as_ref().map(&mut f),
            side_effect,
            suspend: self.suspend,
        }
    }

    /// Whether the call's effects are unknown: `opaque` or `black_box`.
    pub fn is_opaque(&self) -> bool {
        matches!(self.side_effect, SideEffect::Opaque | SideEffect::BlackBox)
    }

    /// The trap conditions, `None` where the call never traps. `black_box`
    /// hands its operand back, which cannot trap.
    pub fn trap(&self) -> Option<&Trap<P>> {
        match &self.side_effect {
            SideEffect::Listed {
                trap: Trap::Never, ..
            }
            | SideEffect::BlackBox => None,
            SideEffect::Listed { trap, .. } => Some(trap),
            SideEffect::Opaque => Some(&Trap::Anywhere),
        }
    }

    /// The two attributes as they are written.
    pub fn written(&self) -> [String; 2]
    where
        P: std::fmt::Display,
    {
        let word = Storage::WRITTEN
            .iter()
            .find_map(|(name, value)| (*value == self.storage).then_some(*name))
            .expect("every storage value has a spelling");
        let storage = match &self.len {
            Some(len) => format!("#[storage({word}, len = {len})]"),
            None => format!("#[storage({word})]"),
        };
        let mut effects: Vec<String> = match &self.side_effect {
            SideEffect::Opaque => vec!["opaque".to_string()],
            SideEffect::BlackBox => vec!["black_box".to_string()],
            SideEffect::Listed {
                trap,
                read,
                write,
                hint,
            } => {
                let mut out = Vec::new();
                if !matches!(trap, Trap::Never) {
                    out.push("trap".to_string());
                }
                for (on, word) in [(*read, "read"), (*write, "write"), (*hint, "hint")] {
                    if on {
                        out.push(word.to_string());
                    }
                }
                out.extend(written_checks(self.trap_checks()));
                if out.is_empty() && !self.suspend {
                    out.push("none".to_string());
                }
                out
            }
        };
        if self.suspend {
            effects.push("suspend".to_string());
        }
        [storage, format!("#[side_effect({})]", effects.join(", "))]
    }

    /// The trap checks, empty where the call traps anywhere or never.
    pub fn trap_checks(&self) -> &[TrapCheck<P>] {
        match self.trap() {
            Some(Trap::Only(checks)) => checks,
            Some(Trap::Anywhere | Trap::Never) | None => &[],
        }
    }

    /// The arrays `outside` names: the call reaches their elements in range,
    /// and leaves the arrays themselves in place.
    pub fn ranged_arrays(&self) -> impl Iterator<Item = &P> {
        self.trap_checks().iter().filter_map(|check| match check {
            TrapCheck::Outside { array, .. } => Some(array),
            TrapCheck::Negative(_) | TrapCheck::Unset(_) => None,
        })
    }
}

/// Trap checks as the keys that state them.
fn written_checks<P: std::fmt::Display>(checks: &[TrapCheck<P>]) -> Vec<String> {
    let mut outside = Vec::new();
    let mut at = Vec::new();
    let mut out = Vec::new();
    let mut count = None;
    for check in checks {
        match check {
            TrapCheck::Outside {
                array,
                at: start,
                count: n,
            } => {
                outside.push(array.to_string());
                at.extend(start.as_ref().map(ToString::to_string));
                count = n.as_ref().map(ToString::to_string);
            }
            TrapCheck::Unset(array) => out.push(format!("unset = {array}")),
            TrapCheck::Negative(p) => out.push(format!("negative = {p}")),
        }
    }
    let mut keys = Vec::new();
    if !outside.is_empty() {
        keys.push(format!("outside = [{}]", outside.join(", ")));
    }
    if !at.is_empty() {
        keys.push(format!("at = [{}]", at.join(", ")));
    }
    keys.extend(count.map(|n| format!("count = {n}")));
    keys.extend(out);
    keys
}

/// One parameter as its type states facts: the signature's half of what a
/// declaration says about its calls.
#[derive(Debug, Clone, Copy)]
pub struct ParamShape<'a> {
    /// The name the attributes call it by.
    pub name: &'a str,
    /// Some instantiation of its type can carry storage.
    pub carries_storage: bool,
    /// Taken through a reference, so it hands on what it points to.
    pub is_reference: bool,
    /// Taken by `&mut`.
    pub is_mut_ref: bool,
    /// An `Array<T>`, by value or through a reference.
    pub is_array: bool,
    /// An integer, as a count or an index is.
    pub is_integer: bool,
}

impl<'a> ParamShape<'a> {
    /// The parameter `name` of type `ty`.
    pub fn of(name: &'a str, ty: TypeId, type_table: &TypeTable) -> Self {
        Self {
            name,
            carries_storage: may_carry_storage(ty, type_table),
            is_reference: is_reference(ty, type_table),
            is_mut_ref: matches!(type_table.get(ty), ResolvedType::MutRef(_)),
            is_array: is_array(ty, type_table),
            is_integer: type_table.is_integer(ty),
        }
    }
}

/// The return type as it states facts.
#[derive(Debug, Clone, Copy)]
pub struct ReturnShape {
    /// Neither `()` nor `!`.
    pub returns_value: bool,
    /// Some instantiation of it can carry storage.
    pub carries_storage: bool,
    /// An `Array<T>`, by value or through a reference.
    pub is_array: bool,
    /// A `&mut`.
    pub is_mut_ref: bool,
    /// `!`, so every call ends in a trap.
    pub is_never: bool,
}

impl ReturnShape {
    /// The return type `ty`.
    pub fn of(ty: TypeId, type_table: &TypeTable) -> Self {
        let resolved = type_table.get(ty);
        Self {
            returns_value: !matches!(resolved, ResolvedType::Unit | ResolvedType::Never),
            carries_storage: may_carry_storage(ty, type_table),
            is_array: is_array(ty, type_table),
            is_mut_ref: matches!(resolved, ResolvedType::MutRef(_)),
            is_never: matches!(resolved, ResolvedType::Never),
        }
    }
}

fn is_array(ty: TypeId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(type_table.peel_refs(ty)),
        ResolvedType::BuiltinArray(_)
    )
}

/// A malformed attribute: which one, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    /// The attribute the fault points at.
    pub attr: &'static str,
    /// Which of the attributes so named, counting from 0: a second one is
    /// the fault, and every other fault is in the first, the one read.
    pub occurrence: usize,
    /// Why, as the diagnostic words it.
    pub message: String,
}

/// The facts `attrs` state, `None` where neither attribute is written. Every
/// fault is reported, and a declaration with one has no facts.
pub fn read(
    attrs: &[Attribute],
    params: &[ParamShape<'_>],
    ret: ReturnShape,
) -> Result<Option<BuiltinFacts<String>>, Vec<Fault>> {
    let mut faults = Vec::new();
    let storage = single(attrs, STORAGE, &mut faults);
    let side_effect = single(attrs, SIDE_EFFECT, &mut faults);
    let facts = match (storage, side_effect) {
        (None, None) => return Ok(None),
        (Some(_), None) => {
            faults.push(fault(STORAGE, "`#[storage]` goes with `#[side_effect]`"));
            None
        }
        (None, Some(_)) => {
            faults.push(fault(
                SIDE_EFFECT,
                "`#[side_effect]` goes with `#[storage]`",
            ));
            None
        }
        (Some(storage), Some(side_effect)) => {
            let reader = Reader {
                params,
                ret,
                faults: &mut faults,
            };
            reader.facts(storage, side_effect)
        }
    };
    match facts {
        Some(facts) if faults.is_empty() => Ok(Some(facts)),
        _ => Err(faults),
    }
}

fn fault(attr: &'static str, message: impl Into<String>) -> Fault {
    Fault {
        attr,
        occurrence: 0,
        message: message.into(),
    }
}

/// The one attribute named `name`, reporting a second.
fn single<'a>(
    attrs: &'a [Attribute],
    name: &'static str,
    faults: &mut Vec<Fault>,
) -> Option<&'a [AttrArg]> {
    let mut written = attrs.iter().filter(|attr| attr.name == name);
    let first = written.next()?;
    if written.next().is_some() {
        faults.push(Fault {
            occurrence: 1,
            ..fault(name, format!("`#[{name}]` is written once"))
        });
    }
    Some(&first.args)
}

struct Reader<'a, 'p> {
    params: &'a [ParamShape<'p>],
    ret: ReturnShape,
    faults: &'a mut Vec<Fault>,
}

/// The `#[side_effect]` keys, in the order a diagnostic lists them.
const SIDE_EFFECT_KEYS: [&str; 5] = ["outside", "at", "count", "unset", "negative"];

/// The `#[side_effect]` identifiers that stand alone, `suspend` aside.
const ALONE: [&str; 3] = ["none", "opaque", "black_box"];

/// The `#[side_effect]` identifiers of a call that returns without suspending.
const NEVER_SUSPENDS: [&str; 3] = ["none", "black_box", "hint"];

impl Reader<'_, '_> {
    fn report(&mut self, attr: &'static str, message: impl Into<String>) {
        self.faults.push(fault(attr, message));
    }

    fn param(&self, name: &str) -> Option<&ParamShape<'_>> {
        self.params.iter().find(|p| p.name == name)
    }

    /// `name` as the parameter a key names, reported where it is none or where
    /// `fits` refuses its type.
    fn named(
        &mut self,
        attr: &'static str,
        key: &str,
        name: &str,
        wants: &str,
        fits: impl Fn(&ParamShape<'_>) -> bool,
    ) -> String {
        match self.param(name) {
            None => self.report(
                attr,
                format!("`#[{attr}({key} = {name})]` names no parameter"),
            ),
            Some(param) if !fits(param) => self.report(
                attr,
                format!("`#[{attr}({key} = {name})]` names a parameter that is not {wants}"),
            ),
            Some(_) => {}
        }
        name.to_string()
    }

    fn facts(
        mut self,
        storage: &[AttrArg],
        side_effect: &[AttrArg],
    ) -> Option<BuiltinFacts<String>> {
        let storage = self.storage(storage);
        let side_effect = self.side_effect(side_effect);
        let ((storage, len), (side_effect, suspend)) = storage.zip(side_effect)?;
        Some(BuiltinFacts {
            storage,
            len,
            side_effect,
            suspend,
        })
    }

    fn storage(&mut self, args: &[AttrArg]) -> Option<(Storage, Option<String>)> {
        let Some((AttrArg::Ident(word), keys)) = args.split_first() else {
            self.report(STORAGE, "`#[storage]` takes one value first: `none`, `fresh`, `part_of_args`, `holds_args`, `copies_args`, `stores_args` or `opaque`");
            return None;
        };
        let Some(storage) = Storage::parse(word) else {
            self.report(STORAGE, format!("`#[storage({word})]` is no storage value"));
            return None;
        };
        let mut len = None;
        for arg in keys {
            match arg {
                AttrArg::KeyIdent(key, name) if key == "len" => {
                    if len.is_some() {
                        self.report(STORAGE, "`#[storage]` names `len` twice");
                    }
                    len = Some(self.named(STORAGE, "len", name, "an integer", |p| p.is_integer));
                }
                _ => self.report(
                    STORAGE,
                    format!(
                        "`#[storage]` takes `len = p` after its value, not `{}`",
                        arg.name()
                    ),
                ),
            }
        }
        if len.is_some() {
            if !storage.returns_new_storage() {
                self.report(
                    STORAGE,
                    format!("`len` goes with `fresh`, `holds_args` or `copies_args`, not `{word}`"),
                );
            }
            if !self.ret.is_array {
                self.report(
                    STORAGE,
                    "`len` names the length of a returned array, and this returns none",
                );
            }
        }
        if storage.describes_result() && !self.ret.returns_value {
            self.report(
                STORAGE,
                format!("`#[storage({word})]` describes a result, and this returns no value"),
            );
        }
        if storage == Storage::None && self.ret.carries_storage {
            self.report(
                STORAGE,
                "`#[storage(none)]` shares no storage, and this result can carry some: state where it comes from",
            );
        }
        // `stores_args` stores into its `&mut` parameter, which is no source.
        let sources = self
            .params
            .iter()
            .filter(|p| p.carries_storage && !(storage == Storage::StoresArgs && p.is_mut_ref));
        if storage.shares_args() && sources.count() == 0 {
            self.report(
                STORAGE,
                format!("`#[storage({word})]` relates the call to its arguments' storage, and none can carry any"),
            );
        }
        if storage == Storage::StoresArgs
            && self.params.iter().filter(|p| p.is_mut_ref).count() != 1
        {
            self.report(
                STORAGE,
                "`stores_args` stores into the one `&mut` parameter, and there is not exactly one",
            );
        }
        Some((storage, len))
    }

    /// The `#[side_effect]` facts, and whether the call may suspend.
    fn side_effect(&mut self, args: &[AttrArg]) -> Option<(SideEffect<String>, bool)> {
        if args.is_empty() {
            self.report(
                SIDE_EFFECT,
                "`#[side_effect]` lists at least one effect, or `none`",
            );
            return None;
        }
        let mut words: IndexSet<&str> = IndexSet::default();
        let mut arrays: Vec<(&str, &[String])> = Vec::new();
        let mut names: Vec<(&str, &str)> = Vec::new();
        for arg in args {
            let key = arg.name();
            let seen = words.contains(key)
                || arrays.iter().any(|(k, _)| *k == key)
                || names.iter().any(|(k, _)| *k == key);
            if seen {
                self.report(SIDE_EFFECT, format!("`#[side_effect]` names `{key}` twice"));
                continue;
            }
            match arg {
                AttrArg::Ident(word)
                    if matches!(
                        word.as_str(),
                        "none"
                            | "trap"
                            | "read"
                            | "write"
                            | "opaque"
                            | "hint"
                            | "black_box"
                            | "suspend"
                    ) =>
                {
                    words.insert(word);
                }
                // `[]` holds no item to say it is a name, so it parses as strings.
                AttrArg::KeyArray(key, items)
                    if items.is_empty() && matches!(key.as_str(), "outside" | "at") =>
                {
                    self.report(
                        SIDE_EFFECT,
                        format!("`#[side_effect({key} = [])]` names no parameter"),
                    );
                }
                AttrArg::KeyIdentArray(key, items) if matches!(key.as_str(), "outside" | "at") => {
                    assert!(!items.is_empty(), "the parser reads `[]` as strings");
                    arrays.push((key, items));
                }
                AttrArg::KeyIdent(key, name)
                    if matches!(key.as_str(), "count" | "unset" | "negative") =>
                {
                    names.push((key, name));
                }
                _ if SIDE_EFFECT_KEYS.contains(&key) => {
                    let form = if matches!(key, "outside" | "at") {
                        "`[p, …]`"
                    } else {
                        "one parameter"
                    };
                    self.report(
                        SIDE_EFFECT,
                        format!("`#[side_effect({key} = …)]` takes {form}"),
                    );
                }
                _ => self.report(SIDE_EFFECT, format!("`#[side_effect]` takes no `{key}`")),
            }
        }

        let suspend = words.shift_remove("suspend");
        if let Some(alone) = ALONE.iter().find(|word| words.contains(*word))
            && words.len() + arrays.len() + names.len() > 1
        {
            self.report(
                SIDE_EFFECT,
                format!("`#[side_effect({alone})]` stands alone"),
            );
            return None;
        }
        if suspend && let Some(word) = NEVER_SUSPENDS.iter().find(|w| words.contains(*w)) {
            self.report(
                SIDE_EFFECT,
                format!("`#[side_effect({word})]` returns without suspending, so it goes without `suspend`"),
            );
            return None;
        }
        if words.contains("opaque") {
            return Some((SideEffect::Opaque, suspend));
        }
        if words.contains("black_box") {
            return Some((SideEffect::BlackBox, suspend));
        }

        let array = |key: &str| arrays.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
        let name = |key: &str| names.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
        let has_conditions = !arrays.is_empty() || !names.is_empty();
        if has_conditions && !words.contains("trap") {
            self.report(SIDE_EFFECT, "a trap condition goes with `trap`");
        }

        let outside = array("outside").unwrap_or(&[]);
        let at = array("at");
        if outside.is_empty() && (at.is_some() || name("count").is_some()) {
            self.report(SIDE_EFFECT, "`at` and `count` go with `outside`");
        }
        if let Some(at) = at
            && at.len() != outside.len()
        {
            self.report(
                SIDE_EFFECT,
                "`outside` and `at` pair up, so they have the same length",
            );
        }
        let count = name("count")
            .map(|p| self.named(SIDE_EFFECT, "count", p, "an integer", |p| p.is_integer));
        let mut checks = Vec::new();
        for (index, a) in outside.iter().enumerate() {
            checks.push(TrapCheck::Outside {
                array: self.named(SIDE_EFFECT, "outside", a, "an array", |p| p.is_array),
                at: at
                    .and_then(|at| at.get(index))
                    .map(|i| self.named(SIDE_EFFECT, "at", i, "an integer", |p| p.is_integer)),
                count: count.clone(),
            });
        }
        if let Some(a) = name("unset") {
            if !outside.iter().any(|o| o == a) {
                self.report(
                    SIDE_EFFECT,
                    format!("`unset = {a}` names an array `outside` does not"),
                );
            }
            checks.push(TrapCheck::Unset(self.named(
                SIDE_EFFECT,
                "unset",
                a,
                "an array",
                |p| p.is_array,
            )));
        }
        if let Some(p) = name("negative") {
            checks.push(TrapCheck::Negative(self.named(
                SIDE_EFFECT,
                "negative",
                p,
                "an integer",
                |p| p.is_integer,
            )));
        }

        let trap = if !words.contains("trap") {
            Trap::Never
        } else if checks.is_empty() {
            Trap::Anywhere
        } else {
            Trap::Only(checks)
        };
        let listed = SideEffect::Listed {
            trap,
            read: words.contains("read"),
            write: words.contains("write"),
            hint: words.contains("hint"),
        };
        Some((listed, suspend))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::Span;

    fn attr(name: &str, args: Vec<AttrArg>) -> Attribute {
        Attribute {
            name: name.to_string(),
            args,
            span: Span::default(),
            cm_boundary: None,
        }
    }

    fn ident(word: &str) -> AttrArg {
        AttrArg::Ident(word.to_string())
    }

    fn key(key: &str, name: &str) -> AttrArg {
        AttrArg::KeyIdent(key.to_string(), name.to_string())
    }

    fn keys(key: &str, names: &[&str]) -> AttrArg {
        AttrArg::KeyIdentArray(
            key.to_string(),
            names.iter().map(|n| (*n).to_string()).collect(),
        )
    }

    const INT: ParamShape<'static> = ParamShape {
        name: "",
        carries_storage: false,
        is_reference: false,
        is_mut_ref: false,
        is_array: false,
        is_integer: true,
    };
    const ARRAY: ParamShape<'static> = ParamShape {
        name: "",
        carries_storage: true,
        is_reference: true,
        is_mut_ref: false,
        is_array: true,
        is_integer: false,
    };

    fn named(name: &'static str, shape: ParamShape<'static>) -> ParamShape<'static> {
        ParamShape { name, ..shape }
    }

    const UNIT: ReturnShape = ReturnShape {
        returns_value: false,
        carries_storage: false,
        is_array: false,
        is_mut_ref: false,
        is_never: false,
    };
    const STRUCT: ReturnShape = ReturnShape {
        returns_value: true,
        carries_storage: true,
        ..UNIT
    };

    /// `array_copy`'s declaration.
    fn copy_params() -> Vec<ParamShape<'static>> {
        vec![
            named(
                "dst",
                ParamShape {
                    is_mut_ref: true,
                    ..ARRAY
                },
            ),
            named("dst_offset", INT),
            named("src", ARRAY),
            named("src_offset", INT),
            named("len", INT),
        ]
    }

    fn messages(result: Result<Option<BuiltinFacts<String>>, Vec<Fault>>) -> Vec<String> {
        result
            .expect_err("the attributes are malformed")
            .into_iter()
            .map(|f| f.message)
            .collect()
    }

    #[test]
    fn array_copy_states_two_ranges() {
        let attrs = [
            attr(STORAGE, vec![ident("stores_args")]),
            attr(
                SIDE_EFFECT,
                vec![
                    ident("trap"),
                    keys("outside", &["dst", "src"]),
                    keys("at", &["dst_offset", "src_offset"]),
                    key("count", "len"),
                ],
            ),
        ];
        let facts = read(&attrs, &copy_params(), UNIT).unwrap().unwrap();
        assert_eq!(facts.storage, Storage::StoresArgs);
        assert_eq!(
            facts.trap_checks(),
            [
                TrapCheck::Outside {
                    array: "dst".to_string(),
                    at: Some("dst_offset".to_string()),
                    count: Some("len".to_string()),
                },
                TrapCheck::Outside {
                    array: "src".to_string(),
                    at: Some("src_offset".to_string()),
                    count: Some("len".to_string()),
                },
            ]
        );
    }

    #[test]
    fn suspend_goes_with_opaque_or_a_listing() {
        for (args, written) in [
            (
                vec![ident("opaque"), ident("suspend")],
                "#[side_effect(opaque, suspend)]",
            ),
            (
                vec![ident("suspend"), ident("write")],
                "#[side_effect(write, suspend)]",
            ),
            (vec![ident("suspend")], "#[side_effect(suspend)]"),
        ] {
            let attrs = [attr(STORAGE, vec![ident("none")]), attr(SIDE_EFFECT, args)];
            let facts = read(&attrs, &[], UNIT).unwrap().unwrap();
            assert!(facts.suspend);
            assert_eq!(facts.written()[1], written);
        }
        let attrs = [
            attr(STORAGE, vec![ident("none")]),
            attr(SIDE_EFFECT, vec![ident("opaque")]),
        ];
        assert!(!read(&attrs, &[], UNIT).unwrap().unwrap().suspend);
    }

    #[test]
    fn neither_attribute_states_nothing() {
        assert_eq!(read(&[], &copy_params(), UNIT), Ok(None));
    }

    #[test]
    fn a_fault_in_the_form_is_reported() {
        let cases: Vec<(Vec<AttrArg>, &str)> = vec![
            (
                vec![keys("outside", &["dst"])],
                "a trap condition goes with `trap`",
            ),
            (
                vec![ident("trap"), key("count", "len")],
                "`at` and `count` go with `outside`",
            ),
            (
                vec![
                    ident("trap"),
                    keys("outside", &["dst"]),
                    keys("at", &["dst_offset", "src_offset"]),
                ],
                "`outside` and `at` pair up, so they have the same length",
            ),
            (
                vec![ident("none"), ident("trap")],
                "`#[side_effect(none)]` stands alone",
            ),
            (
                vec![ident("trap"), ident("trap")],
                "`#[side_effect]` names `trap` twice",
            ),
            (
                vec![ident("none"), ident("none")],
                "`#[side_effect]` names `none` twice",
            ),
            (
                vec![ident("trap"), key("outside", "dst")],
                "`#[side_effect(outside = …)]` takes `[p, …]`",
            ),
            (
                vec![
                    ident("trap"),
                    keys("outside", &["dst"]),
                    key("unset", "src"),
                ],
                "`unset = src` names an array `outside` does not",
            ),
            (
                vec![ident("trap"), keys("outside", &["len"])],
                "`#[side_effect(outside = len)]` names a parameter that is not an array",
            ),
            (
                vec![ident("trap"), key("negative", "x")],
                "`#[side_effect(negative = x)]` names no parameter",
            ),
            (vec![ident("pure")], "`#[side_effect]` takes no `pure`"),
            (
                vec![ident("none"), ident("suspend")],
                "`#[side_effect(none)]` returns without suspending, so it goes without `suspend`",
            ),
            (
                vec![ident("hint"), ident("suspend")],
                "`#[side_effect(hint)]` returns without suspending, so it goes without `suspend`",
            ),
            (
                vec![ident("opaque"), ident("trap"), ident("suspend")],
                "`#[side_effect(opaque)]` stands alone",
            ),
            (
                vec![
                    ident("trap"),
                    AttrArg::KeyArray("outside".to_string(), Vec::new()),
                ],
                "`#[side_effect(outside = [])]` names no parameter",
            ),
        ];
        for (args, expected) in cases {
            let attrs = [
                attr(STORAGE, vec![ident("stores_args")]),
                attr(SIDE_EFFECT, args),
            ];
            assert_eq!(messages(read(&attrs, &copy_params(), UNIT)), [expected]);
        }
    }

    #[test]
    fn both_attributes_report_their_faults() {
        let attrs = [
            attr(STORAGE, vec![ident("shared")]),
            attr(SIDE_EFFECT, vec![ident("trap"), keys("outside", &["len"])]),
        ];
        assert_eq!(
            messages(read(&attrs, &copy_params(), UNIT)),
            [
                "`#[storage(shared)]` is no storage value",
                "`#[side_effect(outside = len)]` names a parameter that is not an array",
            ]
        );
    }

    #[test]
    fn storage_must_fit_the_signature() {
        let side_effect = attr(SIDE_EFFECT, vec![ident("none")]);
        let ints = [named("a", INT), named("b", INT)];
        let only_dst = [
            named(
                "dst",
                ParamShape {
                    is_mut_ref: true,
                    ..ARRAY
                },
            ),
            named("i", INT),
        ];
        let cases: Vec<(Vec<AttrArg>, &[ParamShape<'static>], ReturnShape, &str)> = vec![
            (
                vec![ident("fresh")],
                &ints,
                UNIT,
                "`#[storage(fresh)]` describes a result, and this returns no value",
            ),
            (
                vec![ident("stores_args")],
                &ints,
                UNIT,
                "`#[storage(stores_args)]` relates the call to its arguments' storage, and none can carry any",
            ),
            (
                vec![ident("stores_args")],
                &only_dst,
                UNIT,
                "`#[storage(stores_args)]` relates the call to its arguments' storage, and none can carry any",
            ),
            (
                vec![ident("none")],
                &ints,
                STRUCT,
                "`#[storage(none)]` shares no storage, and this result can carry some: state where it comes from",
            ),
            (
                vec![ident("none"), key("len", "a")],
                &ints,
                UNIT,
                "`len` goes with `fresh`, `holds_args` or `copies_args`, not `none`",
            ),
            (
                vec![ident("shared")],
                &ints,
                UNIT,
                "`#[storage(shared)]` is no storage value",
            ),
        ];
        for (args, params, ret, expected) in cases {
            let attrs = [attr(STORAGE, args), side_effect.clone()];
            let faults = messages(read(&attrs, params, ret));
            assert_eq!(
                faults.first().map(String::as_str),
                Some(expected),
                "{faults:?}"
            );
        }
    }

    #[test]
    fn one_attribute_without_the_other_is_a_fault() {
        let attrs = [attr(STORAGE, vec![ident("none")])];
        assert_eq!(
            messages(read(&attrs, &copy_params(), UNIT)),
            ["`#[storage]` goes with `#[side_effect]`"]
        );
    }

    #[test]
    fn a_second_attribute_is_the_one_at_fault() {
        let attrs = [
            attr(STORAGE, vec![ident("none")]),
            attr(SIDE_EFFECT, vec![ident("none")]),
            attr(STORAGE, vec![ident("fresh")]),
        ];
        let faults = read(&attrs, &[], UNIT).unwrap_err();
        assert_eq!(
            faults
                .iter()
                .map(|f| (f.attr, f.occurrence))
                .collect::<Vec<_>>(),
            [(STORAGE, 1)]
        );
    }
}
