use std::fs;
use std::io::{Cursor, Read};
use serde_json::Value;
use crate::bitmap::BitVec64;
use crate::column::{
    BoolBitmapColumn, ColumnStore, DictStringColumn, FloatColumn, ForIntColumn,
};
use crate::engine::JsonOlapEngine;
use crate::normalizer::{ColumnMeta, ColumnarTable};

const MAGIC: &[u8; 5] = b"JSBT\x01";

/// Serializes a `JsonOlapEngine`'s bit-packed 3NF `ColumnarTable`s into a compact
/// binary `.jsonbit` snapshot (typically 60-85% smaller than minified JSON).
pub fn serialize_snapshot(engine: &JsonOlapEngine) -> Vec<u8> {
    let mut out = Vec::with_capacity(engine.compressed_byte_size() + 256);
    out.extend_from_slice(MAGIC);
    write_u32(&mut out, engine.tables.len() as u32);

    for t in &engine.tables {
        write_str(&mut out, &t.name);
        write_str_vec(&mut out, &t.json_path);
        write_str(&mut out, &t.primary_key);
        out.push(if t.is_keyed_map { 1 } else { 0 });
        out.push(if t.is_singleton { 1 } else { 0 });
        write_u32(&mut out, t.row_count as u32);
        write_str_vec(&mut out, &t.top_level_fields);

        match &t.parent_table {
            Some(pt) => {
                out.push(1);
                write_str(&mut out, pt);
                write_u32_slice(&mut out, &t.parent_row_ids);
                write_u32_slice(&mut out, &t.csr_offsets);
            }
            None => out.push(0),
        }

        write_u32(&mut out, t.columns.len() as u32);
        for c in &t.columns {
            write_str(&mut out, &c.name);
            write_str_vec(&mut out, &c.source_path);
            out.push(if c.is_synthetic_count { 1 } else { 0 });
            let has_explicit_nulls = c.explicit_nulls.count_ones() > 0;
            out.push(if has_explicit_nulls { 1 } else { 0 });
            if has_explicit_nulls {
                write_u64_slice(&mut out, &c.explicit_nulls.words);
            }

            match &c.store {
                ColumnStore::Int(ic) => {
                    out.push(1);
                    out.push(if ic.nullable { 1 } else { 0 });
                    if ic.nullable {
                        write_u64_slice(&mut out, &ic.valid.words);
                    }
                    write_u32(&mut out, ic.non_null_count as u32);
                    out.extend_from_slice(&ic.min_val.to_le_bytes());
                    out.extend_from_slice(&ic.max_val.to_le_bytes());
                    out.extend_from_slice(&ic.sum_val.to_le_bytes());
                    out.push(ic.bit_width);
                    write_u64_slice(&mut out, &ic.packed_deltas);
                }
                ColumnStore::Float(fc) => {
                    out.push(2);
                    out.push(if fc.nullable { 1 } else { 0 });
                    if fc.nullable {
                        write_u64_slice(&mut out, &fc.valid.words);
                    }
                    write_u32(&mut out, fc.non_null_count as u32);
                    out.extend_from_slice(&fc.min_val.to_le_bytes());
                    out.extend_from_slice(&fc.max_val.to_le_bytes());
                    out.extend_from_slice(&fc.sum_val.to_le_bytes());
                    for &v in &fc.raw_values {
                        out.extend_from_slice(&v.to_le_bytes());
                    }
                }
                ColumnStore::Bool(bc) => {
                    out.push(3);
                    out.push(if bc.nullable { 1 } else { 0 });
                    if bc.nullable {
                        write_u64_slice(&mut out, &bc.valid.words);
                    }
                    write_u64_slice(&mut out, &bc.true_bits.words);
                }
                ColumnStore::DictStr(dc) => {
                    out.push(4);
                    out.push(if dc.nullable { 1 } else { 0 });
                    if dc.nullable {
                        write_u64_slice(&mut out, &dc.valid.words);
                    }
                    out.push(if dc.is_primitive_array { 1 } else { 0 });
                    let has_non_str = dc.non_string_json.count_ones() > 0;
                    out.push(if has_non_str { 1 } else { 0 });
                    if has_non_str {
                        write_u64_slice(&mut out, &dc.non_string_json.words);
                    }
                    write_str_vec(&mut out, &dc.dictionary);
                    out.push(dc.bit_width);
                    write_u64_slice(&mut out, &dc.packed_ids);
                }
            }
        }
    }

    out
}

/// Deserializes a `.jsonbit` binary snapshot directly back into a `JsonOlapEngine`
/// without parsing JSON text.
pub fn deserialize_snapshot(bytes: &[u8]) -> Result<JsonOlapEngine, String> {
    if bytes.len() < 5 || &bytes[..5] != MAGIC {
        return Err("Invalid .jsonbit binary header".to_string());
    }
    let mut cur = Cursor::new(&bytes[5..]);
    let table_count = read_u32(&mut cur)? as usize;
    let mut tables = Vec::with_capacity(table_count);

    for _ in 0..table_count {
        let name = read_str(&mut cur)?;
        let json_path = read_str_vec(&mut cur)?;
        let primary_key = read_str(&mut cur)?;
        let is_keyed_map = read_u8(&mut cur)? != 0;
        let is_singleton = read_u8(&mut cur)? != 0;
        let row_count = read_u32(&mut cur)? as usize;
        let top_level_fields = read_str_vec(&mut cur)?;

        let has_parent = read_u8(&mut cur)? != 0;
        let (parent_table, parent_row_ids, csr_offsets) = if has_parent {
            let pt = read_str(&mut cur)?;
            let pids = read_u32_vec(&mut cur)?;
            let csr = read_u32_vec(&mut cur)?;
            (Some(pt), pids, csr)
        } else {
            (None, Vec::new(), Vec::new())
        };

        let col_count = read_u32(&mut cur)? as usize;
        let mut columns = Vec::with_capacity(col_count);
        let mut col_index = std::collections::HashMap::new();

        for idx in 0..col_count {
            let col_name = read_str(&mut cur)?;
            let source_path = read_str_vec(&mut cur)?;
            let is_synthetic_count = read_u8(&mut cur)? != 0;
            let has_explicit_nulls = read_u8(&mut cur)? != 0;
            let explicit_nulls = if has_explicit_nulls {
                BitVec64 {
                    words: read_u64_vec(&mut cur)?,
                    len: row_count,
                }
            } else {
                BitVec64::new_empty(row_count)
            };
            let tag = read_u8(&mut cur)?;

            let store = match tag {
                1 => {
                    let nullable = read_u8(&mut cur)? != 0;
                    let valid = if nullable {
                        BitVec64 {
                            words: read_u64_vec(&mut cur)?,
                            len: row_count,
                        }
                    } else {
                        BitVec64::new_all_ones(row_count)
                    };
                    let non_null_count = read_u32(&mut cur)? as usize;
                    let min_val = read_i64(&mut cur)?;
                    let max_val = read_i64(&mut cur)?;
                    let sum_val = read_i128(&mut cur)?;
                    let bit_width = read_u8(&mut cur)?;
                    let packed_deltas = read_u64_vec(&mut cur)?;
                    let mut raw_values = vec![0i64; row_count];
                    for r in 0..row_count {
                        if valid.get(r) {
                            let delta = crate::column::unpack_bit(&packed_deltas, r, bit_width);
                            raw_values[r] = (min_val as i128 + delta as i128) as i64;
                        }
                    }
                    ColumnStore::Int(ForIntColumn {
                        len: row_count,
                        nullable,
                        valid,
                        non_null_count,
                        min_val,
                        max_val,
                        sum_val,
                        bit_width,
                        packed_deltas,
                        raw_values,
                    })
                }
                2 => {
                    let nullable = read_u8(&mut cur)? != 0;
                    let valid = if nullable {
                        BitVec64 {
                            words: read_u64_vec(&mut cur)?,
                            len: row_count,
                        }
                    } else {
                        BitVec64::new_all_ones(row_count)
                    };
                    let non_null_count = read_u32(&mut cur)? as usize;
                    let min_val = read_f64(&mut cur)?;
                    let max_val = read_f64(&mut cur)?;
                    let sum_val = read_f64(&mut cur)?;
                    let mut raw_values = vec![0.0f64; row_count];
                    for r in 0..row_count {
                        raw_values[r] = read_f64(&mut cur)?;
                    }
                    ColumnStore::Float(FloatColumn {
                        len: row_count,
                        nullable,
                        valid,
                        non_null_count,
                        min_val,
                        max_val,
                        sum_val,
                        raw_values,
                        packed_byte_size: row_count * 4,
                    })
                }
                3 => {
                    let nullable = read_u8(&mut cur)? != 0;
                    let valid = if nullable {
                        BitVec64 {
                            words: read_u64_vec(&mut cur)?,
                            len: row_count,
                        }
                    } else {
                        BitVec64::new_all_ones(row_count)
                    };
                    let true_bits = BitVec64 {
                        words: read_u64_vec(&mut cur)?,
                        len: row_count,
                    };
                    ColumnStore::Bool(BoolBitmapColumn {
                        len: row_count,
                        nullable,
                        valid,
                        true_bits,
                    })
                }
                4 => {
                    let nullable = read_u8(&mut cur)? != 0;
                    let valid = if nullable {
                        BitVec64 {
                            words: read_u64_vec(&mut cur)?,
                            len: row_count,
                        }
                    } else {
                        BitVec64::new_all_ones(row_count)
                    };
                    let is_primitive_array = read_u8(&mut cur)? != 0;
                    let has_non_str = read_u8(&mut cur)? != 0;
                    let non_string_json = if has_non_str {
                        BitVec64 {
                            words: read_u64_vec(&mut cur)?,
                            len: row_count,
                        }
                    } else {
                        BitVec64::new_empty(row_count)
                    };
                    let dictionary = read_str_vec(&mut cur)?;
                    let bit_width = read_u8(&mut cur)?;
                    let packed_ids = read_u64_vec(&mut cur)?;

                    let mut value_to_id = std::collections::HashMap::new();
                    for (id, s) in dictionary.iter().enumerate() {
                        value_to_id.insert(s.clone(), id);
                    }
                    let mut inverted_index = std::collections::HashMap::new();
                    if !dictionary.is_empty() && dictionary.len() <= 64 {
                        for id in 0..dictionary.len() {
                            inverted_index.insert(id, BitVec64::new_empty(row_count));
                        }
                        for r in 0..row_count {
                            if valid.get(r) {
                                let id = crate::column::unpack_bit(&packed_ids, r, bit_width) as usize;
                                if let Some(bm) = inverted_index.get_mut(&id) {
                                    bm.set(r);
                                }
                            }
                        }
                    }
                    let raw_dict_bytes: usize = dictionary.iter().map(|s| s.len() + 2).sum();
                    ColumnStore::DictStr(DictStringColumn {
                        len: row_count,
                        nullable,
                        valid,
                        is_primitive_array,
                        non_string_json,
                        dictionary,
                        value_to_id,
                        bit_width,
                        packed_ids,
                        inverted_index,
                        keyword_index: std::collections::HashMap::new(),
                        token_compressed_dict_bytes: raw_dict_bytes,
                    })
                }
                other => return Err(format!("Unknown column tag: {}", other)),
            };

            col_index.insert(col_name.clone(), idx);
            columns.push(ColumnMeta {
                name: col_name,
                source_path,
                is_synthetic_count,
                explicit_nulls,
                store,
            });
        }

        tables.push(ColumnarTable {
            name,
            json_path,
            primary_key,
            is_keyed_map,
            is_singleton,
            row_count,
            top_level_fields,
            columns,
            col_index,
            parent_table,
            parent_row_ids,
            csr_offsets,
        });
    }

    Ok(JsonOlapEngine { tables })
}

pub fn save_snapshot_file(engine: &JsonOlapEngine, path: &str) -> Result<usize, String> {
    let bytes = serialize_snapshot(engine);
    let len = bytes.len();
    fs::write(path, bytes).map_err(|e| format!("Failed to write {}: {}", path, e))?;
    Ok(len)
}

pub fn load_snapshot_file(path: &str) -> Result<JsonOlapEngine, String> {
    let bytes = fs::read(path).map_err(|e| format!("Failed to read {}: {}", path, e))?;
    deserialize_snapshot(&bytes)
}

pub fn load_json_or_jolap(path: &str) -> Result<JsonOlapEngine, String> {
    let bytes = fs::read(path).map_err(|e| format!("Failed to read {}: {}", path, e))?;
    if bytes.len() >= 5 && &bytes[..5] == MAGIC {
        return deserialize_snapshot(&bytes);
    }
    let raw = String::from_utf8(bytes).map_err(|e| format!("Invalid UTF-8: {}", e))?;
    let val: Value = crate::mcp::parse_json_or_jsonl(&raw);
    Ok(JsonOlapEngine::from_json(&val))
}

fn write_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    write_u32(out, b.len() as u32);
    out.extend_from_slice(b);
}

fn write_str_vec(out: &mut Vec<u8>, v: &[String]) {
    write_u32(out, v.len() as u32);
    for s in v {
        write_str(out, s);
    }
}

fn write_u32_slice(out: &mut Vec<u8>, v: &[u32]) {
    write_u32(out, v.len() as u32);
    for &x in v {
        write_u32(out, x);
    }
}

fn write_u64_slice(out: &mut Vec<u8>, v: &[u64]) {
    write_u32(out, v.len() as u32);
    for &x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
}

fn read_u8(cur: &mut Cursor<&[u8]>) -> Result<u8, String> {
    let mut b = [0u8; 1];
    cur.read_exact(&mut b).map_err(|e| e.to_string())?;
    Ok(b[0])
}

fn read_u32(cur: &mut Cursor<&[u8]>) -> Result<u32, String> {
    let mut b = [0u8; 4];
    cur.read_exact(&mut b).map_err(|e| e.to_string())?;
    Ok(u32::from_le_bytes(b))
}

fn read_i64(cur: &mut Cursor<&[u8]>) -> Result<i64, String> {
    let mut b = [0u8; 8];
    cur.read_exact(&mut b).map_err(|e| e.to_string())?;
    Ok(i64::from_le_bytes(b))
}

fn read_i128(cur: &mut Cursor<&[u8]>) -> Result<i128, String> {
    let mut b = [0u8; 16];
    cur.read_exact(&mut b).map_err(|e| e.to_string())?;
    Ok(i128::from_le_bytes(b))
}

fn read_f64(cur: &mut Cursor<&[u8]>) -> Result<f64, String> {
    let mut b = [0u8; 8];
    cur.read_exact(&mut b).map_err(|e| e.to_string())?;
    Ok(f64::from_le_bytes(b))
}

fn read_str(cur: &mut Cursor<&[u8]>) -> Result<String, String> {
    let len = read_u32(cur)? as usize;
    let mut b = vec![0u8; len];
    cur.read_exact(&mut b).map_err(|e| e.to_string())?;
    String::from_utf8(b).map_err(|e| e.to_string())
}

fn read_str_vec(cur: &mut Cursor<&[u8]>) -> Result<Vec<String>, String> {
    let len = read_u32(cur)? as usize;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        out.push(read_str(cur)?);
    }
    Ok(out)
}

fn read_u32_vec(cur: &mut Cursor<&[u8]>) -> Result<Vec<u32>, String> {
    let len = read_u32(cur)? as usize;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        out.push(read_u32(cur)?);
    }
    Ok(out)
}

fn read_u64_vec(cur: &mut Cursor<&[u8]>) -> Result<Vec<u64>, String> {
    let len = read_u32(cur)? as usize;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        let mut b = [0u8; 8];
        cur.read_exact(&mut b).map_err(|e| e.to_string())?;
        out.push(u64::from_le_bytes(b));
    }
    Ok(out)
}
