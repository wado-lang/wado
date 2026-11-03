//! Unicode character properties, and the bindings of the shippable `core:icu`
//! world (`wit-properties/world.wit`) that `collation` implements too.
//!
//! The exports themselves are generated into `values`, one set per property, so
//! that what one names reaches that property's data alone. What is left here is
//! how each kind of property is asked.

use alloc::string::String;
use alloc::vec::Vec;
use core::ops::RangeInclusive;

wit_bindgen::generate!({
    world: "icu-properties",
    path: "wit-properties",
});

mod values;

use exports::wado::icu::properties::Matching;

use icu::properties::props::{
    BinaryProperty, EnumeratedProperty, GeneralCategory, GeneralCategoryGroup,
    ParseableEnumeratedProperty,
};
use icu::properties::{CodePointMapData, CodePointSetData, PropertyParser};

pub struct Component;

/// UAX #44 loose matching: case, `_`, `-` and spaces are not significant.
fn fold(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '_' && *c != '-' && *c != ' ')
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn ranges_of(iter: impl Iterator<Item = RangeInclusive<u32>>) -> Vec<(u32, u32)> {
    iter.map(|r| (*r.start(), *r.end())).collect()
}

fn binary_ranges<P: BinaryProperty>() -> Vec<(u32, u32)> {
    ranges_of(CodePointSetData::new::<P>().iter_ranges())
}

fn binary_contains<P: BinaryProperty>(ch: char) -> bool {
    CodePointSetData::new::<P>().contains(ch)
}

fn enumerated_ranges<T: EnumeratedProperty + PartialEq>(value: T) -> Vec<(u32, u32)> {
    ranges_of(CodePointMapData::<T>::new().iter_ranges_for_value(value))
}

fn enumerated_contains<T: EnumeratedProperty + PartialEq>(value: T, ch: char) -> bool {
    CodePointMapData::<T>::new().get(ch) == value
}

/// A General_Category value may name a group (`L`), which no single map value
/// is, so it is asked through the group rather than as an enumerated value.
fn category_ranges(group: GeneralCategoryGroup) -> Vec<(u32, u32)> {
    ranges_of(CodePointMapData::<GeneralCategory>::new().iter_ranges_for_group(group))
}

fn category_contains(group: GeneralCategoryGroup, ch: char) -> bool {
    group.contains(CodePointMapData::<GeneralCategory>::new().get(ch))
}

fn parse<T: ParseableEnumeratedProperty>(name: &str, matching: Matching) -> Option<T> {
    let parser = PropertyParser::<T>::new();
    match matching {
        Matching::Strict => parser.get_strict(name),
        Matching::Loose => parser.get_loose(name),
    }
}

/// The value at a position `core:icu` sent. Its list and this one are generated
/// together, so a position past the end is a broken build, and traps.
fn at<T: Copy>(table: &[T], index: u16) -> T {
    match table.get(usize::from(index)) {
        Some(value) => *value,
        None => trap(),
    }
}

/// The position of an ICU4X value in its list. `None` for a value the list
/// does not hold, which an alias of a listed value never is.
fn position<T: PartialEq>(table: &[T], value: T) -> Option<u16> {
    let index = table.iter().position(|v| *v == value)?;
    // The generator refuses a list longer than a `u16` counts.
    Some(index as u16)
}

fn trap() -> ! {
    core::arch::wasm32::unreachable()
}

export!(Component);
