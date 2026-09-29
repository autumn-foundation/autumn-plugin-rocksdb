use std::time::{Duration, SystemTime, UNIX_EPOCH};

use proptest::prelude::*;

use super::*;

#[test]
fn an_envelope_has_the_version_the_expiry_and_the_payload() {
    let bytes = encode(b"abc", Some(0x0102_0304_0506_0708));
    assert_eq!(bytes, [1, 1, 2, 3, 4, 5, 6, 7, 8, b'a', b'b', b'c']);
}

#[test]
fn no_expiry_is_stored_as_zero() {
    let bytes = encode(b"x", None);
    assert_eq!(&bytes[..HEADER], &[1, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(decode(&bytes).unwrap().expires_at, None);
}

#[test]
fn decode_gives_the_expiry_and_the_payload() {
    let bytes = encode(b"payload", Some(42));
    let entry = decode(&bytes).unwrap();
    assert_eq!(entry.expires_at, Some(42));
    assert_eq!(entry.payload, b"payload");
}

#[test]
fn an_empty_payload_decodes() {
    let bytes = encode(b"", Some(7));
    assert_eq!(decode(&bytes).unwrap().payload, b"");
}

#[test]
fn decode_refuses_short_bytes_and_unknown_versions() {
    assert_eq!(decode(b""), Err(BadEnvelope));
    assert_eq!(decode(&[1, 0, 0, 0, 0, 0, 0, 0]), Err(BadEnvelope));
    assert_eq!(decode(&[2, 0, 0, 0, 0, 0, 0, 0, 0]), Err(BadEnvelope));
}

#[test]
fn an_entry_expires_at_its_expiry() {
    let bytes = encode(b"", Some(100));
    let entry = decode(&bytes).unwrap();
    assert!(!entry.is_expired(99));
    assert!(entry.is_expired(100));
    assert!(entry.is_expired(101));
}

#[test]
fn an_entry_without_expiry_never_expires() {
    let bytes = encode(b"", None);
    let entry = decode(&bytes).unwrap();
    assert!(!entry.is_expired(u64::MAX));
}

#[test]
fn expiry_adds_the_ttl() {
    assert_eq!(expiry(1_000, Duration::from_millis(1_500)), 2_500);
}

#[test]
fn expiry_is_never_zero_or_now() {
    assert_eq!(expiry(0, Duration::ZERO), 1);
    assert_eq!(expiry(50, Duration::from_micros(10)), 51);
}

#[test]
fn expiry_saturates() {
    assert_eq!(expiry(u64::MAX - 1, Duration::from_secs(10)), u64::MAX);
    assert_eq!(expiry(0, Duration::MAX), u64::MAX);
}

#[test]
fn keep_removes_expired_entries_only() {
    assert!(keep(&encode(b"", None), 1_000));
    assert!(keep(&encode(b"", Some(2_000)), 1_000));
    assert!(!keep(&encode(b"", Some(1_000)), 1_000));
}

#[test]
fn keep_keeps_bytes_that_do_not_decode() {
    assert!(keep(b"not an envelope", u64::MAX));
    assert!(keep(b"", u64::MAX));
}

#[test]
fn unix_ms_counts_from_1970() {
    assert_eq!(unix_ms(UNIX_EPOCH + Duration::from_millis(1_234)), 1_234);
    assert_eq!(unix_ms(UNIX_EPOCH - Duration::from_secs(1)), 0);
    assert!(now_ms() > 1_600_000_000_000);
    let _ = SystemTime::now();
}

proptest! {
    #[test]
    fn decode_inverts_encode(payload in proptest::collection::vec(any::<u8>(), 0..64), expiry in proptest::option::of(1_u64..)) {
        let bytes = encode(&payload, expiry);
        let entry = decode(&bytes).unwrap();
        prop_assert_eq!(entry.payload, payload.as_slice());
        prop_assert_eq!(entry.expires_at, expiry);
    }

    #[test]
    fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..32)) {
        let _ = decode(&bytes);
        let _ = keep(&bytes, 0);
    }
}

#[test]
fn an_unknown_clock_expires_each_entry_with_an_expiry() {
    // `now_ms` gives 0 for a clock before 1970. Expiry then fails closed.
    let bytes = encode(b"", Some(u64::MAX));
    assert!(decode(&bytes).unwrap().is_expired(0));
    let bytes = encode(b"", None);
    assert!(!decode(&bytes).unwrap().is_expired(0));
}

#[test]
fn keep_keeps_each_entry_when_the_clock_is_unknown() {
    assert!(keep(&encode(b"", Some(5)), 0));
}
