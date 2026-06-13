//! Generate a sliced ICU4X postcard *blob* holding only the casemap markers.
//!
//! This is the "shared data" artifact in the BlobDataProvider (BDP) separation
//! model: instead of every feature component baking CLDR data via
//! `compiled_data`, the data lives once in a blob that a data-free feature
//! component loads at runtime through `BlobDataProvider`.
//!
//! Usage: `cargo run --release -- <out.blob>`

use anyhow::{Context, Result};
use icu_provider_export::blob_exporter::BlobExporter;
use icu_provider_export::prelude::*;
use icu_provider_source::SourceDataProvider;
use std::fs::File;

fn main() -> Result<()> {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "casemap.blob".to_string());

    // Fetches the CLDR/Unicode data icu4x 2.2 was tested against (networking).
    let provider = SourceDataProvider::new();

    let driver = ExportDriver::new(
        // casemap is locale-agnostic in its data (the case mappings are global),
        // so FULL here is effectively just the root payload. Kept explicit to
        // mirror how a locale-bearing component (collator, datetime) would slice.
        [DataLocaleFamily::FULL],
        DeduplicationStrategy::None.into(),
        LocaleFallbacker::try_new_unstable(&provider)
            .context("construct fallbacker from source")?,
    )
    .with_markers(icu_casemap::provider::MARKERS.iter().copied());

    let sink = File::create(&out).with_context(|| format!("create {out}"))?;
    driver
        .export(&provider, BlobExporter::new_with_sink(Box::new(sink)))
        .context("export casemap blob")?;

    let bytes = std::fs::metadata(&out)?.len();
    println!("wrote {out} ({bytes} bytes) — markers: casemap (CaseMapV1, CaseMapUnfoldV1)");
    Ok(())
}
