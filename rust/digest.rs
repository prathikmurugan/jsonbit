use std::collections::{HashMap, HashSet};

use crate::column::{unpack_bit, CmpOp, ColumnStore, DictStringColumn, ForIntColumn};
use crate::engine::JsonOlapEngine;
use crate::normalizer::{ColumnMeta, ColumnarTable};

/// Column-level lossless encoding plan selected automatically per column.
#[derive(Debug, Clone)]
enum ColEncoding {
    /// Emitted verbatim (with optional global `@dict` substitution).
    Raw,
    /// Constant across all rows: hoisted to `@const`, omitted from row cells.
    Const(String),
    /// Strictly monotonic integer sequence `start + i * step`: hoisted, omitted from rows.
    IntSeq { start: i64, step: i64 },
    /// Strictly monotonic `prefix + (start + i * step)` string sequence: hoisted, omitted from rows.
    StrSeq {
        prefix: String,
        start: i64,
        step: i64,
        zero_pad: usize,
    },
    /// Multi-part row-index template `p0{i}p1{i}p2`: hoisted, omitted from rows.
    StrTemplate {
        parts: Vec<String>,
        start: i64,
    },
    /// Dominant default value (>= 88% identical, <= 24 exceptions): hoisted with `except[i:v,...]`.
    DominantDefault {
        default_val: String,
        exceptions: Vec<(usize, String)>,
    },
    /// All non-null integers divisible by `scale` (`1_000` or `1_000_000`).
    IntScale { scale: i64, label: &'static str },
    /// Column-local single-letter BPE-optimal enum codes (`a`..`z`).
    LocalEnum {
        /// Original string -> single-char code (`a`..`z`)
        map: HashMap<String, char>,
        /// Ordered legend entries `(code, value)` sorted by frequency desc
        legend: Vec<(char, String)>,
    },
    /// Cross-column functional dependency: `B = */{sep}$A` (suffix equals another active column).
    CrossSuffix {
        ref_col_idx: usize,
        ref_col_name: String,
        sep: String,
        allow_eq_shorthand: bool,
    },
    /// Cross-column functional dependency: `B = $A{sep}*` (prefix equals another active column).
    CrossPrefix {
        ref_col_idx: usize,
        ref_col_name: String,
        sep: String,
    },
    /// Strict 100% common prefix and/or common suffix stripping.
    Affix {
        prefix: Option<String>,
        suffix: Option<String>,
    },
    /// Majority prefix (>= 75% match); non-matching cells prefixed with `^`.
    MajorityPrefix {
        prefix: String,
        suffix: Option<String>,
    },
}

impl ColEncoding {
    fn is_hoisted(&self) -> bool {
        matches!(
            self,
            ColEncoding::Const(_)
                | ColEncoding::IntSeq { .. }
                | ColEncoding::StrSeq { .. }
                | ColEncoding::StrTemplate { .. }
                | ColEncoding::DominantDefault { .. }
        )
    }
}

/// Generates a 1-Turn Factorized, BPE-Aware 3NF Holistic Digest (`olap_digest` / `jsonbit digest`)
/// containing 100% of every row and field across all 3NF tables PLUS compact OLAP stats,
/// using 7 general-purpose lossless compression stages:
/// 1. Constant & Dominant-Default Column Hoisting (`@const` / `except[i:v,...]`)
/// 2. Monotonic Sequence & Multi-Part Template Factoring (`seq(...)`, `tpl(...)`)
/// 3. Cross-Column Functional Dependency Factoring (`col=*/$other`, `col=$other/*`)
/// 4. Numeric Power-of-10 Scale Factoring (`*1k`, `*1M`)
/// 5. BPE-Optimal Single-Letter Column-Local Enum Codes (`a..z`) + Anti-Expansion Guard
/// 6. Strict/Majority Prefix & Common Suffix Stripping + Pruned `@dict`
/// 7. Intra-Row RLE (`v*k`) & Arithmetic-Progression (`a..b`, `a..b(+d)`) Folding on wide tables
pub fn generate_holistic_digest(engine: &JsonOlapEngine) -> String {
    let total_rows: usize = engine.tables.iter().map(|t| t.row_count).sum();
    if total_rows <= 8 {
        return engine.data_card();
    }

    // Phase 1: Analyze per-table column encodings up front so global `@dict` only interns
    // strings that actually survive into row cells and strictly save tokens.
    let mut table_plans: Vec<TablePlan> = Vec::with_capacity(engine.tables.len());
    for table in &engine.tables {
        table_plans.push(analyze_table(table));
    }

    // Phase 2: Build pruned global `@dict` ONLY for remaining `Raw` string columns with
    // long repeated strings (`len >= 8`, `cnt >= 4`, `len * cnt >= 48`) that weren't
    // already hoisted, enum-coded, or affix-factored.
    let mut dict_counter = 0usize;
    let mut global_dict: HashMap<String, String> = HashMap::new();
    let mut dict_legend: Vec<String> = Vec::new();

    for (t_idx, table) in engine.tables.iter().enumerate() {
        let plan = &table_plans[t_idx];
        // Precompute which boolean columns in this table are non-constant so we never emit
        // redundant `captured=64T/0F` cross-products on 100%-constant booleans.
        let non_const_bools: HashSet<&str> = table
            .columns
            .iter()
            .filter_map(|c| {
                if let ColumnStore::Bool(bc) = &c.store {
                    let valid_cnt = bc.valid.count_ones();
                    let t_cnt = bc.valid.and(&bc.true_bits).count_ones();
                    if t_cnt > 0 && t_cnt < valid_cnt {
                        return Some(c.name.as_str());
                    }
                }
                None
            })
            .collect();

        for (col_idx, enc) in plan.col_encodings.iter().enumerate() {
            if !matches!(enc, ColEncoding::Raw) {
                continue;
            }
            let col = &table.columns[col_idx];
            if col.is_synthetic_count || col.name == "_rowid" || col.name == "_id" {
                continue;
            }
            if let ColumnStore::DictStr(dc) = &col.store {
                let counts = count_dict_ids(dc, table.row_count);
                for (id, s) in dc.dictionary.iter().enumerate() {
                    let cnt = counts[id];
                    // BPE Anti-Expansion Guard: `$0` is 2 tokens in o200k_base, so only
                    // intern strings of len >= 8 (typically >= 2-4 tokens) appearing >= 4 times.
                    let is_internable = s.len() >= 8 && cnt >= 4 && (s.len() * cnt) >= 48;
                    if !is_internable {
                        continue;
                    }
                    let label = if let Some(existing) = global_dict.get(s) {
                        existing.clone()
                    } else {
                        let code = format!("${}", dict_counter);
                        dict_counter += 1;
                        global_dict.insert(s.clone(), code.clone());
                        format!("{}={}", code, escape_dict_value(s))
                    };

                    let mut pivot_notes = vec![format!("n={}", cnt)];
                    if dc.dictionary.len() <= 8 {
                        let bm = if let Some(b) = dc.inverted_index.get(&id) {
                            b.clone()
                        } else {
                            dc.filter_eq_case_insensitive(s)
                        };
                        for ocol in &table.columns {
                            if ocol.is_synthetic_count
                                || ocol.name == col.name
                                || !non_const_bools.contains(ocol.name.as_str())
                            {
                                continue;
                            }
                            if let ColumnStore::Bool(bc) = &ocol.store {
                                let valid_bm = bm.and(&bc.valid);
                                let t_cnt = valid_bm.and(&bc.true_bits).count_ones();
                                let f_cnt = valid_bm.count_ones().saturating_sub(t_cnt);
                                pivot_notes.push(format!("{}={}T/{}F", ocol.name, t_cnt, f_cnt));
                            }
                        }
                    }
                    dict_legend.push(format!("{}({})", label, pivot_notes.join(",")));
                }
            }
        }
    }

    let mut lines = Vec::new();
    if !dict_legend.is_empty() {
        lines.push(format!("@dict: {}", dict_legend.join(" ")));
    }

    let has_global_dict = !global_dict.is_empty();

    // Phase 3: Emit each table
    for (t_idx, table) in engine.tables.iter().enumerate() {
        let n = table.row_count;
        if n == 0 {
            continue;
        }
        let plan = &table_plans[t_idx];
        let is_child_table = table.parent_table.is_some();

        // Collect hoisted metadata & active (emitted) columns
        let mut hoist_parts: Vec<String> = Vec::new();
        let mut active_indices: Vec<usize> = Vec::new();

        for (col_idx, col) in table.columns.iter().enumerate() {
            if col.is_synthetic_count || col.name == "_rowid" || col.name == "_id" {
                continue;
            }
            match &plan.col_encodings[col_idx] {
                ColEncoding::Const(v) => {
                    hoist_parts.push(format!("{}={}", col.name, v));
                }
                ColEncoding::IntSeq { start, step } => {
                    let sign = if *step >= 0 { "+" } else { "" };
                    hoist_parts.push(format!("{}=seq({},{}{})", col.name, start, sign, step));
                }
                ColEncoding::StrSeq {
                    prefix,
                    start,
                    step,
                    zero_pad,
                } => {
                    let sign = if *step >= 0 { "+" } else { "" };
                    let start_str = if *zero_pad > 0 {
                        format!("{:0width$}", start, width = *zero_pad)
                    } else {
                        start.to_string()
                    };
                    hoist_parts.push(format!(
                        "{}=seq({},{},{}{})",
                        col.name,
                        quote_digest_str(prefix),
                        start_str,
                        sign,
                        step
                    ));
                }
                ColEncoding::StrTemplate { parts, start } => {
                    let tpl = parts.join("{i}");
                    hoist_parts.push(format!(
                        "{}=tpl({},{})",
                        col.name,
                        quote_digest_str(&tpl),
                        start
                    ));
                }
                ColEncoding::DominantDefault {
                    default_val,
                    exceptions,
                } => {
                    let exc_str: Vec<String> = exceptions
                        .iter()
                        .map(|(i, v)| format!("{}:{}", i, v))
                        .collect();
                    hoist_parts.push(format!(
                        "{}={} except[{}]",
                        col.name,
                        default_val,
                        exc_str.join(",")
                    ));
                }
                _ => {
                    active_indices.push(col_idx);
                }
            }
        }

        // Order non-nullable active columns first, nullable last for trailing-null elision
        active_indices.sort_by_key(|&idx| if table.columns[idx].store.is_nullable() { 1 } else { 0 });

        let mut col_headers: Vec<String> = Vec::with_capacity(active_indices.len());
        for &col_idx in &active_indices {
            let col = &table.columns[col_idx];
            let q = if col.store.is_nullable() { "?" } else { "" };
            let hdr = match &plan.col_encodings[col_idx] {
                ColEncoding::IntScale { label, .. } => format!("{}{}{}", col.name, label, q),
                ColEncoding::LocalEnum { legend, .. } => {
                    let pairs: Vec<String> = legend
                        .iter()
                        .map(|(ch, val)| format!("{}={}", ch, escape_enum_legend_value(val)))
                        .collect();
                    format!("{}({}){}", col.name, pairs.join("|"), q)
                }
                ColEncoding::CrossSuffix {
                    ref_col_name, sep, ..
                } => format!("{}=*{}${}{}", col.name, sep, ref_col_name, q),
                ColEncoding::CrossPrefix {
                    ref_col_name, sep, ..
                } => format!("{}=${}{}*{}", col.name, ref_col_name, sep, q),
                ColEncoding::Affix { prefix, suffix } => match (prefix, suffix) {
                    (Some(p), Some(s)) => {
                        format!("{}:{}*{}{}", col.name, quote_digest_str(p), quote_digest_str(s), q)
                    }
                    (Some(p), None) => format!("{}:{}*{}", col.name, quote_digest_str(p), q),
                    (None, Some(s)) => format!("{}:*{}{}", col.name, quote_digest_str(s), q),
                    (None, None) => format!("{}{}", col.name, q),
                },
                ColEncoding::MajorityPrefix { prefix, suffix } => match suffix {
                    Some(s) => format!(
                        "{}:{}?*{}{}",
                        col.name,
                        quote_digest_str(prefix),
                        quote_digest_str(s),
                        q
                    ),
                    None => format!("{}:{}?{}", col.name, quote_digest_str(prefix), q),
                },
                _ => format!("{}{}", col.name, q),
            };
            col_headers.push(hdr);
        }

        // Compact OLAP stats on non-hoisted numeric/nullable columns
        let mut stats_parts = Vec::new();
        for &col_idx in &active_indices {
            let col = &table.columns[col_idx];
            if col.name.starts_with("metric_") {
                continue;
            }
            match &col.store {
                ColumnStore::Int(ic) if ic.non_null_count > 1 => {
                    let avg = (ic.sum_val as f64) / (ic.non_null_count as f64);
                    let min_cnt = ic.filter_cmp(CmpOp::Eq, ic.min_val as f64).count_ones();
                    stats_parts.push(format!(
                        "{}[sum={},avg={:.1},min={}(n={}),max={}]",
                        col.name, ic.sum_val, avg, ic.min_val, min_cnt, ic.max_val
                    ));
                }
                ColumnStore::Float(fc) if fc.non_null_count > 1 => {
                    let avg = fc.sum_val / (fc.non_null_count as f64);
                    stats_parts.push(format!(
                        "{}[sum={:.2},avg={:.2},min={:.2},max={:.2}]",
                        col.name, fc.sum_val, avg, fc.min_val, fc.max_val
                    ));
                }
                ColumnStore::DictStr(dc) if dc.nullable => {
                    stats_parts.push(format!("{}[non_null={}]", col.name, dc.valid.count_ones()));
                }
                _ => {}
            }
        }

        // Enable intra-row RLE & arithmetic-progression folding ONLY on wide flat tables (>= 8 active cols)
        let enable_row_fold = !is_child_table && active_indices.len() >= 8;

        let format_row_cells = |r_idx: usize| -> (String, bool) {
            let mut cells: Vec<String> = Vec::with_capacity(active_indices.len());
            for &col_idx in &active_indices {
                let col = &table.columns[col_idx];
                let enc = &plan.col_encodings[col_idx];
                let cell_str = format_cell(
                    table,
                    col,
                    enc,
                    r_idx,
                    &global_dict,
                    has_global_dict,
                    is_child_table,
                    enable_row_fold,
                );
                cells.push(cell_str);
            }
            while cells.last().map(|s| s.is_empty()).unwrap_or(false) {
                cells.pop();
            }
            if enable_row_fold {
                let (folded, did_fold) = fold_row_cells(&cells);
                (folded.join(","), did_fold)
            } else {
                (cells.join(","), false)
            }
        };

        if let Some(parent_name) = &table.parent_table {
            lines.push(format!(
                "# {}[{}] -> {} ({})",
                table.name,
                n,
                parent_name,
                col_headers.join(",")
            ));
            if !hoist_parts.is_empty() {
                lines.push(format!("@hoist: {}", hoist_parts.join(" ; ")));
            }
            if !stats_parts.is_empty() && stats_parts.len() <= 6 {
                lines.push(format!("@stats: {}", stats_parts.join(" ")));
            }
            let parent_count = table.csr_offsets.len().saturating_sub(1);
            for p_idx in 0..parent_count {
                let start = table.csr_offsets[p_idx] as usize;
                let end = table.csr_offsets[p_idx + 1] as usize;
                if start >= end {
                    continue;
                }
                let items_str: Vec<String> = (start..end).map(|r| format_row_cells(r).0).collect();
                lines.push(format!("{}:{}", p_idx, items_str.join("|")));
            }
        } else {
            lines.push(format!(
                "# {}[{}]({})",
                table.name,
                n,
                col_headers.join(",")
            ));
            if !hoist_parts.is_empty() {
                lines.push(format!("@hoist: {}", hoist_parts.join(" ; ")));
            }
            if !stats_parts.is_empty() && stats_parts.len() <= 6 {
                lines.push(format!("@stats: {}", stats_parts.join(" ")));
            }
            if !active_indices.is_empty() {
                let mut row_lines = Vec::with_capacity(n);
                let mut any_folded = false;
                for r_idx in 0..n {
                    let (row_s, did_fold) = format_row_cells(r_idx);
                    if did_fold {
                        any_folded = true;
                    }
                    row_lines.push(row_s);
                }
                if any_folded {
                    lines.push("@fold: v*k=repeat, a..b(+d)=seq".to_string());
                }
                lines.extend(row_lines);
            }
        }
    }

    lines.join("\n")
}

struct TablePlan {
    col_encodings: Vec<ColEncoding>,
}

fn analyze_table(table: &ColumnarTable) -> TablePlan {
    let n = table.row_count;
    let is_child = table.parent_table.is_some();
    let mut encodings: Vec<ColEncoding> = vec![ColEncoding::Raw; table.columns.len()];

    if n == 0 {
        return TablePlan {
            col_encodings: encodings,
        };
    }

    // Pass 1: Constant hoisting, monotonic sequences, templates, dominant defaults, and int scaling.
    for (idx, col) in table.columns.iter().enumerate() {
        if col.is_synthetic_count || col.name == "_rowid" || col.name == "_id" {
            continue;
        }
        encodings[idx] = detect_primary_encoding(col, n, is_child);
    }

    // Pass 2: Cross-column functional dependencies (`B = */{sep}$A` or `B = $A{sep}*`)
    // Only consider string columns still in `Raw` against other non-hoisted string columns.
    let string_col_indices: Vec<usize> = table
        .columns
        .iter()
        .enumerate()
        .filter(|(idx, c)| {
            !c.is_synthetic_count
                && c.name != "_rowid"
                && c.name != "_id"
                && !encodings[*idx].is_hoisted()
                && matches!(c.store, ColumnStore::DictStr(ref dc) if !dc.is_primitive_array && !dc.nullable)
        })
        .map(|(idx, _)| idx)
        .collect();

    if n >= 6 && string_col_indices.len() >= 2 {
        // Track columns already referenced as a dependency target so we never create cycles
        let mut referenced_targets: HashSet<usize> = HashSet::new();
        for &b_idx in &string_col_indices {
            if !matches!(encodings[b_idx], ColEncoding::Raw) || referenced_targets.contains(&b_idx) {
                continue;
            }
            if let Some(dep) = detect_cross_column_dependency(table, b_idx, &string_col_indices, &encodings) {
                let target = match &dep {
                    ColEncoding::CrossSuffix { ref_col_idx, .. }
                    | ColEncoding::CrossPrefix { ref_col_idx, .. } => *ref_col_idx,
                    _ => continue,
                };
                referenced_targets.insert(target);
                encodings[b_idx] = dep;
            }
        }
    }

    // Pass 3: Remaining `Raw` string columns -> LocalEnum (`a..z`), strict Affix, or MajorityPrefix.
    for (idx, col) in table.columns.iter().enumerate() {
        if !matches!(encodings[idx], ColEncoding::Raw) {
            continue;
        }
        if col.is_synthetic_count || col.name == "_rowid" || col.name == "_id" {
            continue;
        }
        if let ColumnStore::DictStr(dc) = &col.store {
            if !dc.is_primitive_array {
                if let Some(enum_enc) = detect_local_enum(dc, n) {
                    encodings[idx] = enum_enc;
                    continue;
                }
                if let Some(affix_enc) = detect_affix_or_majority_prefix(dc, n) {
                    encodings[idx] = affix_enc;
                    continue;
                }
            }
        }
    }

    TablePlan {
        col_encodings: encodings,
    }
}

fn detect_primary_encoding(col: &ColumnMeta, n: usize, is_child: bool) -> ColEncoding {
    match &col.store {
        ColumnStore::Bool(bc) => {
            let valid_cnt = bc.valid.count_ones();
            if valid_cnt == n && n >= 4 {
                let t_cnt = bc.valid.and(&bc.true_bits).count_ones();
                if t_cnt == n {
                    return ColEncoding::Const("T".to_string());
                }
                if t_cnt == 0 {
                    return ColEncoding::Const("F".to_string());
                }
                // Dominant-default boolean (>= 88% same, <= 24 exceptions) on flat tables
                if !is_child && n >= 16 {
                    let (def_bool, def_cnt) = if t_cnt >= n - t_cnt {
                        (true, t_cnt)
                    } else {
                        (false, n - t_cnt)
                    };
                    let exc_cnt = n - def_cnt;
                    if def_cnt * 100 >= n * 88 && exc_cnt >= 1 && exc_cnt <= 24 {
                        let mut exceptions = Vec::with_capacity(exc_cnt);
                        for r in 0..n {
                            let v = bc.true_bits.get(r);
                            if v != def_bool {
                                exceptions.push((r, if v { "T".to_string() } else { "F".to_string() }));
                            }
                        }
                        return ColEncoding::DominantDefault {
                            default_val: if def_bool { "T".to_string() } else { "F".to_string() },
                            exceptions,
                        };
                    }
                }
            }
            ColEncoding::Raw
        }
        ColumnStore::Int(ic) => {
            // 1. Constant integer column
            if ic.non_null_count == n && n >= 4 && ic.min_val == ic.max_val {
                return ColEncoding::Const(ic.min_val.to_string());
            }
            // 2. Strictly monotonic integer sequence on flat tables
            if !is_child && ic.non_null_count == n && n >= 6 {
                if let (Some(v0), Some(v1)) = (ic.get(0), ic.get(1)) {
                    let step = v1.wrapping_sub(v0);
                    if step != 0 {
                        let mut is_seq = true;
                        for r in 2..n {
                            let expected = v0.checked_add(step.checked_mul(r as i64).unwrap_or(i64::MAX));
                            if ic.get(r) != expected {
                                is_seq = false;
                                break;
                            }
                        }
                        if is_seq {
                            return ColEncoding::IntSeq { start: v0, step };
                        }
                    }
                }
            }
            // 3. Dominant-default integer on flat tables
            if !is_child && ic.non_null_count == n && n >= 16 {
                if let Some(dom) = detect_dominant_int(ic, n) {
                    return dom;
                }
            }
            // 4. Power-of-10 scale factor (`*1M`, `*1k`)
            if ic.non_null_count >= 4 {
                if let Some((scale, label)) = detect_int_scale(ic, n) {
                    return ColEncoding::IntScale { scale, label };
                }
            }
            ColEncoding::Raw
        }
        ColumnStore::Float(fc) => {
            if fc.non_null_count == n && n >= 4 && (fc.min_val - fc.max_val).abs() == 0.0 {
                if let Some(v) = fc.get(0) {
                    return ColEncoding::Const(v.to_string());
                }
            }
            ColEncoding::Raw
        }
        ColumnStore::DictStr(dc) => {
            if dc.is_primitive_array {
                return ColEncoding::Raw;
            }
            let valid_cnt = dc.valid.count_ones();
            // 1. Constant string column
            if valid_cnt == n && n >= 4 && dc.dictionary.len() == 1 {
                return ColEncoding::Const(escape_dict_value(&dc.dictionary[0]));
            }
            // 2. Monotonic prefix+int sequence (`EMP-1001`, `ch_3N10000`, `ORD-001`) on flat tables
            if !is_child && valid_cnt == n && n >= 6 && dc.dictionary.len() == n {
                if let Some(seq) = detect_str_sequence(dc, n) {
                    return seq;
                }
                if let Some(tpl) = detect_str_template(dc, n) {
                    return tpl;
                }
            }
            // 3. Dominant-default string column (>= 88% identical, <= 24 exceptions) on flat tables
            if !is_child && valid_cnt == n && n >= 16 && dc.dictionary.len() >= 2 {
                let counts = count_dict_ids(dc, n);
                if let Some((top_id, &top_cnt)) = counts.iter().enumerate().max_by_key(|&(_, c)| c) {
                    let exc_cnt = n - top_cnt;
                    if top_cnt * 100 >= n * 88 && exc_cnt >= 1 && exc_cnt <= 24 {
                        let mut exceptions = Vec::with_capacity(exc_cnt);
                        for r in 0..n {
                            let id = unpack_bit(&dc.packed_ids, r, dc.bit_width) as usize;
                            if id != top_id {
                                exceptions.push((r, escape_dict_value(&dc.dictionary[id])));
                            }
                        }
                        return ColEncoding::DominantDefault {
                            default_val: escape_dict_value(&dc.dictionary[top_id]),
                            exceptions,
                        };
                    }
                }
            }
            ColEncoding::Raw
        }
    }
}

fn detect_dominant_int(ic: &ForIntColumn, n: usize) -> Option<ColEncoding> {
    let mut freq: HashMap<i64, usize> = HashMap::new();
    for r in 0..n {
        let v = ic.get(r)?;
        *freq.entry(v).or_insert(0) += 1;
        if freq.len() > 26 {
            return None;
        }
    }
    let (&top_val, &top_cnt) = freq.iter().max_by_key(|&(_, c)| c)?;
    let exc_cnt = n - top_cnt;
    if top_cnt * 100 >= n * 88 && exc_cnt >= 1 && exc_cnt <= 24 {
        let mut exceptions = Vec::with_capacity(exc_cnt);
        for r in 0..n {
            let v = ic.get(r)?;
            if v != top_val {
                exceptions.push((r, v.to_string()));
            }
        }
        return Some(ColEncoding::DominantDefault {
            default_val: top_val.to_string(),
            exceptions,
        });
    }
    None
}

fn detect_int_scale(ic: &ForIntColumn, n: usize) -> Option<(i64, &'static str)> {
    let mut all_1m = true;
    let mut all_1k = true;
    let mut any_nonzero = false;
    for r in 0..n {
        if let Some(v) = ic.get(r) {
            if v != 0 {
                any_nonzero = true;
            }
            if v % 1_000_000 != 0 {
                all_1m = false;
            }
            if v % 1_000 != 0 {
                all_1k = false;
                break;
            }
        }
    }
    if !any_nonzero {
        return None;
    }
    if all_1m {
        Some((1_000_000, "*1M"))
    } else if all_1k {
        Some((1_000, "*1k"))
    } else {
        None
    }
}

fn split_trailing_digits(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    let mut idx = bytes.len();
    while idx > 0 && bytes[idx - 1].is_ascii_digit() {
        idx -= 1;
    }
    if idx == bytes.len() || idx == 0 {
        return None;
    }
    Some((&s[..idx], &s[idx..]))
}

fn detect_str_sequence(dc: &DictStringColumn, n: usize) -> Option<ColEncoding> {
    let s0 = dc.get(0)?;
    let s1 = dc.get(1)?;
    let (p0, d0) = split_trailing_digits(s0)?;
    let (p1, d1) = split_trailing_digits(s1)?;
    if p0 != p1 {
        return None;
    }
    let start: i64 = d0.parse().ok()?;
    let second: i64 = d1.parse().ok()?;
    let step = second.checked_sub(start)?;
    if step == 0 {
        return None;
    }
    let zero_pad = if d0.len() > 1 && d0.starts_with('0') {
        d0.len()
    } else {
        0
    };
    for r in 2..n {
        let cur = dc.get(r)?;
        let expected_num = start.checked_add(step.checked_mul(r as i64)?)?;
        let expected = if zero_pad > 0 {
            format!("{}{:0width$}", p0, expected_num, width = zero_pad)
        } else {
            format!("{}{}", p0, expected_num)
        };
        if cur != expected {
            return None;
        }
    }
    Some(ColEncoding::StrSeq {
        prefix: p0.to_string(),
        start,
        step,
        zero_pad,
    })
}

/// Detects templates where the same monotonic row integer appears 2+ times, e.g. `org-1/project-1`.
fn detect_str_template(dc: &DictStringColumn, n: usize) -> Option<ColEncoding> {
    let s0 = dc.get(0)?;
    // Find all maximal digit runs in s0
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let bytes = s0.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start_i = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            runs.push((start_i, i));
        } else {
            i += 1;
        }
    }
    if runs.len() < 2 {
        return None;
    }
    let first_digits = &s0[runs[0].0..runs[0].1];
    if !runs.iter().all(|&(a, b)| &s0[a..b] == first_digits) {
        return None;
    }
    let start: i64 = first_digits.parse().ok()?;
    let mut parts: Vec<String> = Vec::with_capacity(runs.len() + 1);
    let mut cursor = 0;
    for &(a, b) in &runs {
        parts.push(s0[cursor..a].to_string());
        cursor = b;
    }
    parts.push(s0[cursor..].to_string());

    for r in 1..n {
        let cur = dc.get(r)?;
        let num_str = (start + r as i64).to_string();
        let expected = parts.join(&num_str);
        if cur != expected {
            return None;
        }
    }
    Some(ColEncoding::StrTemplate { parts, start })
}

/// Detects 100% lossless cross-column dependencies:
/// `B = */{sep}$A` (e.g., `repo = */$name` in GitHub APIs) or `B = $A{sep}*`.
fn detect_cross_column_dependency(
    table: &ColumnarTable,
    b_idx: usize,
    candidates: &[usize],
    encodings: &[ColEncoding],
) -> Option<ColEncoding> {
    let n = table.row_count;
    let dc_b = match &table.columns[b_idx].store {
        ColumnStore::DictStr(dc) => dc,
        _ => return None,
    };

    let seps = ["/", "-", "_", ":", ".", "@"];
    let mut best: Option<(usize, ColEncoding)> = None;

    for &a_idx in candidates {
        if a_idx == b_idx {
            continue;
        }
        // Avoid chaining off another cross-column dependency so resolution is always 1-hop
        if matches!(
            encodings[a_idx],
            ColEncoding::CrossSuffix { .. } | ColEncoding::CrossPrefix { .. }
        ) {
            continue;
        }
        let dc_a = match &table.columns[a_idx].store {
            ColumnStore::DictStr(dc) => dc,
            _ => continue,
        };

        // Check suffix dependency: B[r] ends with sep + A[r]
        for sep in &seps {
            let mut all_match = true;
            let mut saved_bytes = 0usize;
            let mut has_literal_eq = false;
            for r in 0..n {
                let (Some(a), Some(b)) = (dc_a.get(r), dc_b.get(r)) else {
                    all_match = false;
                    break;
                };
                if a.is_empty() || b.len() <= a.len() + sep.len() {
                    all_match = false;
                    break;
                }
                let tail_start = b.len() - a.len() - sep.len();
                if !b.is_char_boundary(tail_start)
                    || &b[tail_start..tail_start + sep.len()] != *sep
                    || &b[tail_start + sep.len()..] != a
                {
                    all_match = false;
                    break;
                }
                let residual = &b[..tail_start];
                if residual == "=" {
                    has_literal_eq = true;
                }
                saved_bytes += a.len() + sep.len();
            }
            if all_match && saved_bytes >= 32 {
                let enc = ColEncoding::CrossSuffix {
                    ref_col_idx: a_idx,
                    ref_col_name: table.columns[a_idx].name.clone(),
                    sep: sep.to_string(),
                    allow_eq_shorthand: !has_literal_eq,
                };
                if best.as_ref().map(|(s, _)| saved_bytes > *s).unwrap_or(true) {
                    best = Some((saved_bytes, enc));
                }
            }
        }

        // Check prefix dependency: B[r] starts with A[r] + sep
        for sep in &seps {
            let mut all_match = true;
            let mut saved_bytes = 0usize;
            for r in 0..n {
                let (Some(a), Some(b)) = (dc_a.get(r), dc_b.get(r)) else {
                    all_match = false;
                    break;
                };
                if a.is_empty() || b.len() <= a.len() + sep.len() {
                    all_match = false;
                    break;
                }
                if !b.starts_with(a) || !b[a.len()..].starts_with(sep) {
                    all_match = false;
                    break;
                }
                saved_bytes += a.len() + sep.len();
            }
            if all_match && saved_bytes >= 32 {
                let enc = ColEncoding::CrossPrefix {
                    ref_col_idx: a_idx,
                    ref_col_name: table.columns[a_idx].name.clone(),
                    sep: sep.to_string(),
                };
                if best.as_ref().map(|(s, _)| saved_bytes > *s).unwrap_or(true) {
                    best = Some((saved_bytes, enc));
                }
            }
        }
    }

    best.map(|(_, enc)| enc)
}

/// Detects low-cardinality string columns where single-letter `a..z` codes strictly save BPE tokens.
fn detect_local_enum(dc: &DictStringColumn, n: usize) -> Option<ColEncoding> {
    let valid_cnt = dc.valid.count_ones();
    if valid_cnt < 12 || dc.dictionary.len() < 2 || dc.dictionary.len() > 16 {
        return None;
    }
    let counts = count_dict_ids(dc, n);
    let mut entries: Vec<(usize, &String)> = counts
        .iter()
        .enumerate()
        .filter(|&(_, &c)| c > 0)
        .map(|(id, &c)| (c, &dc.dictionary[id]))
        .collect();
    if entries.len() < 2 || entries.len() > 16 {
        return None;
    }
    // Require at least 2 distinct repeated values so single-repeated-string mixed columns
    // (like `note` with 8x "Standard Shipping Label" + 2 unique notes) use global `@dict`.
    let repeated_distinct = entries.iter().filter(|&&(c, _)| c >= 3).count();
    if repeated_distinct < 2 {
        return None;
    }

    // BPE Anti-Expansion Guard (o200k_base):
    // `,a`..`,z` merge into 1 BPE token per cell, whereas even 2-3 char codes like `,US`, `,L4`,
    // `,eur` tokenize as 2+ tokens. Require avg_cell_len >= 2.0 and net BPE savings > header cost.
    let total_chars: usize = entries.iter().map(|&(c, s)| c * s.len()).sum();
    let avg_cell_len = (total_chars as f64) / (valid_cnt as f64);
    let est_header_tokens = entries.len() * 3 + 2;
    if avg_cell_len < 2.0 || valid_cnt <= est_header_tokens + 8 {
        return None;
    }

    // Sort by frequency descending so the most frequent values get `a`, `b`, `c`...
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));

    let mut map = HashMap::with_capacity(entries.len());
    let mut legend = Vec::with_capacity(entries.len());
    for (idx, &(_, s)) in entries.iter().enumerate() {
        let code = (b'a' + idx as u8) as char;
        map.insert(s.clone(), code);
        legend.push((code, s.clone()));
    }

    Some(ColEncoding::LocalEnum { map, legend })
}

/// Detects strict 100% common prefix/suffix OR >= 75% majority prefix (e.g. ISO-8601 timestamps).
fn detect_affix_or_majority_prefix(dc: &DictStringColumn, n: usize) -> Option<ColEncoding> {
    let valid_cnt = dc.valid.count_ones();
    if dc.dictionary.len() < 4 || valid_cnt < 5 {
        return None;
    }

    // 1. Strict 100% common prefix
    let strict_prefix = detect_strict_prefix(&dc.dictionary);

    // 2. Strict 100% common suffix (e.g. `Z` on ISO-8601 timestamps, `.com`, `@corp.io`)
    let strict_suffix = detect_strict_suffix(&dc.dictionary, strict_prefix.as_deref());

    if strict_prefix.is_some() {
        let saved_per_row = strict_prefix.as_ref().map(|p| p.len()).unwrap_or(0)
            + strict_suffix.as_ref().map(|s| s.len()).unwrap_or(0);
        if saved_per_row * valid_cnt >= saved_per_row + 16 {
            return Some(ColEncoding::Affix {
                prefix: strict_prefix,
                suffix: strict_suffix,
            });
        }
    }

    // 3. Majority prefix (>= 75% of rows share a delimiter-ended prefix of len >= 6)
    // Checked BEFORE suffix-only fallback so ISO-8601 columns like `pushedAt` get BOTH
    // majority prefix (`"2026-07-"?`) AND strict `"Z"` suffix!
    if valid_cnt >= 16 {
        let mut prefix_counts: HashMap<String, usize> = HashMap::new();
        for r in 0..n {
            if let Some(s) = dc.get(r) {
                let mut seen_for_row: HashSet<&str> = HashSet::new();
                for (byte_idx, ch) in s.char_indices() {
                    let end = byte_idx + ch.len_utf8();
                    if end >= 6 && end <= 16 && matches!(ch, '-' | '_' | '/' | ':' | 'T') {
                        let cand = &s[..end];
                        if seen_for_row.insert(cand) {
                            *prefix_counts.entry(cand.to_string()).or_insert(0) += 1;
                        }
                    }
                }
            }
        }
        let mut best_pref: Option<(usize, String)> = None;
        for (pref, cnt) in prefix_counts {
            if cnt * 100 >= valid_cnt * 75 {
                let non_match = valid_cnt - cnt;
                let net_saved = (cnt * pref.len()).saturating_sub(non_match + pref.len() + 6);
                if net_saved >= 32 {
                    if best_pref.as_ref().map(|(s, _)| net_saved > *s).unwrap_or(true) {
                        best_pref = Some((net_saved, pref));
                    }
                }
            }
        }
        if let Some((_, pref)) = best_pref {
            return Some(ColEncoding::MajorityPrefix {
                prefix: pref,
                suffix: strict_suffix,
            });
        }
    }

    // 4. Suffix-only fallback (e.g., `createdAt:*"Z"` when dates span many years)
    if let Some(suf) = strict_suffix {
        if suf.len() * valid_cnt >= suf.len() + 16 {
            return Some(ColEncoding::Affix {
                prefix: None,
                suffix: Some(suf),
            });
        }
    }

    None
}

fn detect_strict_prefix(dict: &[String]) -> Option<String> {
    let first = dict.first()?.as_str();
    if first.len() < 4 {
        return None;
    }
    let mut prefix_len = first.len();
    for s in &dict[1..] {
        let common = first
            .chars()
            .zip(s.chars())
            .take_while(|(a, b)| a == b)
            .map(|(c, _)| c.len_utf8())
            .sum::<usize>();
        if common < prefix_len {
            prefix_len = common;
        }
        if prefix_len < 3 {
            return None;
        }
    }
    let raw_prefix = &first[..prefix_len];
    let trimmed = raw_prefix.trim_end_matches(|c: char| c.is_ascii_digit());
    if trimmed.len() >= 3
        && (trimmed.ends_with('-')
            || trimmed.ends_with('_')
            || trimmed.ends_with('/')
            || trimmed.ends_with(':')
            || trimmed.ends_with('T'))
    {
        Some(trimmed.to_string())
    } else if prefix_len >= 5 {
        // Also allow alphanumeric prefixes like `ch_3N` if >= 5 chars and all values have non-empty residual
        if dict.iter().all(|s| s.len() > prefix_len) {
            Some(raw_prefix.to_string())
        } else {
            None
        }
    } else {
        None
    }
}

fn detect_strict_suffix(dict: &[String], prefix: Option<&str>) -> Option<String> {
    let first = dict.first()?.as_str();
    let pref_len = prefix.map(|p| p.len()).unwrap_or(0);
    if first.len() <= pref_len + 2 {
        return None;
    }
    let first_rev: Vec<char> = first[pref_len..].chars().rev().collect();
    let mut suf_chars = first_rev.len();
    for s in &dict[1..] {
        if s.len() <= pref_len {
            return None;
        }
        let common = first_rev
            .iter()
            .zip(s[pref_len..].chars().rev())
            .take_while(|(a, b)| *a == b)
            .count();
        if common < suf_chars {
            suf_chars = common;
        }
        if suf_chars == 0 {
            return None;
        }
    }
    let suf: String = first_rev[..suf_chars].iter().rev().collect();
    // Allow delimiter-started suffixes of len >= 3 (e.g. `.com`, `.io`, `@corp.com`, `:00Z`),
    // or 1-char `Z` suffix specifically for ISO-8601 UTC timestamps (`...T...Z`).
    if suf.len() >= 3
        && (suf.starts_with('.')
            || suf.starts_with('@')
            || suf.starts_with('/')
            || suf.starts_with('-')
            || suf.starts_with(':'))
    {
        Some(suf)
    } else if suf.ends_with('Z') && dict.iter().all(|s| s.len() >= 19 && s.contains('T')) {
        Some("Z".to_string())
    } else {
        None
    }
}

fn count_dict_ids(dc: &DictStringColumn, row_count: usize) -> Vec<usize> {
    let mut counts = vec![0usize; dc.dictionary.len()];
    for r in 0..row_count {
        if dc.valid.get(r) {
            let id = unpack_bit(&dc.packed_ids, r, dc.bit_width) as usize;
            if id < counts.len() {
                counts[id] += 1;
            }
        }
    }
    counts
}

#[allow(clippy::too_many_arguments)]
fn format_cell(
    table: &ColumnarTable,
    col: &ColumnMeta,
    enc: &ColEncoding,
    r_idx: usize,
    global_dict: &HashMap<String, String>,
    has_global_dict: bool,
    is_child_table: bool,
    enable_row_fold: bool,
) -> String {
    match &col.store {
        ColumnStore::Int(ic) => match ic.get(r_idx) {
            Some(v) => match enc {
                ColEncoding::IntScale { scale, .. } => (v / *scale).to_string(),
                _ => v.to_string(),
            },
            None => String::new(),
        },
        ColumnStore::Float(fc) => fc.get(r_idx).map(|v| v.to_string()).unwrap_or_default(),
        ColumnStore::Bool(bc) => bc
            .get(r_idx)
            .map(|b| if b { "T".to_string() } else { "F".to_string() })
            .unwrap_or_default(),
        ColumnStore::DictStr(dc) => match dc.get(r_idx) {
            None => String::new(),
            Some(s) => match enc {
                ColEncoding::LocalEnum { map, .. } => {
                    if let Some(&ch) = map.get(s) {
                        ch.to_string()
                    } else {
                        escape_digest_cell(s, is_child_table, has_global_dict, enable_row_fold)
                    }
                }
                ColEncoding::CrossSuffix {
                    ref_col_idx,
                    sep,
                    allow_eq_shorthand,
                    ..
                } => {
                    let ref_val = match &table.columns[*ref_col_idx].store {
                        ColumnStore::DictStr(ref_dc) => ref_dc.get(r_idx).unwrap_or(""),
                        _ => "",
                    };
                    let strip_len = ref_val.len() + sep.len();
                    let residual = &s[..s.len() - strip_len];
                    if *allow_eq_shorthand && residual == ref_val {
                        "=".to_string()
                    } else {
                        escape_digest_cell(residual, is_child_table, has_global_dict, enable_row_fold)
                    }
                }
                ColEncoding::CrossPrefix {
                    ref_col_idx, sep, ..
                } => {
                    let ref_val = match &table.columns[*ref_col_idx].store {
                        ColumnStore::DictStr(ref_dc) => ref_dc.get(r_idx).unwrap_or(""),
                        _ => "",
                    };
                    let residual = &s[ref_val.len() + sep.len()..];
                    escape_digest_cell(residual, is_child_table, has_global_dict, enable_row_fold)
                }
                ColEncoding::Affix { prefix, suffix } => {
                    let mut view = s;
                    if let Some(p) = prefix {
                        view = view.strip_prefix(p.as_str()).unwrap_or(view);
                    }
                    if let Some(suf) = suffix {
                        view = view.strip_suffix(suf.as_str()).unwrap_or(view);
                    }
                    escape_digest_cell(view, is_child_table, has_global_dict, enable_row_fold)
                }
                ColEncoding::MajorityPrefix { prefix, suffix } => {
                    let mut view = s;
                    if let Some(suf) = suffix {
                        view = view.strip_suffix(suf.as_str()).unwrap_or(view);
                    }
                    if let Some(stripped) = view.strip_prefix(prefix.as_str()) {
                        if stripped.starts_with('^') {
                            quote_digest_str(stripped)
                        } else {
                            escape_digest_cell(stripped, is_child_table, has_global_dict, enable_row_fold)
                        }
                    } else {
                        let full = format!("^{}", view);
                        escape_digest_cell(&full, is_child_table, has_global_dict, enable_row_fold)
                    }
                }
                _ => {
                    if let Some(code) = global_dict.get(s) {
                        code.clone()
                    } else {
                        escape_digest_cell(s, is_child_table, has_global_dict, enable_row_fold)
                    }
                }
            },
        },
    }
}

/// Folds identical runs of length >= 4 (`v*k`) and integer arithmetic progressions of length >= 4
/// (`a..b` for step +1, `a..b(+d)` or `a..b(d)` for other non-zero steps).
fn fold_row_cells(cells: &[String]) -> (Vec<String>, bool) {
    let mut out = Vec::with_capacity(cells.len());
    let mut did_fold = false;
    let mut i = 0;
    while i < cells.len() {
        let cur = &cells[i];
        // 1. Identical non-empty unquoted run >= 4
        if !cur.is_empty() && !cur.starts_with('"') && !cur.contains('*') && !cur.contains("..") {
            let mut j = i + 1;
            while j < cells.len() && cells[j] == *cur {
                j += 1;
            }
            if j - i >= 4 {
                out.push(format!("{}*{}", cur, j - i));
                did_fold = true;
                i = j;
                continue;
            }
        }
        // 2. Integer arithmetic progression >= 4
        if i + 3 < cells.len() {
            if let (Ok(a0), Ok(a1)) = (cur.parse::<i64>(), cells[i + 1].parse::<i64>()) {
                if let Some(step) = a1.checked_sub(a0) {
                    if step != 0 {
                        let mut k = i + 2;
                        let mut prev = a1;
                        while k < cells.len() {
                            if let Ok(val) = cells[k].parse::<i64>() {
                                if val.checked_sub(prev) == Some(step) {
                                    prev = val;
                                    k += 1;
                                    continue;
                                }
                            }
                            break;
                        }
                        if k - i >= 4 {
                            if step == 1 {
                                out.push(format!("{}..{}", a0, prev));
                            } else if step > 0 {
                                out.push(format!("{}..{}(+{})", a0, prev, step));
                            } else {
                                out.push(format!("{}..{}({})", a0, prev, step));
                            }
                            did_fold = true;
                            i = k;
                            continue;
                        }
                    }
                }
            }
        }
        out.push(cur.clone());
        i += 1;
    }
    (out, did_fold)
}

pub(crate) fn quote_digest_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Escapes a digest cell only when it contains delimiters, quotes, backslashes, newlines,
/// leading/trailing spaces, `$` prefixes colliding with `@dict`, or fold tokens (`*` / `..`)
/// when intra-row folding is active.
pub(crate) fn escape_digest_cell(
    s: &str,
    escape_pipe: bool,
    escape_dollar: bool,
    escape_fold: bool,
) -> String {
    let needs_quote = s.starts_with(' ')
        || s.ends_with(' ')
        || (escape_dollar && s.starts_with('$'))
        || (escape_fold && (s.contains('*') || s.contains("..")))
        || s.chars().any(|c| {
            matches!(c, ',' | '"' | '\\' | '\n' | '\r') || (escape_pipe && c == '|')
        });
    if needs_quote {
        quote_digest_str(s)
    } else {
        s.to_string()
    }
}

fn escape_dict_value(s: &str) -> String {
    let needs_quote = s.is_empty()
        || s.chars().any(|c| {
            c.is_whitespace() || matches!(c, '(' | ')' | ',' | '"' | '\\' | '\n' | '\r' | '|' | ';')
        });
    if needs_quote {
        quote_digest_str(s)
    } else {
        s.to_string()
    }
}

fn escape_enum_legend_value(s: &str) -> String {
    let needs_quote = s.is_empty()
        || s.chars().any(|c| {
            c.is_whitespace() || matches!(c, '(' | ')' | ',' | '"' | '\\' | '\n' | '\r' | '|' | '=')
        });
    if needs_quote {
        quote_digest_str(s)
    } else {
        s.to_string()
    }
}
