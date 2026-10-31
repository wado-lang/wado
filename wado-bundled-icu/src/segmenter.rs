//! Grapheme cluster segmentation. See `wit-properties/world.wit`.

use alloc::string::String;
use alloc::vec::Vec;

use icu::segmenter::GraphemeClusterSegmenter;

use crate::properties::Component;
use crate::properties::exports::wado::icu::segmenter::Guest;

/// Every boundary but the one at the start, which ends no cluster.
fn ends(text: &str) -> impl Iterator<Item = usize> {
    GraphemeClusterSegmenter::new().segment_str(text).skip(1)
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).expect("a Wado string is under 4 GiB")
}

impl Guest for Component {
    fn grapheme_ends(text: String) -> Vec<u32> {
        ends(&text).map(to_u32).collect()
    }

    fn grapheme_count(text: String) -> u32 {
        to_u32(ends(&text).count())
    }
}
