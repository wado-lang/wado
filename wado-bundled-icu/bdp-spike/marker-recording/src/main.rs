//! Prototype: record the markers an ICU op's constructor actually requests.

use icu_provider::buf::BufferMarker;
use icu_provider::{DataError, DataMarkerInfo, DataRequest, DataResponse, DynamicDataProvider};
use icu_provider_blob::BlobDataProvider;
use std::cell::RefCell;
use std::collections::BTreeSet;

/// Wraps a BufferProvider and records every marker requested through it.
struct Recorder<'a> {
    inner: &'a BlobDataProvider,
    seen: RefCell<BTreeSet<String>>,
}

impl DynamicDataProvider<BufferMarker> for Recorder<'_> {
    fn load_data(
        &self,
        marker: DataMarkerInfo,
        req: DataRequest,
    ) -> Result<DataResponse<BufferMarker>, DataError> {
        self.seen.borrow_mut().insert(format!("{marker:?}"));
        self.inner.load_data(marker, req)
    }
}

fn main() {
    let blob = std::fs::read("../shared.blob").expect("../shared.blob (run from crate dir)");
    let provider = BlobDataProvider::try_new_from_blob(blob.into_boxed_slice()).unwrap();
    let rec = Recorder {
        inner: &provider,
        seen: RefCell::new(BTreeSet::new()),
    };

    use icu_collator::options::CollatorOptions;
    use icu_collator::{Collator, CollatorPreferences};
    use icu_locale_core::Locale;
    let loc: Locale = "und".parse().unwrap();
    let prefs = CollatorPreferences::from(&loc);
    Collator::try_new_with_buffer_provider(&rec, prefs, CollatorOptions::default()).unwrap();

    let markers: Vec<String> = rec.seen.borrow().iter().cloned().collect();
    println!("collator.compare requested {} markers:", markers.len());
    for m in &markers {
        println!("  {m}");
    }
    let has_coll = markers.iter().any(|m| m.contains("Collation"));
    let has_nfd = markers.iter().any(|m| m.contains("Nfd"));
    assert!(has_coll, "expected collation markers");
    assert!(
        has_nfd,
        "expected TRANSITIVE NFD normalizer markers (not in icu_collator::provider::MARKERS)"
    );
    println!("\nOK — recorder auto-captured the transitive collator->NFD dependency.");
}
