//! The value envelope for entries with a time to live (TTL).
//!
//! # Contract
//!
//! - An envelope is one version byte (`1`), an 8-byte big-endian expiry and the payload.
//! - The expiry is Unix time in milliseconds. The expiry `0` means "no expiry".
//! - An entry is expired when its expiry is at or before the current time.
//! - A current time of `0` means the clock is before 1970. Then each entry with an expiry is expired.
//! - [`expiry`] never gives `0`. A zero TTL gives an expiry 1 ms after `now`.
//! - [`decode`] refuses a short envelope and an unknown version.
//! - [`keep`] keeps an entry that does not decode. The compaction filter never removes unknown data.
//! - [`keep`] keeps each entry when the current time is `0`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The envelope version.
const VERSION: u8 = 1;

/// The header length: the version byte and the expiry.
pub(crate) const HEADER: usize = 9;

/// The envelope bytes are not valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BadEnvelope;

/// A decoded envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Envelope<'a> {
    /// The expiry in Unix milliseconds. `None` means "no expiry".
    pub(crate) expires_at: Option<u64>,
    /// The stored bytes.
    pub(crate) payload: &'a [u8],
}

impl Envelope<'_> {
    /// Returns `true` if the entry is expired at `now_ms`.
    pub(crate) const fn is_expired(&self, now_ms: u64) -> bool {
        match self.expires_at {
            // A clock before 1970 gives 0. Expiry fails closed.
            Some(at) => now_ms == 0 || at <= now_ms,
            None => false,
        }
    }
}

/// Makes an envelope.
pub(crate) fn encode(payload: &[u8], expires_at: Option<u64>) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER + payload.len());
    bytes.push(VERSION);
    bytes.extend_from_slice(&expires_at.unwrap_or(0).to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

/// Reads an envelope.
pub(crate) const fn decode(bytes: &[u8]) -> Result<Envelope<'_>, BadEnvelope> {
    let Some((&VERSION, rest)) = bytes.split_first() else {
        return Err(BadEnvelope);
    };
    let Some((expiry, payload)) = rest.split_first_chunk::<8>() else {
        return Err(BadEnvelope);
    };
    let expires_at = match u64::from_be_bytes(*expiry) {
        0 => None,
        at => Some(at),
    };
    Ok(Envelope {
        expires_at,
        payload,
    })
}

/// Gives the expiry of an entry with `ttl`, written at `now_ms`.
pub(crate) fn expiry(now_ms: u64, ttl: Duration) -> u64 {
    let ttl_ms = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX).max(1);
    now_ms.saturating_add(ttl_ms)
}

/// Returns `true` if the compaction filter keeps the entry at `now_ms`.
pub(crate) fn keep(bytes: &[u8], now_ms: u64) -> bool {
    now_ms == 0 || decode(bytes).map_or(true, |envelope| !envelope.is_expired(now_ms))
}

/// The current Unix time in milliseconds. A clock before 1970 gives `0`.
pub(crate) fn now_ms() -> u64 {
    unix_ms(SystemTime::now())
}

/// Converts `time` to Unix milliseconds. A time before 1970 gives `0`.
pub(crate) fn unix_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |since| {
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    })
}

#[cfg(test)]
mod tests;
