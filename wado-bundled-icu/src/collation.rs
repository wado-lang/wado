//! Locale-aware string comparison. See `wit-properties/world.wit`.

use alloc::string::String;

use icu::collator::{Collator, CollatorBorrowed, CollatorPreferences, options::CollatorOptions};

use crate::locale::langid;
use crate::properties::Component;
use crate::properties::exports::wado::icu::collation::{
    Collator as CollatorHandle, Guest, GuestCollator, Ordering,
};

pub struct CollatorRes {
    inner: CollatorBorrowed<'static>,
}

impl GuestCollator for CollatorRes {
    fn create(language: u32, script: u32, region: u32) -> CollatorHandle {
        let prefs = CollatorPreferences::from(&langid(language, script, region));
        let inner = Collator::try_new(prefs, CollatorOptions::default())
            .expect("compiled data falls back to the root collation for every locale");
        CollatorHandle::new(CollatorRes { inner })
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
