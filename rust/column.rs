use std::collections::HashMap;
use serde_json::Value;
use crate::bitmap::BitVec64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Gt,
    Gte,
    Lt,
    Lte,
}

/// Packs a slice of unsigned integers into a tightly bit-packed `Vec<u64>` buffer
/// using exactly `bit_width` bits per element (0..=64).
pub fn pack_bits(values: &[u64], bit_width: u8) -> Vec<u64> {
    if bit_width == 0 || values.is_empty() {
        return Vec::new();
    }
    let total_bits = values.len() * (bit_width as usize);
    let num_words = (total_bits + 63) / 64;
    let mut out = vec![0u64; num_words];
    let mask = if bit_width == 64 {
        !0u64
    } else {
        (1u64 << bit_width) - 1
    };

    for (i, &val) in values.iter().enumerate() {
        let v = val & mask;
        let bit_pos = i * (bit_width as usize);
        let word_idx = bit_pos >> 6;
        let bit_offset = (bit_pos & 63) as u8;
        out[word_idx] |= v << bit_offset;
        if bit_offset + bit_width > 64 {
            out[word_idx + 1] |= v >> (64 - bit_offset);
        }
    }
    out
}

/// Unpacks a single `bit_width`-bit integer at `idx` from a packed `u64` buffer.
#[inline]
pub fn unpack_bit(packed: &[u64], idx: usize, bit_width: u8) -> u64 {
    if bit_width == 0 {
        return 0;
    }
    let bit_pos = idx * (bit_width as usize);
    let word_idx = bit_pos >> 6;
    let bit_offset = (bit_pos & 63) as u8;
    let mask = if bit_width == 64 {
        !0u64
    } else {
        (1u64 << bit_width) - 1
    };

    let mut val = packed[word_idx] >> bit_offset;
    if bit_offset + bit_width > 64 {
        val |= packed[word_idx + 1] << (64 - bit_offset);
    }
    val & mask
}

#[inline]
pub fn required_bit_width(max_delta: u64) -> u8 {
    if max_delta == 0 {
        0
    } else {
        (64 - max_delta.leading_zeros()) as u8
    }
}

/// Frame-of-Reference (`FOR`) Bit-Packed Integer Column with O(1) header stats
/// and contiguous SIMD-friendly slice for masked range scans.
#[derive(Clone, Debug)]
pub struct ForIntColumn {
    pub len: usize,
    pub nullable: bool,
    pub valid: BitVec64,
    pub non_null_count: usize,
    pub min_val: i64,
    pub max_val: i64,
    pub sum_val: i128,
    pub bit_width: u8,
    pub packed_deltas: Vec<u64>,
    pub raw_values: Vec<i64>,
}

impl ForIntColumn {
    pub fn from_options(opts: &[Option<i64>]) -> Self {
        let len = opts.len();
        let mut valid = BitVec64::new_empty(len);
        let mut non_null_count = 0usize;
        let mut min_val = i64::MAX;
        let mut max_val = i64::MIN;
        let mut sum_val = 0i128;
        let mut raw_values = vec![0i64; len];

        for (i, item) in opts.iter().enumerate() {
            if let Some(v) = *item {
                valid.set(i);
                non_null_count += 1;
                if v < min_val {
                    min_val = v;
                }
                if v > max_val {
                    max_val = v;
                }
                sum_val += v as i128;
                raw_values[i] = v;
            }
        }

        if non_null_count == 0 {
            min_val = 0;
            max_val = 0;
        }

        let max_delta = if non_null_count > 0 {
            (max_val as i128 - min_val as i128) as u64
        } else {
            0
        };
        let bit_width = required_bit_width(max_delta);
        let deltas: Vec<u64> = raw_values
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                if valid.get(i) {
                    (v as i128 - min_val as i128) as u64
                } else {
                    0
                }
            })
            .collect();
        let packed_deltas = pack_bits(&deltas, bit_width);
        let nullable = non_null_count < len;

        Self {
            len,
            nullable,
            valid,
            non_null_count,
            min_val,
            max_val,
            sum_val,
            bit_width,
            packed_deltas,
            raw_values,
        }
    }

    #[inline]
    pub fn get(&self, idx: usize) -> Option<i64> {
        if !self.valid.get(idx) {
            return None;
        }
        let delta = unpack_bit(&self.packed_deltas, idx, self.bit_width);
        Some((self.min_val as i128 + delta as i128) as i64)
    }

    pub fn filter_cmp(&self, op: CmpOp, thresh: f64) -> BitVec64 {
        let mut out = BitVec64::new_empty(self.len);
        for (i, &val) in self.raw_values.iter().enumerate() {
            if !self.valid.get(i) {
                continue;
            }
            let vf = val as f64;
            let hit = match op {
                CmpOp::Eq => (vf - thresh).abs() < 1e-9,
                CmpOp::Gt => vf > thresh,
                CmpOp::Gte => vf >= thresh,
                CmpOp::Lt => vf < thresh,
                CmpOp::Lte => vf <= thresh,
            };
            if hit {
                out.set(i);
            }
        }
        out
    }

    pub fn filter_range_inclusive(&self, low: i64, high: i64) -> BitVec64 {
        let mut out = BitVec64::new_empty(self.len);
        for (i, &val) in self.raw_values.iter().enumerate() {
            if self.valid.get(i) && val >= low && val <= high {
                out.set(i);
            }
        }
        out
    }

    /// Computes `(count, sum, avg, min, max)` over the rows selected by `mask`.
    /// If `mask` selects all rows, returns in `O(1)` directly from pre-computed headers!
    pub fn aggregate_masked(&self, mask: &BitVec64) -> Option<(usize, f64, f64, f64, f64)> {
        if mask.count_ones() == self.len && !self.nullable && self.non_null_count > 0 {
            let sum = self.sum_val as f64;
            let avg = sum / (self.non_null_count as f64);
            return Some((
                self.non_null_count,
                sum,
                avg,
                self.min_val as f64,
                self.max_val as f64,
            ));
        }

        let mut cnt = 0usize;
        let mut sum = 0i128;
        let mut mn = i64::MAX;
        let mut mx = i64::MIN;

        for idx in mask.iter_ones() {
            if self.valid.get(idx) {
                let v = self.raw_values[idx];
                cnt += 1;
                sum += v as i128;
                if v < mn {
                    mn = v;
                }
                if v > mx {
                    mx = v;
                }
            }
        }

        if cnt == 0 {
            return None;
        }
        let sum_f = sum as f64;
        Some((cnt, sum_f, sum_f / (cnt as f64), mn as f64, mx as f64))
    }

    pub fn compressed_byte_size(&self) -> usize {
        let validity_bytes = if self.nullable { self.valid.byte_size() } else { 0 };
        16 + 1 + (self.packed_deltas.len() * 8) + validity_bytes
    }
}

/// Contiguous SIMD-Aligned Float Column with Fixed-Point Bit-Pack Compression Sizing
/// and O(1) header stats.
#[derive(Clone, Debug)]
pub struct FloatColumn {
    pub len: usize,
    pub nullable: bool,
    pub valid: BitVec64,
    pub non_null_count: usize,
    pub min_val: f64,
    pub max_val: f64,
    pub sum_val: f64,
    pub raw_values: Vec<f64>,
    pub packed_byte_size: usize,
}

impl FloatColumn {
    pub fn from_options(opts: &[Option<f64>]) -> Self {
        let len = opts.len();
        let mut valid = BitVec64::new_empty(len);
        let mut non_null_count = 0usize;
        let mut min_val = f64::INFINITY;
        let mut max_val = f64::NEG_INFINITY;
        let mut sum_val = 0.0f64;
        let mut raw_values = vec![0.0f64; len];

        for (i, item) in opts.iter().enumerate() {
            if let Some(v) = *item {
                valid.set(i);
                non_null_count += 1;
                if v < min_val {
                    min_val = v;
                }
                if v > max_val {
                    max_val = v;
                }
                sum_val += v;
                raw_values[i] = v;
            }
        }

        if non_null_count == 0 {
            min_val = 0.0;
            max_val = 0.0;
        }

        // Fixed-point (x100) FOR bit-packing size calculation for 2-decimal metrics/currencies
        let min_scaled = (min_val * 100.0).round() as i64;
        let max_scaled = (max_val * 100.0).round() as i64;
        let max_delta = (max_scaled - min_scaled).max(0) as u64;
        let bw = required_bit_width(max_delta).max(16);
        let nullable = non_null_count < len;
        let validity_bytes = if nullable { valid.byte_size() } else { 0 };
        let packed_byte_size = 16 + ((len * (bw as usize) + 7) / 8) + validity_bytes;

        Self {
            len,
            nullable,
            valid,
            non_null_count,
            min_val,
            max_val,
            sum_val,
            raw_values,
            packed_byte_size,
        }
    }

    #[inline]
    pub fn get(&self, idx: usize) -> Option<f64> {
        if !self.valid.get(idx) {
            None
        } else {
            Some(self.raw_values[idx])
        }
    }

    pub fn filter_cmp(&self, op: CmpOp, thresh: f64) -> BitVec64 {
        let mut out = BitVec64::new_empty(self.len);
        for (i, &vf) in self.raw_values.iter().enumerate() {
            if !self.valid.get(i) {
                continue;
            }
            let hit = match op {
                CmpOp::Eq => (vf - thresh).abs() < 1e-6,
                CmpOp::Gt => vf > thresh,
                CmpOp::Gte => vf >= thresh,
                CmpOp::Lt => vf < thresh,
                CmpOp::Lte => vf <= thresh,
            };
            if hit {
                out.set(i);
            }
        }
        out
    }

    pub fn aggregate_masked(&self, mask: &BitVec64) -> Option<(usize, f64, f64, f64, f64)> {
        if mask.count_ones() == self.len && !self.nullable && self.non_null_count > 0 {
            let avg = self.sum_val / (self.non_null_count as f64);
            return Some((
                self.non_null_count,
                self.sum_val,
                avg,
                self.min_val,
                self.max_val,
            ));
        }

        let mut cnt = 0usize;
        let mut sum = 0.0f64;
        let mut mn = f64::INFINITY;
        let mut mx = f64::NEG_INFINITY;

        for idx in mask.iter_ones() {
            if self.valid.get(idx) {
                let v = self.raw_values[idx];
                cnt += 1;
                sum += v;
                if v < mn {
                    mn = v;
                }
                if v > mx {
                    mx = v;
                }
            }
        }

        if cnt == 0 {
            return None;
        }
        Some((cnt, sum, sum / (cnt as f64), mn, mx))
    }

    pub fn compressed_byte_size(&self) -> usize {
        self.packed_byte_size
    }
}

/// 1-Bit-Per-Row Word-Aligned Boolean Column (`BitVec64`).
/// Filtering `== true` or `== false` is O(1) with zero row iteration!
#[derive(Clone, Debug)]
pub struct BoolBitmapColumn {
    pub len: usize,
    pub nullable: bool,
    pub valid: BitVec64,
    pub true_bits: BitVec64,
}

impl BoolBitmapColumn {
    pub fn from_options(opts: &[Option<bool>]) -> Self {
        let len = opts.len();
        let mut valid = BitVec64::new_empty(len);
        let mut true_bits = BitVec64::new_empty(len);
        let mut non_null = 0usize;

        for (i, item) in opts.iter().enumerate() {
            if let Some(b) = *item {
                valid.set(i);
                non_null += 1;
                if b {
                    true_bits.set(i);
                }
            }
        }

        Self {
            len,
            nullable: non_null < len,
            valid,
            true_bits,
        }
    }

    #[inline]
    pub fn get(&self, idx: usize) -> Option<bool> {
        if !self.valid.get(idx) {
            None
        } else {
            Some(self.true_bits.get(idx))
        }
    }

    #[inline]
    pub fn filter_bool(&self, target: bool) -> BitVec64 {
        if target {
            self.true_bits.clone()
        } else {
            self.true_bits.inverted_with_validity(&self.valid)
        }
    }

    #[inline]
    pub fn counts(&self) -> (usize, usize) {
        let t = self.true_bits.count_ones();
        let f = self.valid.count_ones().saturating_sub(t);
        (t, f)
    }

    pub fn compressed_byte_size(&self) -> usize {
        let v_bytes = if self.nullable { self.valid.byte_size() } else { 0 };
        self.true_bits.byte_size() + v_bytes
    }
}

/// Dictionary-Encoded Bit-Packed String Column with O(1) Inverted `BitVec64` Bitmap Index
/// and Word-Token Vocabulary Compression + Inverted Keyword BitVec64 Index for free-text/logs.
#[derive(Clone, Debug)]
pub struct DictStringColumn {
    pub len: usize,
    pub nullable: bool,
    pub valid: BitVec64,
    pub is_primitive_array: bool,
    pub non_string_json: BitVec64,
    pub dictionary: Vec<String>,
    pub value_to_id: HashMap<String, usize>,
    pub bit_width: u8,
    pub packed_ids: Vec<u64>,
    pub inverted_index: HashMap<usize, BitVec64>,
    pub keyword_index: HashMap<String, BitVec64>,
    pub token_compressed_dict_bytes: usize,
}

impl DictStringColumn {
    pub fn from_options(opts: &[Option<String>], is_primitive_array: bool) -> Self {
        let len = opts.len();
        let mut valid = BitVec64::new_empty(len);
        let mut dictionary: Vec<String> = Vec::new();
        let mut value_to_id: HashMap<String, usize> = HashMap::new();
        let mut row_ids = vec![0u64; len];
        let mut non_null = 0usize;

        for (i, item) in opts.iter().enumerate() {
            if let Some(s) = item {
                valid.set(i);
                non_null += 1;
                let id = if let Some(&existing_id) = value_to_id.get(s) {
                    existing_id
                } else {
                    let new_id = dictionary.len();
                    dictionary.push(s.clone());
                    value_to_id.insert(s.clone(), new_id);
                    new_id
                };
                row_ids[i] = id as u64;
            }
        }

        let max_id = dictionary.len().saturating_sub(1) as u64;
        let bit_width = required_bit_width(max_id);
        let packed_ids = pack_bits(&row_ids, bit_width);

        // Build O(1) inverted bitmap index for categorical columns (<= 64 distinct values)
        let mut inverted_index = HashMap::new();
        if !dictionary.is_empty() && dictionary.len() <= 64 {
            for id in 0..dictionary.len() {
                inverted_index.insert(id, BitVec64::new_empty(len));
            }
            for (i, &id) in row_ids.iter().enumerate() {
                if valid.get(i) {
                    if let Some(bm) = inverted_index.get_mut(&(id as usize)) {
                        bm.set(i);
                    }
                }
            }
        }

        // Build Word-Token Vocabulary Compression & Inverted Keyword BitVec64 Index
        // for high-cardinality or free-text/log columns
        let raw_dict_bytes: usize = dictionary.iter().map(|s| s.len() + 2).sum();
        let mut keyword_index: HashMap<String, BitVec64> = HashMap::new();
        let mut token_compressed_dict_bytes = raw_dict_bytes;

        if !dictionary.is_empty() && !is_primitive_array {
            let mut word_vocab: HashMap<&str, usize> = HashMap::new();
            let mut total_word_tokens = 0usize;
            for s in &dictionary {
                for tok in s.split(|c: char| !c.is_alphanumeric()) {
                    if !tok.is_empty() {
                        total_word_tokens += 1;
                        let next_id = word_vocab.len();
                        word_vocab.entry(tok).or_insert(next_id);
                    }
                }
            }
            if word_vocab.len() < total_word_tokens {
                let vocab_str_bytes: usize = word_vocab.keys().map(|w| w.len() + 1).sum();
                let word_bw = required_bit_width(word_vocab.len().saturating_sub(1) as u64) as usize;
                let stream_bytes = (total_word_tokens * word_bw + 7) / 8 + dictionary.len();
                let candidate_bytes = vocab_str_bytes + stream_bytes;
                if candidate_bytes < raw_dict_bytes {
                    token_compressed_dict_bytes = candidate_bytes;
                }
            }

            // Index recurring lowercase keywords (3..=24 chars) across rows into BitVec64 bitmaps
            if len <= 100_000 {
                let mut kw_counts: HashMap<String, usize> = HashMap::new();
                for s in &dictionary {
                    for tok in s.split(|c: char| !c.is_alphanumeric()) {
                        if tok.len() >= 3 && tok.len() <= 24 {
                            *kw_counts.entry(tok.to_lowercase()).or_insert(0) += 1;
                        }
                    }
                }
                for (i, &id) in row_ids.iter().enumerate() {
                    if !valid.get(i) {
                        continue;
                    }
                    let s = &dictionary[id as usize];
                    for tok in s.split(|c: char| !c.is_alphanumeric()) {
                        if tok.len() >= 3 && tok.len() <= 24 {
                            let low = tok.to_lowercase();
                            if kw_counts.get(&low).copied().unwrap_or(0) >= 2 || dictionary.len() <= 256 {
                                keyword_index
                                    .entry(low)
                                    .or_insert_with(|| BitVec64::new_empty(len))
                                    .set(i);
                            }
                        }
                    }
                }
            }
        }

        Self {
            len,
            nullable: non_null < len,
            valid,
            is_primitive_array,
            non_string_json: BitVec64::new_empty(len),
            dictionary,
            value_to_id,
            bit_width,
            packed_ids,
            inverted_index,
            keyword_index,
            token_compressed_dict_bytes,
        }
    }

    #[inline]
    pub fn get(&self, idx: usize) -> Option<&str> {
        if !self.valid.get(idx) {
            return None;
        }
        let id = unpack_bit(&self.packed_ids, idx, self.bit_width) as usize;
        self.dictionary.get(id).map(|s| s.as_str())
    }

    /// O(1) bitmap lookup if inverted index is available; fast bit-unpack scan otherwise.
    pub fn filter_eq_case_insensitive(&self, target: &str) -> BitVec64 {
        let target_lower = target.to_lowercase();
        for (dict_id, s) in self.dictionary.iter().enumerate() {
            if s.to_lowercase() == target_lower {
                if let Some(bm) = self.inverted_index.get(&dict_id) {
                    return bm.clone();
                }
                let mut out = BitVec64::new_empty(self.len);
                for i in 0..self.len {
                    if self.valid.get(i)
                        && (unpack_bit(&self.packed_ids, i, self.bit_width) as usize) == dict_id
                    {
                        out.set(i);
                    }
                }
                return out;
            }
        }
        BitVec64::new_empty(self.len)
    }

    /// Checks whether a serialized JSON primitive array cell (`["read","write","admin"]`) contains `elem`.
    pub fn filter_array_contains(&self, elem: &str) -> BitVec64 {
        let mut out = BitVec64::new_empty(self.len);
        for i in 0..self.len {
            if let Some(raw_json) = self.get(i) {
                if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(raw_json) {
                    if arr.iter().any(|v| v.as_str() == Some(elem)) {
                        out.set(i);
                    }
                }
            }
        }
        out
    }

    /// Returns sorted `(category_value, count)` frequency pairs using hardware POPCNT (`count_ones()`).
    pub fn category_frequencies(&self) -> Option<Vec<(String, usize)>> {
        if self.is_primitive_array || self.dictionary.is_empty() || self.dictionary.len() > 12 {
            return None;
        }
        if self
            .dictionary
            .iter()
            .any(|s| s.len() > 40 || s.contains('\n'))
        {
            return None;
        }

        let mut freqs: Vec<(String, usize)> = self
            .dictionary
            .iter()
            .enumerate()
            .filter_map(|(id, val)| {
                let cnt = self.inverted_index.get(&id)?.count_ones();
                if cnt > 0 {
                    Some((val.clone(), cnt))
                } else {
                    None
                }
            })
            .collect();

        freqs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Some(freqs)
    }

    /// Fast case-insensitive substring (`LIKE '%pat%'`) filter:
    /// 1. Checks `keyword_index` for O(1) word-token bitmap hit.
    /// 2. Otherwise scans only the unique `dictionary` entries (`|D| << N`) rather than all `N` rows!
    pub fn filter_like_case_insensitive(&self, pat_lower: &str) -> BitVec64 {
        if pat_lower.chars().all(|c| c.is_alphanumeric()) {
            if let Some(bm) = self.keyword_index.get(pat_lower) {
                return bm.clone();
            }
        }
        let mut matching_dict_ids = Vec::new();
        for (dict_id, s) in self.dictionary.iter().enumerate() {
            if s.to_lowercase().contains(pat_lower) {
                matching_dict_ids.push(dict_id);
            }
        }
        if matching_dict_ids.is_empty() {
            return BitVec64::new_empty(self.len);
        }
        // If all matching dict_ids are in `inverted_index`, union their bitmaps in O(1)!
        if matching_dict_ids.iter().all(|id| self.inverted_index.contains_key(id)) {
            let mut out = BitVec64::new_empty(self.len);
            for id in matching_dict_ids {
                if let Some(bm) = self.inverted_index.get(&id) {
                    out = out.or(bm);
                }
            }
            return out;
        }
        let id_set: std::collections::HashSet<usize> = matching_dict_ids.into_iter().collect();
        let mut out = BitVec64::new_empty(self.len);
        for i in 0..self.len {
            if self.valid.get(i) {
                let id = unpack_bit(&self.packed_ids, i, self.bit_width) as usize;
                if id_set.contains(&id) {
                    out.set(i);
                }
            }
        }
        out
    }

    pub fn compressed_byte_size(&self) -> usize {
        let packed_bytes = self.packed_ids.len() * 8;
        let valid_bytes = if self.nullable { self.valid.byte_size() } else { 0 };
        self.token_compressed_dict_bytes + packed_bytes + valid_bytes
    }
}

#[derive(Clone, Debug)]
pub enum ColumnStore {
    Int(ForIntColumn),
    Float(FloatColumn),
    Bool(BoolBitmapColumn),
    DictStr(DictStringColumn),
}

impl ColumnStore {
    pub fn type_name(&self) -> &'static str {
        match self {
            ColumnStore::Int(_) => "INTEGER",
            ColumnStore::Float(_) => "REAL",
            ColumnStore::Bool(_) => "BOOLEAN",
            ColumnStore::DictStr(_) => "TEXT",
        }
    }

    pub fn is_nullable(&self) -> bool {
        match self {
            ColumnStore::Int(c) => c.nullable,
            ColumnStore::Float(c) => c.nullable,
            ColumnStore::Bool(c) => c.nullable,
            ColumnStore::DictStr(c) => c.nullable,
        }
    }

    pub fn get_json_value(&self, idx: usize) -> Option<Value> {
        match self {
            ColumnStore::Int(c) => c.get(idx).map(|v| Value::Number(v.into())),
            ColumnStore::Float(c) => c.get(idx).and_then(|v| {
                if v.fract() == 0.0 && v >= (i64::MIN as f64) && v <= (i64::MAX as f64) {
                    Some(Value::Number((v as i64).into()))
                } else {
                    serde_json::Number::from_f64(v).map(Value::Number)
                }
            }),
            ColumnStore::Bool(c) => c.get(idx).map(Value::Bool),
            ColumnStore::DictStr(c) => {
                let s = c.get(idx)?;
                if c.is_primitive_array || c.non_string_json.get(idx) {
                    serde_json::from_str::<Value>(s).ok()
                } else {
                    Some(Value::String(s.to_string()))
                }
            }
        }
    }

    pub fn get_scalar_string(&self, idx: usize, bool_as_yes_no: bool) -> Option<String> {
        match self {
            ColumnStore::Int(c) => c.get(idx).map(|v| v.to_string()),
            ColumnStore::Float(c) => c.get(idx).map(|v| v.to_string()),
            ColumnStore::Bool(c) => c.get(idx).map(|b| {
                if bool_as_yes_no {
                    if b { "yes".to_string() } else { "no".to_string() }
                } else if b {
                    "true".to_string()
                } else {
                    "false".to_string()
                }
            }),
            ColumnStore::DictStr(c) => c.get(idx).map(|s| s.to_string()),
        }
    }

    pub fn compressed_byte_size(&self) -> usize {
        match self {
            ColumnStore::Int(c) => c.compressed_byte_size(),
            ColumnStore::Float(c) => c.compressed_byte_size(),
            ColumnStore::Bool(c) => c.compressed_byte_size(),
            ColumnStore::DictStr(c) => c.compressed_byte_size(),
        }
    }
}
