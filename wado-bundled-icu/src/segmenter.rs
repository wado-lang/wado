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

impl Guest for Component {
    fn grapheme_ends(text: String) -> Vec<u32> {
        ends(&text).map(|i| u32::try_from(i).expect("a Wado string is under 4 GiB")).collect()
    }

    fn grapheme_count(text: String) -> u32 {
        u32::try_from(ends(&text).count()).expect("a Wado string is under 4 GiB")
    }
}
