//! Locale-aware string comparison. See `wit-properties/world.wit`.

use alloc::string::{String, ToString};

use icu::collator::{Collator, CollatorBorrowed, CollatorPreferences, options::CollatorOptions};
use icu::locale::Locale;

use crate::properties::Component;
use crate::properties::exports::wado::icu::collation::{
    Collator as CollatorHandle, Guest, GuestCollator, Ordering,
};

pub struct CollatorRes {
    inner: CollatorBorrowed<'static>,
}

impl GuestCollator for CollatorRes {
    fn create(locale: String) -> Result<CollatorHandle, String> {
        let locale = Locale::try_from_str(&locale).map_err(|e| e.to_string())?;
        let inner = Collator::try_new(CollatorPreferences::from(&locale), CollatorOptions::default())
            .map_err(|e| e.to_string())?;
        Ok(CollatorHandle::new(CollatorRes { inner }))
    }

    fn compare(&self, left: String, right: String) -> Ordering {
        match self.inner.compare(&left, &right) {
            core::cmp::Ordering::Less => Ordering::Less,
            core::cmp::Ordering::Equal => Ordering::Equal,
            core::cmp::Ordering::Greater => Ordering::Greater,
        }
    }
}

impl Guest for Component {
    type Collator = CollatorRes;
}
