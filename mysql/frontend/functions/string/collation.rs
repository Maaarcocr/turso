use std::{cmp::Ordering, sync::LazyLock};

use icu_collator::{
    options::{CollatorOptions, Strength},
    Collator, CollatorBorrowed,
};

use super::convert::Str;

static UTF8MB4_0900_AI_CI: LazyLock<CollatorBorrowed<'static>> = LazyLock::new(|| {
    let mut options = CollatorOptions::default();
    options.strength = Some(Strength::Primary);
    Collator::try_new(Default::default(), options).expect("the root collation is compiled in")
});

pub(super) fn compare(left: &Str, right: &Str) -> Ordering {
    compare_bytes(
        left.as_bytes(),
        right.as_bytes(),
        left.is_binary() || right.is_binary(),
    )
}

pub(super) fn compare_bytes(left: &[u8], right: &[u8], binary: bool) -> Ordering {
    if binary {
        left.cmp(right)
    } else {
        UTF8MB4_0900_AI_CI.compare_utf8(left, right)
    }
}
