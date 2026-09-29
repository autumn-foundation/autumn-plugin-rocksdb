//! Scan bounds in bytewise key order.
//!
//! # Contract
//!
//! - [`prefix_end`] gives the smallest key that is larger than each key with the prefix.
//!   It gives `None` for an empty prefix and for a prefix of `0xFF` bytes only.
//! - [`Bounds::new`] joins a prefix, an inclusive start, an exclusive end and an exclusive cursor.
//!   The lower bound is inclusive. The upper bound is exclusive. `None` means "no upper bound".
//! - A key is in the bounds only if it satisfies each given condition.

/// The key range of one scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Bounds {
    /// The first key to read.
    pub(crate) lower: Vec<u8>,
    /// The first key not to read. `None` means "no upper bound".
    pub(crate) upper: Option<Vec<u8>>,
}

/// The conditions of one scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Range {
    /// Each key starts with these bytes.
    pub(crate) prefix: Option<Vec<u8>>,
    /// Each key is equal to or larger than this key.
    pub(crate) start: Option<Vec<u8>>,
    /// Each key is smaller than this key.
    pub(crate) end: Option<Vec<u8>>,
    /// Each key is larger than this key. The cursor of the last page.
    pub(crate) after: Option<Vec<u8>>,
}

impl Bounds {
    /// Makes the bounds of `range`.
    pub(crate) fn new(range: &Range) -> Self {
        // The key directly after `after` in bytewise order is `after` + `0x00`.
        let after = range.after.as_ref().map(|after| {
            let mut next = after.clone();
            next.push(0);
            next
        });
        let lower = [range.prefix.clone(), range.start.clone(), after]
            .into_iter()
            .flatten()
            .max()
            .unwrap_or_default();
        let upper = [
            range.prefix.as_deref().and_then(prefix_end),
            range.end.clone(),
        ]
        .into_iter()
        .flatten()
        .min();
        Self { lower, upper }
    }

    /// Returns `true` if no key is in the bounds.
    pub(crate) fn is_empty(&self) -> bool {
        self.upper
            .as_ref()
            .is_some_and(|upper| *upper <= self.lower)
    }

    /// Returns `true` if `key` is in the bounds.
    pub(crate) fn contains(&self, key: &[u8]) -> bool {
        key >= self.lower.as_slice()
            && self
                .upper
                .as_ref()
                .is_none_or(|upper| key < upper.as_slice())
    }
}

/// Gives the smallest key that is larger than each key that starts with `prefix`.
pub(crate) fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let last = prefix.iter().rposition(|&byte| byte != 0xFF)?;
    let mut end = prefix[..=last].to_vec();
    end[last] += 1;
    Some(end)
}

#[cfg(test)]
mod tests;
