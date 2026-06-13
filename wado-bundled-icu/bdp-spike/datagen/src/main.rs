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
use icu_provider::DataMarker;
use icu_provider_export::blob_exporter::BlobExporter;
use icu_provider_export::prelude::*;
use icu_provider_source::SourceDataProvider;
use std::fs::File;

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
