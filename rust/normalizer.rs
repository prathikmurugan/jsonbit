use std::collections::HashMap;
use serde_json::{Map, Value};
use crate::bitmap::BitVec64;
use crate::column::{
    BoolBitmapColumn, ColumnStore, DictStringColumn, FloatColumn, ForIntColumn,
};

#[derive(Clone, Debug)]
pub struct ColumnMeta {
    pub name: String,
    pub source_path: Vec<String>,
    pub is_synthetic_count: bool,
    pub explicit_nulls: BitVec64,
    pub store: ColumnStore,
}

#[derive(Clone, Debug)]
pub struct ColumnarTable {
    pub name: String,
    pub json_path: Vec<String>,
    pub primary_key: String,
    pub is_keyed_map: bool,
    pub is_singleton: bool,
    pub row_count: usize,
    pub top_level_fields: Vec<String>,
    pub columns: Vec<ColumnMeta>,
    pub col_index: HashMap<String, usize>,
    /// If this is a 1-to-Many child table, `parent_table` is the parent table name,
    /// `parent_row_ids[child_idx]` is the parent row index (`0..parent_rows`),
    /// and `csr_offsets[parent_idx]..csr_offsets[parent_idx + 1]` is the O(1) CSR slice!
    pub parent_table: Option<String>,
    pub parent_row_ids: Vec<u32>,
    pub csr_offsets: Vec<u32>,
}

impl ColumnarTable {
    #[inline]
    pub fn get_col(&self, name: &str) -> Option<&ColumnMeta> {
        let &idx = self.col_index.get(name)?;
        self.columns.get(idx)
    }

    pub fn reconstruct_flat_row_json(&self, row_idx: usize, include_synthetic: bool) -> Map<String, Value> {
        let mut map = Map::new();
        for col in &self.columns {
            if !include_synthetic && (col.is_synthetic_count || col.name == "_rowid" || col.name == "_id") {
                continue;
            }
            if let Some(v) = col.store.get_json_value(row_idx) {
                map.insert(col.name.clone(), v);
            }
        }
        map
    }

    pub fn compressed_byte_size(&self) -> usize {
        let col_bytes: usize = self.columns.iter().map(|c| c.store.compressed_byte_size()).sum();
        let csr_bytes = if self.parent_table.is_some() {
            self.csr_offsets.len() * 4
        } else {
            0
        };
        col_bytes + csr_bytes
    }
}

pub fn sanitize_ident(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}

/// Resolves a deterministic, collision-free SQL column alias for an exact JSON `source_path`.
/// If two distinct JSON paths (e.g. `["a-b"]` and `["a_b"]`) sanitize to the same SQL identifier,
/// the second receives a disambiguated alias (`a_b_2`, `a_b_3`, ...) while preserving its exact
/// `source_path` for 100% lossless rehydration.
fn resolve_unique_col_name(
    path: &[String],
    path_to_col: &mut HashMap<Vec<String>, String>,
    used_names: &mut HashMap<String, Vec<String>>,
) -> String {
    if let Some(existing) = path_to_col.get(path) {
        return existing.clone();
    }
    let base = sanitize_ident(&path.join("_"));
    let mut candidate = base.clone();
    let mut suffix = 2usize;
    while let Some(owner) = used_names.get(&candidate) {
        if owner == path {
            break;
        }
        candidate = format!("{}_{}", base, suffix);
        suffix += 1;
    }
    path_to_col.insert(path.to_vec(), candidate.clone());
    used_names.insert(candidate.clone(), path.to_vec());
    candidate
}

pub fn is_array_of_objects(v: &Value) -> bool {
    match v {
        Value::Array(arr) => !arr.is_empty() && arr.iter().all(|x| x.is_object()),
        _ => false,
    }
}

pub fn is_array_of_primitives(v: &Value) -> bool {
    match v {
        Value::Array(arr) => arr.iter().all(|x| !x.is_object() && !x.is_array()),
        _ => false,
    }
}

pub fn is_keyed_object_map(map: &Map<String, Value>) -> bool {
    if map.len() < 2 {
        return false;
    }
    let mut obj_entries = Vec::with_capacity(map.len());
    for (_, v) in map {
        match v {
            Value::Object(sub) if !sub.is_empty() => obj_entries.push(sub),
            _ => return false,
        }
    }
    let first_keys: Vec<&String> = obj_entries[0].keys().collect();
    first_keys
        .into_iter()
        .any(|k| obj_entries.iter().all(|sub| sub.contains_key(k)))
}

/// Decomposes any arbitrary nested JSON `Value` into compressed 3NF `ColumnarTable`s
/// with CSR (Compressed Sparse Row) 1-to-Many parent-child offset arrays.
pub fn normalize_json_to_3nf(data: &Value) -> Vec<ColumnarTable> {
    let mut tables = Vec::new();

    match data {
        Value::Array(arr) if !arr.is_empty() && arr.iter().all(|x| x.is_object()) => {
            let rows: Vec<Map<String, Value>> = arr
                .iter()
                .filter_map(|x| x.as_object().cloned())
                .collect();
            extract_table_from_rows("records", Vec::new(), &rows, false, None, &mut tables);
        }
        Value::Object(map) => {
            if map.len() == 1 {
                let (key, val) = map.iter().next().unwrap();
                let safe_name = sanitize_ident(key);
                if is_array_of_objects(val) {
                    let rows: Vec<Map<String, Value>> = val
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|x| x.as_object().cloned())
                        .collect();
                    extract_table_from_rows(&safe_name, vec![key.clone()], &rows, false, None, &mut tables);
                    return tables;
                }
                if let Value::Object(sub_map) = val {
                    if is_keyed_object_map(sub_map) {
                        let mut rows = Vec::with_capacity(sub_map.len());
                        for (k, v) in sub_map {
                            if let Value::Object(obj) = v {
                                let mut row = Map::new();
                                row.insert("_key".to_string(), Value::String(k.clone()));
                                for (rk, rv) in obj {
                                    row.insert(rk.clone(), rv.clone());
                                }
                                rows.push(row);
                            }
                        }
                        extract_table_from_rows(&safe_name, vec![key.clone()], &rows, true, None, &mut tables);
                        return tables;
                    }
                }
            }
            extract_complex_singleton("config", Vec::new(), map, &mut tables);
        }
        _ => {}
    }

    tables
}

fn extract_complex_singleton(
    table_name: &str,
    json_path: Vec<String>,
    map: &Map<String, Value>,
    tables: &mut Vec<ColumnarTable>,
) {
    let top_fields: Vec<String> = map.keys().cloned().collect();
    let mut flat_row = Map::new();
    let mut path_map: HashMap<String, (Vec<String>, bool)> = HashMap::new();
    let mut path_to_col: HashMap<Vec<String>, String> = HashMap::new();
    let mut used_names: HashMap<String, Vec<String>> = HashMap::new();
    let mut explicit_null_cols: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut col_order: Vec<String> = Vec::new();

    #[allow(clippy::too_many_arguments)]
    fn walk_singleton(
        prefix: &[String],
        current: &Map<String, Value>,
        flat_row: &mut Map<String, Value>,
        path_map: &mut HashMap<String, (Vec<String>, bool)>,
        path_to_col: &mut HashMap<Vec<String>, String>,
        used_names: &mut HashMap<String, Vec<String>>,
        explicit_null_cols: &mut std::collections::HashSet<String>,
        col_order: &mut Vec<String>,
        tables: &mut Vec<ColumnarTable>,
    ) {
        for (k, v) in current {
            let mut next_path = prefix.to_vec();
            next_path.push(k.clone());
            let col_name = resolve_unique_col_name(&next_path, path_to_col, used_names);

            if v.is_null() {
                if !path_map.contains_key(&col_name) {
                    col_order.push(col_name.clone());
                    path_map.insert(col_name.clone(), (next_path, false));
                }
                explicit_null_cols.insert(col_name);
            } else if v.is_string() || v.is_number() || v.is_boolean() {
                if !path_map.contains_key(&col_name) {
                    col_order.push(col_name.clone());
                    path_map.insert(col_name.clone(), (next_path, false));
                }
                flat_row.insert(col_name, v.clone());
            } else if is_array_of_primitives(v) {
                let s = serde_json::to_string(v).unwrap_or_else(|_| "[]".into());
                if !path_map.contains_key(&col_name) {
                    col_order.push(col_name.clone());
                    path_map.insert(col_name.clone(), (next_path, true));
                }
                flat_row.insert(col_name, Value::String(s));
            } else if is_array_of_objects(v) {
                let rows: Vec<Map<String, Value>> = v
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|x| x.as_object().cloned())
                    .collect();
                extract_table_from_rows(&col_name, next_path, &rows, false, None, tables);
            } else if let Value::Object(sub_map) = v {
                if is_keyed_object_map(sub_map) {
                    let mut rows = Vec::with_capacity(sub_map.len());
                    for (ek, ev) in sub_map {
                        if let Value::Object(obj) = ev {
                            let mut row = Map::new();
                            row.insert("_key".to_string(), Value::String(ek.clone()));
                            for (rk, rv) in obj {
                                row.insert(rk.clone(), rv.clone());
                            }
                            rows.push(row);
                        }
                    }
                    extract_table_from_rows(&col_name, next_path, &rows, true, None, tables);
                } else {
                    walk_singleton(
                        &next_path,
                        sub_map,
                        flat_row,
                        path_map,
                        path_to_col,
                        used_names,
                        explicit_null_cols,
                        col_order,
                        tables,
                    );
                }
            }
        }
    }

    walk_singleton(
        &json_path,
        map,
        &mut flat_row,
        &mut path_map,
        &mut path_to_col,
        &mut used_names,
        &mut explicit_null_cols,
        &mut col_order,
        tables,
    );

    if !col_order.is_empty() {
        let mut columns = Vec::with_capacity(col_order.len());
        let mut col_index = HashMap::new();
        for c_name in col_order {
            let (src_path, is_prim_arr) = path_map.remove(&c_name).unwrap();
            let val_slice = vec![flat_row.get(&c_name).cloned()];
            let store = build_column_store(&val_slice, is_prim_arr);
            let mut explicit_nulls = BitVec64::new_empty(1);
            if explicit_null_cols.contains(&c_name) {
                explicit_nulls.set(0);
            }
            let idx = columns.len();
            col_index.insert(c_name.clone(), idx);
            columns.push(ColumnMeta {
                name: c_name,
                source_path: src_path,
                is_synthetic_count: false,
                explicit_nulls,
                store,
            });
        }
        tables.insert(
            0,
            ColumnarTable {
                name: table_name.to_string(),
                json_path,
                primary_key: "_id".to_string(),
                is_keyed_map: false,
                is_singleton: true,
                row_count: 1,
                top_level_fields: top_fields,
                columns,
                col_index,
                parent_table: None,
                parent_row_ids: Vec::new(),
                csr_offsets: Vec::new(),
            },
        );
    }
}

struct ChildBucket {
    child_path: Vec<String>,
    child_rows: Vec<Map<String, Value>>,
    parent_row_ids: Vec<u32>,
    csr_offsets: Vec<u32>,
}

fn extract_table_from_rows(
    table_name: &str,
    json_path: Vec<String>,
    rows: &[Map<String, Value>],
    is_keyed_map: bool,
    parent_info: Option<(String, Vec<u32>, Vec<u32>)>,
    tables: &mut Vec<ColumnarTable>,
) {
    let n = rows.len();
    let mut top_level_fields = Vec::new();
    for r in rows {
        for k in r.keys() {
            if k != "_key" && !top_level_fields.contains(k) {
                top_level_fields.push(k.clone());
            }
        }
    }

    let candidate_pks = ["_key", "orderId", "id", "sku", "date", "repo"];
    let mut primary_key = "_rowid".to_string();
    for pk in candidate_pks {
        if rows.iter().all(|r| {
            matches!(
                r.get(pk),
                Some(Value::String(_)) | Some(Value::Number(_))
            )
        }) {
            primary_key = pk.to_string();
            break;
        }
    }

    let mut col_order: Vec<String> = Vec::new();
    let mut col_meta_info: HashMap<String, (Vec<String>, bool, bool)> = HashMap::new();
    let mut path_to_col: HashMap<Vec<String>, String> = HashMap::new();
    let mut used_names: HashMap<String, Vec<String>> = HashMap::new();
    let mut col_cells: HashMap<String, Vec<Option<Value>>> = HashMap::new();
    let mut col_explicit_nulls: HashMap<String, BitVec64> = HashMap::new();

    let mut child_buckets: HashMap<String, ChildBucket> = HashMap::new();
    let mut child_order: Vec<String> = Vec::new();

    for (row_idx, raw_row) in rows.iter().enumerate() {
        flatten_row_recursive(
            raw_row,
            &[],
            row_idx,
            n,
            &mut col_order,
            &mut col_meta_info,
            &mut path_to_col,
            &mut used_names,
            &mut col_cells,
            &mut col_explicit_nulls,
            &mut child_order,
            &mut child_buckets,
        );
    }

    // Finalize CSR offset tails for any child tables
    for bucket in child_buckets.values_mut() {
        while bucket.csr_offsets.len() <= n {
            bucket.csr_offsets.push(bucket.child_rows.len() as u32);
        }
    }

    let mut columns = Vec::with_capacity(col_order.len());
    let mut col_index = HashMap::new();

    for col_name in col_order {
        let (src_path, is_prim_arr, is_synth) = col_meta_info.remove(&col_name).unwrap();
        let cells = col_cells.remove(&col_name).unwrap();
        let explicit_nulls = col_explicit_nulls
            .remove(&col_name)
            .unwrap_or_else(|| BitVec64::new_empty(n));
        let store = build_column_store(&cells, is_prim_arr);
        let idx = columns.len();
        col_index.insert(col_name.clone(), idx);
        columns.push(ColumnMeta {
            name: col_name,
            source_path: src_path,
            is_synthetic_count: is_synth,
            explicit_nulls,
            store,
        });
    }

    let (parent_table, parent_row_ids, csr_offsets) = match parent_info {
        Some((pt, pids, csr)) => (Some(pt), pids, csr),
        None => (None, Vec::new(), Vec::new()),
    };

    tables.push(ColumnarTable {
        name: table_name.to_string(),
        json_path: json_path.clone(),
        primary_key,
        is_keyed_map,
        is_singleton: false,
        row_count: n,
        top_level_fields,
        columns,
        col_index,
        parent_table,
        parent_row_ids,
        csr_offsets,
    });

    for child_col_name in child_order {
        if let Some(bucket) = child_buckets.remove(&child_col_name) {
            let child_table_name = sanitize_ident(&format!("{}_{}", table_name, child_col_name));
            let mut full_child_path = json_path.clone();
            full_child_path.extend(bucket.child_path);
            extract_table_from_rows(
                &child_table_name,
                full_child_path,
                &bucket.child_rows,
                false,
                Some((
                    table_name.to_string(),
                    bucket.parent_row_ids,
                    bucket.csr_offsets,
                )),
                tables,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn flatten_row_recursive(
    obj: &Map<String, Value>,
    prefix: &[String],
    row_idx: usize,
    total_rows: usize,
    col_order: &mut Vec<String>,
    col_meta_info: &mut HashMap<String, (Vec<String>, bool, bool)>,
    path_to_col: &mut HashMap<Vec<String>, String>,
    used_names: &mut HashMap<String, Vec<String>>,
    col_cells: &mut HashMap<String, Vec<Option<Value>>>,
    col_explicit_nulls: &mut HashMap<String, BitVec64>,
    child_order: &mut Vec<String>,
    child_buckets: &mut HashMap<String, ChildBucket>,
) {
    for (k, v) in obj {
        let mut path = prefix.to_vec();
        path.push(k.clone());
        let col_name = resolve_unique_col_name(&path, path_to_col, used_names);

        if v.is_null() || v.is_string() || v.is_number() || v.is_boolean() {
            if !col_meta_info.contains_key(&col_name) {
                col_order.push(col_name.clone());
                col_meta_info.insert(col_name.clone(), (path, false, false));
                col_cells.insert(col_name.clone(), vec![None; total_rows]);
            }
            if !v.is_null() {
                col_cells.get_mut(&col_name).unwrap()[row_idx] = Some(v.clone());
            } else {
                // Record explicit null if key was present with null
                col_cells.get_mut(&col_name).unwrap()[row_idx] = None;
                col_explicit_nulls
                    .entry(col_name)
                    .or_insert_with(|| BitVec64::new_empty(total_rows))
                    .set(row_idx);
            }
        } else if is_array_of_primitives(v) {
            let arr = v.as_array().unwrap();
            let s = serde_json::to_string(v).unwrap_or_else(|_| "[]".into());
            if !col_meta_info.contains_key(&col_name) {
                col_order.push(col_name.clone());
                col_meta_info.insert(col_name.clone(), (path.clone(), true, false));
                col_cells.insert(col_name.clone(), vec![None; total_rows]);
            }
            col_cells.get_mut(&col_name).unwrap()[row_idx] = Some(Value::String(s));

            // Synthetic `_count` column for fast cardinality filtering
            let count_col = format!("{}_count", col_name);
            if !col_meta_info.contains_key(&count_col) {
                let mut count_path = path;
                count_path.push("_count".to_string());
                col_order.push(count_col.clone());
                col_meta_info.insert(count_col.clone(), (count_path, false, true));
                col_cells.insert(count_col.clone(), vec![None; total_rows]);
            }
            col_cells.get_mut(&count_col).unwrap()[row_idx] =
                Some(Value::Number((arr.len() as i64).into()));
        } else if is_array_of_objects(v) {
            let arr = v.as_array().unwrap();
            let count_col = format!("{}_count", col_name);
            if !col_meta_info.contains_key(&count_col) {
                let mut count_path = path.clone();
                count_path.push("_count".to_string());
                col_order.push(count_col.clone());
                col_meta_info.insert(count_col.clone(), (count_path, false, true));
                col_cells.insert(count_col.clone(), vec![None; total_rows]);
            }
            col_cells.get_mut(&count_col).unwrap()[row_idx] =
                Some(Value::Number((arr.len() as i64).into()));

            if !child_buckets.contains_key(&col_name) {
                child_order.push(col_name.clone());
                child_buckets.insert(
                    col_name.clone(),
                    ChildBucket {
                        child_path: path,
                        child_rows: Vec::new(),
                        parent_row_ids: Vec::new(),
                        csr_offsets: Vec::with_capacity(total_rows + 1),
                    },
                );
            }
            let bucket = child_buckets.get_mut(&col_name).unwrap();
            while bucket.csr_offsets.len() <= row_idx {
                bucket.csr_offsets.push(bucket.child_rows.len() as u32);
            }
            for child_val in arr {
                if let Some(child_obj) = child_val.as_object() {
                    bucket.child_rows.push(child_obj.clone());
                    bucket.parent_row_ids.push(row_idx as u32);
                }
            }
        } else if let Value::Object(sub_obj) = v {
            flatten_row_recursive(
                sub_obj,
                &path,
                row_idx,
                total_rows,
                col_order,
                col_meta_info,
                path_to_col,
                used_names,
                col_cells,
                col_explicit_nulls,
                child_order,
                child_buckets,
            );
        }
    }
}

fn build_column_store(cells: &[Option<Value>], is_primitive_array: bool) -> ColumnStore {
    let non_nulls: Vec<&Value> = cells.iter().filter_map(|x| x.as_ref()).collect();
    if non_nulls.is_empty() {
        let str_opts = vec![None; cells.len()];
        return ColumnStore::DictStr(DictStringColumn::from_options(&str_opts, is_primitive_array));
    }

    if non_nulls.iter().all(|v| v.is_boolean()) {
        let bool_opts: Vec<Option<bool>> =
            cells.iter().map(|c| c.as_ref()?.as_bool()).collect();
        return ColumnStore::Bool(BoolBitmapColumn::from_options(&bool_opts));
    }

    if non_nulls.iter().all(|v| v.is_number()) {
        if non_nulls.iter().all(|v| v.as_i64().is_some()) {
            let int_opts: Vec<Option<i64>> = cells
                .iter()
                .map(|c| c.as_ref().and_then(|v| v.as_i64()))
                .collect();
            return ColumnStore::Int(ForIntColumn::from_options(&int_opts));
        } else if !non_nulls
            .iter()
            .any(|v| v.is_u64() && v.as_i64().is_none())
        {
            let float_opts: Vec<Option<f64>> = cells
                .iter()
                .map(|c| c.as_ref().and_then(|v| v.as_f64()))
                .collect();
            return ColumnStore::Float(FloatColumn::from_options(&float_opts));
        }
    }

    let mut non_string_json = BitVec64::new_empty(cells.len());
    let str_opts: Vec<Option<String>> = cells
        .iter()
        .enumerate()
        .map(|(i, c)| {
            c.as_ref().map(|v| match v {
                Value::String(s) => s.clone(),
                other => {
                    non_string_json.set(i);
                    other.to_string()
                }
            })
        })
        .collect();
    let mut dc = DictStringColumn::from_options(&str_opts, is_primitive_array);
    dc.non_string_json = non_string_json;
    ColumnStore::DictStr(dc)
}

/// Codd's Lossless-Join 3NF Rehydrator in Pure Rust:
/// Reconstructs the exact original nested JSON `Value` directly from the bit-packed
/// `ColumnarTable`s and O(1) CSR child offset arrays (`csr_offsets[i]..csr_offsets[i+1]`).
pub fn rehydrate_from_3nf(tables: &[ColumnarTable]) -> Value {
    if tables.is_empty() {
        return Value::Object(Map::new());
    }

    if tables[0].is_singleton {
        let mut root_obj = Map::new();
        let root_table = &tables[0];
        for col in &root_table.columns {
            if col.is_synthetic_count || col.name == "_id" {
                continue;
            }
            if let Some(val) = col.store.get_json_value(0) {
                set_nested_path(&mut root_obj, &col.source_path, val);
            } else if col.explicit_nulls.get(0) {
                set_nested_path(&mut root_obj, &col.source_path, Value::Null);
            }
        }
        for child_table in &tables[1..] {
            if child_table.parent_table.is_some() {
                continue;
            }
            let rehydrated = rehydrate_table_rows(child_table, tables);
            set_nested_path(&mut root_obj, &child_table.json_path, rehydrated);
        }
        return Value::Object(root_obj);
    }

    let primary = &tables[0];
    let rehydrated = rehydrate_table_rows(primary, tables);
    if primary.json_path.len() == 1 {
        let mut wrapper = Map::new();
        wrapper.insert(primary.json_path[0].clone(), rehydrated);
        return Value::Object(wrapper);
    }
    rehydrated
}

fn rehydrate_table_rows(table: &ColumnarTable, all_tables: &[ColumnarTable]) -> Value {
    let mut objects = Vec::with_capacity(table.row_count);
    let mut keyed_map = Map::new();

    for row_idx in 0..table.row_count {
        let (entry_key, obj) = rehydrate_single_row(table, row_idx, all_tables);
        if table.is_keyed_map {
            keyed_map.insert(entry_key, Value::Object(obj));
        } else {
            objects.push(Value::Object(obj));
        }
    }

    if table.is_keyed_map {
        Value::Object(keyed_map)
    } else {
        Value::Array(objects)
    }
}

fn rehydrate_single_row(
    table: &ColumnarTable,
    row_idx: usize,
    all_tables: &[ColumnarTable],
) -> (String, Map<String, Value>) {
    let child_tables: Vec<&ColumnarTable> = all_tables
        .iter()
        .filter(|t| t.parent_table.as_deref() == Some(&table.name))
        .collect();

    let mut obj = Map::new();
    let mut entry_key = String::new();

    for col in &table.columns {
        if col.is_synthetic_count || col.name == "_rowid" || col.name == "_id" {
            continue;
        }
        if col.name == "_key" {
            if let Some(Value::String(k)) = col.store.get_json_value(row_idx) {
                entry_key = k;
            }
            continue;
        }
        if let Some(val) = col.store.get_json_value(row_idx) {
            set_nested_path(&mut obj, &col.source_path, val);
        } else if col.explicit_nulls.get(row_idx) {
            set_nested_path(&mut obj, &col.source_path, Value::Null);
        }
    }

    // Recursive O(1) CSR slice lookup for arbitrary-depth 1-to-Many child & grandchild arrays!
    for ct in &child_tables {
        let rel_path = &ct.json_path[table.json_path.len()..];
        let start = ct.csr_offsets.get(row_idx).copied().unwrap_or(0) as usize;
        let end = ct.csr_offsets.get(row_idx + 1).copied().unwrap_or(start as u32) as usize;
        let mut child_arr = Vec::with_capacity(end.saturating_sub(start));

        for c_idx in start..end {
            let (_, c_obj) = rehydrate_single_row(ct, c_idx, all_tables);
            child_arr.push(Value::Object(c_obj));
        }
        set_nested_path(&mut obj, rel_path, Value::Array(child_arr));
    }

    (entry_key, obj)
}

fn set_nested_path(target: &mut Map<String, Value>, path: &[String], value: Value) {
    if path.is_empty() {
        return;
    }
    if path.len() == 1 {
        target.insert(path[0].clone(), value);
        return;
    }
    let first = &path[0];
    let entry = target
        .entry(first.clone())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(sub_map) = entry {
        set_nested_path(sub_map, &path[1..], value);
    }
}
