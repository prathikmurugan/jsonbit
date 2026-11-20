/// A 64-bit word-aligned bitmap supporting hardware `POPCNT` (`u64::count_ones`),
/// branchless word-level bitwise algebra (`AND`, `OR`, `AND NOT`), and fast
/// set-bit iteration via `trailing_zeros()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitVec64 {
    pub words: Vec<u64>,
    pub len: usize,
}

impl BitVec64 {
    #[inline]
    pub fn new_empty(len: usize) -> Self {
        let num_words = (len + 63) / 64;
        Self {
            words: vec![0u64; num_words],
            len,
        }
    }

    #[inline]
    pub fn new_all_ones(len: usize) -> Self {
        let num_words = (len + 63) / 64;
        let mut words = vec![!0u64; num_words];
        let rem = len % 64;
        if rem != 0 && !words.is_empty() {
            let last = words.len() - 1;
            words[last] = (1u64 << rem) - 1;
        }
        Self { words, len }
    }

    #[inline]
    pub fn set(&mut self, idx: usize) {
        debug_assert!(idx < self.len);
        self.words[idx >> 6] |= 1u64 << (idx & 63);
    }

    #[inline]
    pub fn get(&self, idx: usize) -> bool {
        if idx >= self.len {
            return false;
        }
        (self.words[idx >> 6] & (1u64 << (idx & 63))) != 0
    }

    #[inline]
    pub fn and(&self, other: &Self) -> Self {
        debug_assert_eq!(self.len, other.len);
        let words = self
            .words
            .iter()
            .zip(other.words.iter())
            .map(|(&a, &b)| a & b)
            .collect();
        Self {
            words,
            len: self.len,
        }
    }

    #[inline]
    pub fn and_in_place(&mut self, other: &Self) {
        debug_assert_eq!(self.len, other.len);
        for (a, &b) in self.words.iter_mut().zip(other.words.iter()) {
            *a &= b;
        }
    }

    #[inline]
    pub fn or(&self, other: &Self) -> Self {
        debug_assert_eq!(self.len, other.len);
        let words = self
            .words
            .iter()
            .zip(other.words.iter())
            .map(|(&a, &b)| a | b)
            .collect();
        Self {
            words,
            len: self.len,
        }
    }

    #[inline]
    pub fn inverted_with_validity(&self, valid: &Self) -> Self {
        debug_assert_eq!(self.len, valid.len);
        let words = self
            .words
            .iter()
            .zip(valid.words.iter())
            .map(|(&a, &v)| (!a) & v)
            .collect();
        Self {
            words,
            len: self.len,
        }
    }

    /// Counts set bits using hardware POPCNT (`u64::count_ones`).
    #[inline]
    pub fn count_ones(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Iterates over all set bit indices in ascending order using `trailing_zeros()`
    /// and Kernighan's `w &= w - 1` bit clearing (skips 64-row empty blocks in 1 cycle).
    pub fn iter_ones(&self) -> Vec<usize> {
        let mut out = Vec::with_capacity(self.count_ones());
        for (word_idx, &word) in self.words.iter().enumerate() {
            let mut w = word;
            let base = word_idx << 6;
            while w != 0 {
                let tz = w.trailing_zeros() as usize;
                out.push(base + tz);
                w &= w - 1;
            }
        }
        out
    }

    #[inline]
    pub fn byte_size(&self) -> usize {
        self.words.len() * 8
    }
}
