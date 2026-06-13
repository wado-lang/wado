//! Generate sliced ICU4X postcard *blobs* for the BDP separation experiment.
//!
//! Usage: `cargo run --release -- <set> <out.blob>`
//!
//! Marker sets:
//!   casemap   casemap markers
//!   norm      all normalizer markers (NFC/NFD/NFKC/NFKD/UTS46)
//!   coll      collator markers + the NFD normalizer markers collator needs
//!             (root collation only) — i.e. exactly what a collator feature loads
//!   shared    collator markers + ALL normalizer markers (root collation) —
//!             one blob serving both a collator feature and a normalizer feature
//!
//! Comparing sizes of `coll`, `norm` and `shared` quantifies the marker dedup:
//! `size(coll) + size(norm) - size(shared)` is the normalization data that
//! compiled_data would duplicate across the two components but the shared blob
//! stores once.

use anyhow::{Context, Result};
use icu_normalizer::provider::{NormalizerNfdDataV1, NormalizerNfdTablesV1};
use icu_properties::provider::{
    PropertyBinaryAlphabeticV1, PropertyBinaryEmojiV1, PropertyBinaryLowercaseV1,
    PropertyBinaryUppercaseV1, PropertyBinaryWhiteSpaceV1, PropertyEnumGeneralCategoryV1,
    PropertyEnumScriptV1, PropertyNameShortScriptV1,
};
use icu_provider::DataMarker;
use icu_provider_export::blob_exporter::BlobExporter;
use icu_provider_export::prelude::*;
use icu_provider_source::SourceDataProvider;
use std::fs::File;

// The property markers the `properties` feature uses (General_Category, Script
// + short names, and the binaries Alphabetic/White_Space/Uppercase/Lowercase/
// Emoji).
fn properties_markers() -> Vec<DataMarkerInfo> {
    vec![
        PropertyEnumGeneralCategoryV1::INFO,
        PropertyEnumScriptV1::INFO,
        PropertyNameShortScriptV1::INFO,
        PropertyBinaryAlphabeticV1::INFO,
        PropertyBinaryWhiteSpaceV1::INFO,
        PropertyBinaryUppercaseV1::INFO,
        PropertyBinaryLowercaseV1::INFO,
        PropertyBinaryEmojiV1::INFO,
    ]
}

fn main() -> Result<()> {
    let set = std::env::args().nth(1).unwrap_or_else(|| "casemap".into());
    let out = std::env::args().nth(2).unwrap_or_else(|| format!("{set}.blob"));

    let provider = SourceDataProvider::new();

    // Collation data is locale-bearing; restrict it to root ("und") so the blob
    // stays focused on the normalization-dedup story. Normalizer/casemap data is
    // locale-agnostic, so FULL there is just the root payload.
    let (markers, family): (Vec<DataMarkerInfo>, DataLocaleFamily) = match set.as_str() {
        "casemap" => (icu_casemap::provider::MARKERS.to_vec(), DataLocaleFamily::FULL),
        "norm" => (icu_normalizer::provider::MARKERS.to_vec(), DataLocaleFamily::FULL),
        "coll" => {
            let mut m = icu_collator::provider::MARKERS.to_vec();
            m.push(NormalizerNfdDataV1::INFO);
            m.push(NormalizerNfdTablesV1::INFO);
            (m, DataLocaleFamily::single("und".parse().unwrap()))
        }
        "shared" => {
            let mut m = icu_collator::provider::MARKERS.to_vec();
            m.extend_from_slice(icu_normalizer::provider::MARKERS);
            (m, DataLocaleFamily::single("und".parse().unwrap()))
        }
        "properties" => (properties_markers(), DataLocaleFamily::FULL),
        "segmenter" => (icu_segmenter::provider::MARKERS.to_vec(), DataLocaleFamily::FULL),
        // Union of the three independent string features, to measure whether
        // they share any markers (dedup = sum of singles - this union).
        "csp" => {
            let mut m = icu_casemap::provider::MARKERS.to_vec();
            m.extend(properties_markers());
            m.extend_from_slice(icu_segmenter::provider::MARKERS);
            (m, DataLocaleFamily::FULL)
        }
        other => anyhow::bail!("unknown marker set: {other}"),
    };

    ExportDriver::new(
        [family],
        DeduplicationStrategy::None.into(),
        LocaleFallbacker::try_new_unstable(&provider).context("fallbacker")?,
    )
    .with_markers(markers.iter().copied())
    .export(
        &provider,
        BlobExporter::new_with_sink(Box::new(
            File::create(&out).with_context(|| format!("create {out}"))?,
        )),
    )
    .context("export blob")?;

    let bytes = std::fs::metadata(&out)?.len();
    println!("wrote {out} ({bytes} bytes) — set={set}, {} markers", markers.len());
    Ok(())
}
