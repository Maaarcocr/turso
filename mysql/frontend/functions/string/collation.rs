use std::{cmp::Ordering, sync::LazyLock};

use icu_collator::{
    options::{CollatorOptions, Strength},
    Collator, CollatorBorrowed,
};
use icu_locale::locale;

fn collator(japanese: bool, strength: Strength) -> CollatorBorrowed<'static> {
    let mut options = CollatorOptions::default();
    options.strength = Some(strength);
    let preferences = if japanese {
        locale!("ja").into()
    } else {
        Default::default()
    };
    Collator::try_new(preferences, options).expect("the collation data is compiled in")
}

static UTF8MB4_0900_AI_CI: LazyLock<CollatorBorrowed<'static>> =
    LazyLock::new(|| collator(false, Strength::Primary));
static UTF8MB4_0900_AS_CI: LazyLock<CollatorBorrowed<'static>> =
    LazyLock::new(|| collator(false, Strength::Secondary));
static UTF8MB4_0900_AS_CS: LazyLock<CollatorBorrowed<'static>> =
    LazyLock::new(|| collator(false, Strength::Tertiary));
static UTF8MB4_JA_0900_AS_CS: LazyLock<CollatorBorrowed<'static>> =
    LazyLock::new(|| collator(true, Strength::Tertiary));
static UTF8MB4_JA_0900_AS_CS_KS: LazyLock<CollatorBorrowed<'static>> =
    LazyLock::new(|| collator(true, Strength::Quaternary));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Collation {
    AccentInsensitive,
    CaseInsensitive,
    CaseSensitive,
    Japanese,
    JapaneseKanaSensitive,
    Binary,
    PadSpaceBinary,
}

impl Collation {
    pub(super) fn named(name: &str) -> Option<Self> {
        Some(match name {
            "utf8mb4_0900_ai_ci" => Collation::AccentInsensitive,
            "utf8mb4_0900_as_ci" => Collation::CaseInsensitive,
            "utf8mb4_0900_as_cs" => Collation::CaseSensitive,
            "utf8mb4_ja_0900_as_cs" => Collation::Japanese,
            "utf8mb4_ja_0900_as_cs_ks" => Collation::JapaneseKanaSensitive,
            "utf8mb4_0900_bin" | "binary" => Collation::Binary,
            "utf8mb4_bin" => Collation::PadSpaceBinary,
            _ => return None,
        })
    }

    pub(super) fn compare_bytes(self, left: &[u8], right: &[u8]) -> Ordering {
        let collator = match self {
            Collation::Binary => return left.cmp(right),
            Collation::PadSpaceBinary => {
                return trim_trailing_spaces(left).cmp(trim_trailing_spaces(right))
            }
            Collation::AccentInsensitive => &*UTF8MB4_0900_AI_CI,
            Collation::CaseInsensitive => &*UTF8MB4_0900_AS_CI,
            Collation::CaseSensitive => &*UTF8MB4_0900_AS_CS,
            Collation::Japanese => &*UTF8MB4_JA_0900_AS_CS,
            Collation::JapaneseKanaSensitive => &*UTF8MB4_JA_0900_AS_CS_KS,
        };
        collator.compare_utf8(left, right)
    }
}

fn trim_trailing_spaces(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .rposition(|byte| *byte != b' ')
        .map_or(0, |last| last + 1);
    &bytes[..end]
}

pub(super) fn compare_bytes(left: &[u8], right: &[u8], binary: bool) -> Ordering {
    if binary {
        left.cmp(right)
    } else {
        Collation::AccentInsensitive.compare_bytes(left, right)
    }
}
