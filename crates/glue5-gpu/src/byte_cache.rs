//! Idle buffers need a byte budget, not just an object-count limit: a single
//! loading-screen upload or screenshot can be much larger than normal work.

pub struct ByteCache<T> {
    entries: Vec<(u64, T)>,
    bytes: u64,
    budget: u64,
    largest: u64,
    count: usize,
}

impl<T> ByteCache<T> {
    pub fn new(budget: u64, largest: u64, count: usize) -> Self {
        Self { entries: Vec::new(), bytes: 0, budget, largest, count }
    }

    /// Smallest suitable buffer; exact size is useful for readbacks.
    pub fn take(&mut self, size: u64, exact: bool) -> Option<T> {
        let i = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, (bytes, _))| if exact { *bytes == size } else { *bytes >= size })
            .min_by_key(|(_, (bytes, _))| *bytes)
            .map(|(i, _)| i)?;
        let (bytes, value) = self.entries.swap_remove(i);
        self.bytes -= bytes;
        Some(value)
    }

    /// Returns rejected objects to the caller for safe destruction.
    pub fn insert(&mut self, size: u64, value: T) -> Result<(), T> {
        if size > self.largest || size > self.budget.saturating_sub(self.bytes) || self.entries.len() >= self.count {
            return Err(value);
        }
        self.entries.push((size, value));
        self.bytes += size;
        Ok(())
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_bytes_and_rejects_large_one_off_buffers() {
        let mut cache = ByteCache::new(32, 8, 16);
        assert_eq!(cache.insert(16, "large"), Err("large"));
        for _ in 0..4 {
            cache.insert(8, "normal").unwrap();
        }
        assert_eq!(cache.bytes(), 32);
        assert_eq!(cache.insert(4, "over budget"), Err("over budget"));
        assert_eq!(cache.take(8, true), Some("normal"));
        cache.insert(4, "small").unwrap();
        assert_eq!(cache.bytes(), 28);
    }

    #[test]
    fn chooses_best_fit_and_preserves_exact_readback_matching() {
        let mut cache = ByteCache::new(100, 50, 3);
        cache.insert(32, 32).unwrap();
        cache.insert(8, 8).unwrap();
        cache.insert(16, 16).unwrap();
        assert_eq!(cache.take(9, true), None);
        assert_eq!(cache.take(9, false), Some(16));
        assert_eq!(cache.take(4, false), Some(8));
        assert_eq!(cache.bytes(), 32);
        assert_eq!(cache.take(32, true), Some(32));
        assert_eq!(cache.bytes(), 0);
    }

    #[test]
    fn count_limit_and_overflow_are_safe() {
        let mut cache = ByteCache::new(u64::MAX, u64::MAX, 1);
        cache.insert(u64::MAX, 1).unwrap();
        assert_eq!(cache.insert(1, 2), Err(2));
        assert_eq!(cache.take(u64::MAX, true), Some(1));
        cache.insert(0, 3).unwrap();
        assert_eq!(cache.insert(0, 4), Err(4));
    }
}
