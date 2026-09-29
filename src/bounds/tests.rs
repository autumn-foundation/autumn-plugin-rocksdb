use proptest::prelude::*;

use super::*;

fn range(
    prefix: Option<&[u8]>,
    start: Option<&[u8]>,
    end: Option<&[u8]>,
    after: Option<&[u8]>,
) -> Range {
    Range {
        prefix: prefix.map(<[u8]>::to_vec),
        start: start.map(<[u8]>::to_vec),
        end: end.map(<[u8]>::to_vec),
        after: after.map(<[u8]>::to_vec),
    }
}

#[test]
fn prefix_end_increments_the_last_byte() {
    assert_eq!(prefix_end(b"user:"), Some(b"user;".to_vec()));
    assert_eq!(prefix_end(&[1, 2, 3]), Some(vec![1, 2, 4]));
}

#[test]
fn prefix_end_drops_trailing_ff_bytes() {
    assert_eq!(prefix_end(&[1, 0xFF, 0xFF]), Some(vec![2]));
    assert_eq!(prefix_end(&[0, 0xFE, 0xFF]), Some(vec![0, 0xFF]));
}

#[test]
fn prefix_end_has_no_end_for_empty_and_ff_prefixes() {
    assert_eq!(prefix_end(b""), None);
    assert_eq!(prefix_end(&[0xFF, 0xFF]), None);
}

#[test]
fn no_conditions_give_all_keys() {
    let bounds = Bounds::new(&Range::default());
    assert_eq!(bounds.lower, b"");
    assert_eq!(bounds.upper, None);
    assert!(!bounds.is_empty());
    assert!(bounds.contains(b""));
    assert!(bounds.contains(&[0xFF, 0xFF]));
}

#[test]
fn a_prefix_gives_the_prefix_range() {
    let bounds = Bounds::new(&range(Some(b"b"), None, None, None));
    assert_eq!(bounds.lower, b"b");
    assert_eq!(bounds.upper, Some(b"c".to_vec()));
    assert!(bounds.contains(b"b"));
    assert!(bounds.contains(b"b\xFF"));
    assert!(!bounds.contains(b"a"));
    assert!(!bounds.contains(b"c"));
}

#[test]
fn start_is_inclusive_and_end_is_exclusive() {
    let bounds = Bounds::new(&range(None, Some(b"b"), Some(b"d"), None));
    assert!(!bounds.contains(b"a"));
    assert!(bounds.contains(b"b"));
    assert!(bounds.contains(b"c"));
    assert!(!bounds.contains(b"d"));
}

#[test]
fn the_cursor_is_exclusive() {
    let bounds = Bounds::new(&range(None, None, None, Some(b"b")));
    assert_eq!(bounds.lower, b"b\0");
    assert!(!bounds.contains(b"b"));
    assert!(bounds.contains(b"b\0"));
    assert!(bounds.contains(b"c"));
}

#[test]
fn the_largest_lower_and_the_smallest_upper_win() {
    let bounds = Bounds::new(&range(Some(b"k"), Some(b"k5"), Some(b"k8"), Some(b"k3")));
    assert_eq!(bounds.lower, b"k5");
    assert_eq!(bounds.upper, Some(b"k8".to_vec()));
    let bounds = Bounds::new(&range(Some(b"k"), Some(b"a"), Some(b"z"), Some(b"k7")));
    assert_eq!(bounds.lower, b"k7\0");
    assert_eq!(bounds.upper, Some(b"l".to_vec()));
}

#[test]
fn bounds_with_no_keys_are_empty() {
    assert!(Bounds::new(&range(None, Some(b"b"), Some(b"b"), None)).is_empty());
    assert!(Bounds::new(&range(None, Some(b"c"), Some(b"b"), None)).is_empty());
    assert!(Bounds::new(&range(Some(b"a"), Some(b"b"), None, None)).is_empty());
    assert!(!Bounds::new(&range(None, Some(b"b"), Some(b"b\0"), None)).is_empty());
}

fn small_key() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(
        prop_oneof![Just(0_u8), Just(1), Just(0xFE), Just(0xFF), any::<u8>()],
        0..5,
    )
}

proptest! {
    #[test]
    fn prefix_end_bounds_each_key_with_the_prefix(prefix in small_key(), tail in small_key()) {
        let mut key = prefix.clone();
        key.extend_from_slice(&tail);
        if let Some(end) = prefix_end(&prefix) {
            prop_assert!(key < end);
            prop_assert!(prefix < end);
        }
    }

    #[test]
    fn contains_matches_each_condition(
        prefix in proptest::option::of(small_key()),
        start in proptest::option::of(small_key()),
        end in proptest::option::of(small_key()),
        after in proptest::option::of(small_key()),
        key in small_key(),
    ) {
        let range = Range { prefix: prefix.clone(), start: start.clone(), end: end.clone(), after: after.clone() };
        let bounds = Bounds::new(&range);
        let expected = prefix.as_ref().is_none_or(|p| key.starts_with(p))
            && start.as_ref().is_none_or(|s| key >= *s)
            && end.as_ref().is_none_or(|e| key < *e)
            && after.as_ref().is_none_or(|a| key > *a);
        prop_assert_eq!(bounds.contains(&key), expected);
        if expected {
            prop_assert!(!bounds.is_empty());
        }
    }
}
