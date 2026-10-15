//! Bounds-checked texture dictionary registration lookup for script assets.
//!
//! Upstream handles every missing dictionary. Unknown/corrupt stores must
//! not be mistaken for missing assets and must not hang while walking a cycle.

use crate::memory::Mem;

// Texture-dictionary name map in the SHA-verified 8b0b5899ed module. These
// are the same globals read by graphics_commands::HasStreamedTxdLoaded.
const ENTRIES: u64 = 11_097_504;
const BUCKET_HEADS: u64 = 11_097_512;
const BUCKET_COUNT: u64 = 11_097_520;
const NO_SLOT: u32 = u32::MAX;
const MAX_CHAIN: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Registration {
    /// Not initialized, unreadable or an invalid chain: never bypass.
    Unknown,
    Present,
    Absent,
}

pub(crate) fn txd_registration(mem: Mem, bytes: u64, hash: u32) -> Registration {
    registration(hash, |addr| contains(bytes, addr, 4).then(|| mem.u32(addr)))
}

fn contains(bytes: u64, addr: u64, len: u64) -> bool {
    addr.checked_add(len).is_some_and(|end| end <= bytes)
}

/// Read the store, not the filesystem: dictionaries may live inside mounted
/// RPF archives or overlays. The read callback keeps every dereference within
/// committed WASM memory and makes malformed/uninitialized maps testable.
fn registration(hash: u32, read: impl Fn(u64) -> Option<u32>) -> Registration {
    let pointer = |addr| -> Option<u64> { Some(read(addr)? as u64 | ((read(addr.checked_add(4)?)? as u64) << 32)) };
    let Some(buckets) = read(BUCKET_COUNT).filter(|&n| n != 0) else {
        return Registration::Unknown;
    };
    let Some(heads) = pointer(BUCKET_HEADS).filter(|&p| p != 0) else {
        return Registration::Unknown;
    };
    let Some(head) = heads.checked_add(4 * u64::from(hash % buckets)).and_then(&read) else {
        return Registration::Unknown;
    };
    if head == NO_SLOT {
        return Registration::Absent;
    }
    let Some(entries) = pointer(ENTRIES).filter(|&p| p != 0) else {
        return Registration::Unknown;
    };
    let mut index = head;
    for _ in 0..MAX_CHAIN {
        let Some(at) = entries.checked_add(12 * u64::from(index)) else {
            return Registration::Unknown;
        };
        let Some(entry_hash) = read(at) else { return Registration::Unknown };
        if entry_hash == hash {
            return match at.checked_add(4).and_then(&read) {
                Some(NO_SLOT) => Registration::Absent,
                Some(_) => Registration::Present,
                None => Registration::Unknown,
            };
        }
        match at.checked_add(8).and_then(&read) {
            Some(NO_SLOT) => return Registration::Absent,
            Some(next) => index = next,
            None => return Registration::Unknown,
        }
    }
    Registration::Unknown // A cycle/overlong chain is not proof of absence.
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const SHOP_HASH: u32 = 0xfdf0_5854;
    const HEADS: u64 = 1024;
    const ITEMS: u64 = 4096;

    fn store(head: u32, entries: &[[u32; 3]]) -> HashMap<u64, u32> {
        let mut words = HashMap::from([
            (BUCKET_COUNT, 7),
            (BUCKET_HEADS, HEADS as u32),
            (BUCKET_HEADS + 4, 0),
            (ENTRIES, ITEMS as u32),
            (ENTRIES + 4, 0),
            (HEADS + 4 * u64::from(SHOP_HASH % 7), head),
        ]);
        for (i, entry) in entries.iter().enumerate() {
            for (j, &word) in entry.iter().enumerate() {
                words.insert(ITEMS + 12 * i as u64 + 4 * j as u64, word);
            }
        }
        words
    }

    fn lookup(words: &HashMap<u64, u32>) -> Registration {
        registration(SHOP_HASH, |addr| words.get(&addr).copied())
    }

    #[test]
    fn registered_but_unloaded_dictionaries_keep_waiting() {
        let words = store(0, &[[SHOP_HASH, 123, NO_SLOT]]);
        assert_eq!(lookup(&words), Registration::Present);
    }

    #[test]
    fn missing_bucket_and_removed_slot_are_absent() {
        assert_eq!(lookup(&store(NO_SLOT, &[])), Registration::Absent);
        assert_eq!(lookup(&store(0, &[[SHOP_HASH, NO_SLOT, NO_SLOT]])), Registration::Absent);
        assert_eq!(lookup(&store(0, &[[SHOP_HASH ^ 1, 1, NO_SLOT]])), Registration::Absent);
    }

    #[test]
    fn collisions_are_walked_before_deciding_a_dictionary_is_absent() {
        let words = store(0, &[[SHOP_HASH ^ 1, 4, 1], [SHOP_HASH, 567, NO_SLOT]]);
        assert_eq!(lookup(&words), Registration::Present);
        let words = store(0, &[[SHOP_HASH ^ 1, 4, 1], [SHOP_HASH ^ 2, 5, NO_SLOT]]);
        assert_eq!(lookup(&words), Registration::Absent);
    }

    #[test]
    fn uninitialized_truncated_and_cyclic_stores_never_bypass_loading() {
        assert_eq!(lookup(&HashMap::new()), Registration::Unknown);
        let mut words = store(NO_SLOT, &[]);
        words.insert(BUCKET_COUNT, 0);
        assert_eq!(lookup(&words), Registration::Unknown);
        words.insert(BUCKET_COUNT, 7);
        words.insert(BUCKET_HEADS, 0);
        assert_eq!(lookup(&words), Registration::Unknown);
        let mut words = store(0, &[[SHOP_HASH, 42, NO_SLOT]]);
        words.remove(&(ITEMS + 4));
        assert_eq!(lookup(&words), Registration::Unknown);
        assert_eq!(lookup(&store(10, &[])), Registration::Unknown);
        assert_eq!(lookup(&store(0, &[[SHOP_HASH ^ 1, 4, 0]])), Registration::Unknown);
    }

    #[test]
    fn pointer_overflow_and_out_of_bounds_reads_fail_closed() {
        assert!(contains(16, 12, 4));
        assert!(!contains(16, 13, 4));
        assert!(!contains(u64::MAX, u64::MAX - 1, 4));
        let mut words = store(0, &[]);
        words.insert(ENTRIES, u32::MAX);
        words.insert(ENTRIES + 4, u32::MAX);
        assert_eq!(lookup(&words), Registration::Unknown);
    }

    #[test]
    fn shop_hash_matches_the_engine_name_hash() {
        let mut hash = 0u32;
        for c in b"mpshopsale" {
            hash = hash.wrapping_add(u32::from(*c));
            hash = hash.wrapping_add(hash << 10);
            hash ^= hash >> 6;
        }
        hash = hash.wrapping_add(hash << 3);
        hash ^= hash >> 11;
        hash = hash.wrapping_add(hash << 15);
        assert_eq!(SHOP_HASH, hash);
    }
}
