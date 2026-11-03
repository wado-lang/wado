//! Rarity is two questions, as LLVM and GCC ask them: whether the branch to a
//! path is rarely taken, and whether the path rarely runs in the whole
//! program. A marker states one answer or both, and each pass reads the half it
//! needs. See
//! [WEP: Profile-Guided Optimization](../../docs/wep-2026-10-25-profile-guided-optimization.md).

use crate::name::{
    COLD_CODE_INTRINSIC, COLD_PATH_INTRINSIC, HOT_CODE_INTRINSIC, KNOWN_COLD_PATH_INTRINSIC,
    UNLIKELY_PATH_INTRINSIC,
};

/// The answer about a path a pass reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rarity {
    /// The path runs rarely in the whole program: what pricing, inlining and
    /// outlining read.
    Cold,
    /// The branch to the path is rarely taken: what a branch hint reads.
    Unlikely,
    /// The path runs often in the whole program: what the inliner's hot price
    /// reads.
    Hot,
}

/// What a marker states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mark {
    /// Unlikely and cold: `cold_path`.
    ColdPath,
    /// Unlikely, not cold: `unlikely_path`.
    Unlikely,
    /// Cold, not unlikely: `cold_code`.
    Cold,
    /// Hot: `hot_code`.
    Hot,
}

impl Mark {
    pub const ALL: [Self; 4] = [Self::ColdPath, Self::Unlikely, Self::Cold, Self::Hot];

    /// The mark stating these answers, where either is yes.
    #[must_use]
    pub fn of(unlikely: bool, cold: bool) -> Option<Self> {
        match (unlikely, cold) {
            (true, true) => Some(Self::ColdPath),
            (true, false) => Some(Self::Unlikely),
            (false, true) => Some(Self::Cold),
            (false, false) => None,
        }
    }

    /// The mark the builtin `name` states, where it is a marker.
    #[must_use]
    pub fn of_builtin(name: &str) -> Option<Self> {
        match name {
            COLD_PATH_INTRINSIC | KNOWN_COLD_PATH_INTRINSIC => Some(Self::ColdPath),
            UNLIKELY_PATH_INTRINSIC => Some(Self::Unlikely),
            COLD_CODE_INTRINSIC => Some(Self::Cold),
            HOT_CODE_INTRINSIC => Some(Self::Hot),
            _ => None,
        }
    }

    /// The builtin stating this mark, as an author or a profile writes it.
    #[must_use]
    pub fn builtin(self) -> &'static str {
        match self {
            Self::ColdPath => COLD_PATH_INTRINSIC,
            Self::Unlikely => UNLIKELY_PATH_INTRINSIC,
            Self::Cold => COLD_CODE_INTRINSIC,
            Self::Hot => HOT_CODE_INTRINSIC,
        }
    }

    #[must_use]
    pub fn states(self, rarity: Rarity) -> bool {
        match (self, rarity) {
            (Self::ColdPath, Rarity::Cold | Rarity::Unlikely)
            | (Self::Unlikely, Rarity::Unlikely)
            | (Self::Cold, Rarity::Cold)
            | (Self::Hot, Rarity::Hot) => true,
            (Self::ColdPath | Self::Unlikely | Self::Cold, Rarity::Hot)
            | (Self::Unlikely, Rarity::Cold)
            | (Self::Cold, Rarity::Unlikely)
            | (Self::Hot, Rarity::Cold | Rarity::Unlikely) => false,
        }
    }

    /// The answers, as a marked region's listing spells them.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ColdPath => "unlikely cold",
            Self::Unlikely => "unlikely",
            Self::Cold => "cold",
            Self::Hot => "hot",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mark_states_exactly_the_answers_it_is_made_of() {
        for unlikely in [false, true] {
            for cold in [false, true] {
                let states = |rarity| Mark::of(unlikely, cold).is_some_and(|m| m.states(rarity));
                assert_eq!(states(Rarity::Unlikely), unlikely);
                assert_eq!(states(Rarity::Cold), cold);
            }
        }
    }

    #[test]
    fn each_mark_is_named_by_its_builtin() {
        for mark in Mark::ALL {
            assert_eq!(Mark::of_builtin(mark.builtin()), Some(mark));
        }
        assert_eq!(
            Mark::of_builtin(KNOWN_COLD_PATH_INTRINSIC),
            Some(Mark::ColdPath)
        );
        assert_eq!(Mark::of_builtin("unreachable"), None);
    }
}
