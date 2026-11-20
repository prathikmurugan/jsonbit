use std::collections::HashMap;
use serde_json::{Map, Value};
use crate::bitmap::BitVec64;
use crate::column::{CmpOp, ColumnStore};
use crate::engine::JsonOlapEngine;
use crate::normalizer::ColumnarTable;

/// Executes a read-only SQL query directly against the Pure-Rust Bit-Packed
/// Columnar Tables and `BitVec64` Bitmap Indices (Zero SQLite).
///
/// Supports:
/// - Automatic JSON dot-path resolution (`customer.name` -> `customer_name`, `error.retryable` -> `error_retryable`)
/// - `WHERE` bitmap algebra (`AND`, `OR`, `=`, `!=`, `>`, `>=`, `<`, `<=`, `LIKE`, `ILIKE`, `IS NULL`, `IS NOT NULL`, `true`/`false`)
/// - `COUNT(*)`, `COUNT(DISTINCT col)`, `SUM(col)`, `AVG(col)`, `MIN(col)`, `MAX(col)`
/// - `GROUP BY col`, `ORDER BY col ASC|DESC`, `LIMIT n`
/// - 1-to-Many CSR parent-child `JOIN`s (`FROM orders JOIN orders_items ...`)
pub fn execute_sql(engine: &JsonOlapEngine, raw_sql: &str) -> Result<Value, String> {
    let trimmed = raw_sql.trim().trim_end_matches(';').trim();
    let upper = trimmed.to_uppercase();

    for forbidden in [
        "DROP ", "DELETE ", "UPDATE ", "INSERT ", "ALTER ", "CREATE ", "TRUNCATE ", "PRAGMA ",
    ] {
        if upper.starts_with(forbidden) || upper.contains(&format!(" {}", forbidden)) {
            return Err(format!(
                "SecurityError: Only read-only SELECT queries are permitted (found {}).",
                forbidden.trim()
            ));
        }
    }

    if !upper.starts_with("SELECT ") {
        return Err("SyntaxError: Query must begin with SELECT.".to_string());
    }

    // Rewrite JSON dot-paths (`customer.name` -> `customer_name`) outside single quotes
    let normalized_sql = rewrite_dot_paths(trimmed, &engine.tables);

    // Parse main clauses: SELECT <proj> FROM <from_clause> [WHERE <where>] [GROUP BY <gb>] [ORDER BY <ob>] [LIMIT <lim>]
    let parsed = parse_select_query(&normalized_sql)?;

    // Resolve target table (and optional CSR joined child table)
    let table = engine
        .tables
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(&parsed.from_table))
        .or_else(|| engine.tables.first())
        .ok_or_else(|| format!("Table not found: {}", parsed.from_table))?;

    // Check if query joins a child table (supports both `FROM parent JOIN child` and `FROM child JOIN parent`)
    if let Some(join_table_name) = &parsed.join_table {
        let joined_table = engine
            .tables
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(join_table_name))
            .ok_or_else(|| format!("Joined table not found: {}", join_table_name))?;
        let (parent, child) =
            if !table.parent_row_ids.is_empty() && joined_table.parent_row_ids.is_empty() {
                (joined_table, table)
            } else {
                (table, joined_table)
            };
        return execute_csr_join_sql(parent, child, &parsed);
    }

    // Evaluate WHERE clause into a `BitVec64` bitmap mask!
    let mask = match &parsed.where_clause {
        Some(w_str) => eval_where_bitmap(table, w_str)?,
        None => BitVec64::new_all_ones(table.row_count),
    };

    // Check if GROUP BY is present (supports single or comma-separated multi-column GROUP BY)
    if let Some(group_col_raw) = &parsed.group_by {
        let group_cols: Vec<String> = split_top_level_commas(group_col_raw)
            .into_iter()
            .map(|c| resolve_col_name(table, &c))
            .collect();
        return execute_group_by(table, &mask, &group_cols, &parsed);
    }

    // Check if SELECT clause contains aggregate functions (`COUNT`, `SUM`, `AVG`, `MIN`, `MAX`)
    let has_agg = parsed.projections.iter().any(|p| is_aggregate_expr(&p.expr));
    if has_agg {
        let row_obj = eval_agg_projections(table, &mask, &parsed.projections);
        return Ok(Value::Array(vec![Value::Object(row_obj)]));
    }

    // Standard row projection over set bits in `mask`
    let mut rows_out: Vec<Map<String, Value>> = Vec::new();
    let is_star = parsed.projections.len() == 1 && parsed.projections[0].expr == "*";

    for row_idx in mask.iter_ones() {
        if is_star {
            rows_out.push(table.reconstruct_flat_row_json(row_idx, false));
        } else {
            let mut obj = Map::new();
            for proj in &parsed.projections {
                let col_name = resolve_col_name(table, &proj.expr);
                let val = table
                    .get_col(&col_name)
                    .and_then(|c| c.store.get_json_value(row_idx))
                    .unwrap_or(Value::Null);
                obj.insert(proj.alias.clone(), val);
            }
            rows_out.push(obj);
        }
    }

    // Apply ORDER BY if present
    if let Some((order_col_raw, desc)) = &parsed.order_by {
        let order_col = resolve_col_name(table, order_col_raw);
        rows_out.sort_by(|a, b| {
            let va = a.get(&order_col).or_else(|| a.get(order_col_raw));
            let vb = b.get(&order_col).or_else(|| b.get(order_col_raw));
            let cmp = compare_json_vals(va, vb);
            if *desc { cmp.reverse() } else { cmp }
        });
    }

    if let Some(off) = parsed.offset {
        if off < rows_out.len() {
            rows_out.drain(0..off);
        } else {
            rows_out.clear();
        }
    }

    if let Some(lim) = parsed.limit {
        rows_out.truncate(lim);
    }

    Ok(Value::Array(
        rows_out.into_iter().map(Value::Object).collect(),
    ))
}

#[derive(Debug)]
struct ProjectionItem {
    expr: String,
    alias: String,
}

#[derive(Debug)]
struct ParsedSelect {
    projections: Vec<ProjectionItem>,
    from_table: String,
    join_table: Option<String>,
    where_clause: Option<String>,
    group_by: Option<String>,
    order_by: Option<(String, bool)>,
    limit: Option<usize>,
    offset: Option<usize>,
}

fn rewrite_dot_paths(sql: &str, tables: &[ColumnarTable]) -> String {
    let mut dot_mappings: Vec<(String, String)> = Vec::new();
    for t in tables {
        for c in &t.columns {
            if c.source_path.len() >= 2 && !c.is_synthetic_count {
                dot_mappings.push((c.source_path.join("."), c.name.clone()));
            }
        }
    }
    dot_mappings.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

    let mut out = String::with_capacity(sql.len() + 16);
    let mut in_single = false;
    let mut seg_start = 0;
    let bytes = sql.as_bytes();

    for i in 0..bytes.len() {
        if bytes[i] == b'\'' && (i == 0 || bytes[i - 1] != b'\\') {
            if !in_single {
                out.push_str(&replace_segment(&sql[seg_start..i], &dot_mappings));
                seg_start = i;
                in_single = true;
            } else {
                out.push_str(&sql[seg_start..=i]);
                seg_start = i + 1;
                in_single = false;
            }
        }
    }
    if seg_start < sql.len() {
        if in_single {
            out.push_str(&sql[seg_start..]);
        } else {
            out.push_str(&replace_segment(&sql[seg_start..], &dot_mappings));
        }
    }
    out
}

fn replace_segment(seg: &str, mappings: &[(String, String)]) -> String {
    let mut s = seg.to_string();
    for (dot, col) in mappings {
        s = s.replace(dot, col);
    }
    s
}

fn parse_select_query(sql: &str) -> Result<ParsedSelect, String> {
    let upper = sql.to_uppercase();
    let from_pos = find_keyword_outside_quotes(&upper, " FROM ")
        .ok_or_else(|| "SyntaxError: Missing FROM clause.".to_string())?;

    let select_part = sql[7..from_pos].trim();
    let rest_from = &sql[from_pos + 6..];
    let rest_upper = &upper[from_pos + 6..];

    let where_pos = find_keyword_outside_quotes(rest_upper, " WHERE ");
    let group_pos = find_keyword_outside_quotes(rest_upper, " GROUP BY ");
    let order_pos = find_keyword_outside_quotes(rest_upper, " ORDER BY ");
    let limit_pos = find_keyword_outside_quotes(rest_upper, " LIMIT ");
    let offset_pos = find_keyword_outside_quotes(rest_upper, " OFFSET ");

    let mut clause_starts: Vec<usize> = [where_pos, group_pos, order_pos, limit_pos, offset_pos]
        .into_iter()
        .flatten()
        .collect();
    clause_starts.sort_unstable();

    let from_end = clause_starts.first().copied().unwrap_or(rest_from.len());
    let from_part = rest_from[..from_end].trim();

    // Check for JOIN inside `from_part`
    let from_upper = from_part.to_uppercase();
    let (from_table, join_table) = if let Some(j_pos) = from_upper.find(" JOIN ") {
        let left_raw = from_part[..j_pos].trim();
        let right_raw = from_part[j_pos + 6..].trim();
        let left_tbl = clean_ident(left_raw.split_whitespace().next().unwrap_or(""));
        let right_tbl = clean_ident(right_raw.split_whitespace().next().unwrap_or(""));
        (left_tbl, Some(right_tbl))
    } else {
        let tbl = clean_ident(from_part.split_whitespace().next().unwrap_or(""));
        (tbl, None)
    };

    let slice_clause = |start_opt: Option<usize>, kw_len: usize| -> Option<String> {
        let s = start_opt?;
        let body_start = s + kw_len;
        let body_end = clause_starts
            .iter()
            .copied()
            .find(|&p| p > s)
            .unwrap_or(rest_from.len());
        Some(rest_from[body_start..body_end].trim().to_string())
    };

    let where_clause = slice_clause(where_pos, 7);
    let group_by = slice_clause(group_pos, 10);
    let order_by = slice_clause(order_pos, 10).map(|ob| {
        let parts: Vec<&str> = ob.split_whitespace().collect();
        let col = clean_ident(parts.first().copied().unwrap_or(""));
        let desc = parts
            .get(1)
            .map(|d| d.eq_ignore_ascii_case("DESC"))
            .unwrap_or(false);
        (col, desc)
    });
    let limit = slice_clause(limit_pos, 7).and_then(|l| l.parse::<usize>().ok());
    let offset = slice_clause(offset_pos, 8).and_then(|o| o.parse::<usize>().ok());

    let projections = split_top_level_commas(select_part)
        .into_iter()
        .map(|item| {
            let item_trim = item.trim();
            let item_upper = item_trim.to_uppercase();
            if let Some(as_pos) = find_keyword_outside_quotes(&item_upper, " AS ") {
                let expr = clean_ident(item_trim[..as_pos].trim());
                let alias = clean_ident(item_trim[as_pos + 4..].trim());
                ProjectionItem { expr, alias }
            } else {
                let expr = clean_ident(item_trim);
                ProjectionItem {
                    alias: expr.clone(),
                    expr,
                }
            }
        })
        .collect();

    Ok(ParsedSelect {
        projections,
        from_table,
        join_table,
        where_clause,
        group_by,
        order_by,
        limit,
        offset,
    })
}

fn find_keyword_outside_quotes(upper_sql: &str, kw: &str) -> Option<usize> {
    let bytes = upper_sql.as_bytes();
    let mut in_single = false;
    let mut paren_depth = 0usize;

    for i in 0..bytes.len() {
        let b = bytes[i];
        if b == b'\'' && (i == 0 || bytes[i - 1] != b'\\') {
            in_single = !in_single;
            continue;
        }
        if in_single {
            continue;
        }
        if b == b'(' {
            paren_depth += 1;
        } else if b == b')' {
            paren_depth = paren_depth.saturating_sub(1);
        } else if paren_depth == 0 && upper_sql[i..].starts_with(kw) {
            return Some(i);
        }
    }
    None
}

fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut curr = String::new();
    let mut in_single = false;
    let mut paren = 0usize;

    for c in s.chars() {
        if c == '\'' {
            in_single = !in_single;
            curr.push(c);
        } else if !in_single && c == '(' {
            paren += 1;
            curr.push(c);
        } else if !in_single && c == ')' {
            paren = paren.saturating_sub(1);
            curr.push(c);
        } else if !in_single && paren == 0 && c == ',' {
            out.push(curr.trim().to_string());
            curr.clear();
        } else {
            curr.push(c);
        }
    }
    if !curr.trim().is_empty() {
        out.push(curr.trim().to_string());
    }
    out
}

fn clean_ident(raw: &str) -> String {
    let t = raw.trim().trim_matches('"').trim_matches('`');
    // Strip table alias prefix like `p."status"` or `c.name` if single identifier
    if !t.contains('(') && t.contains('.') {
        if let Some((_, right)) = t.split_once('.') {
            return right.trim_matches('"').to_string();
        }
    }
    t.to_string()
}

fn resolve_col_name(table: &ColumnarTable, raw: &str) -> String {
    let mut t = raw.trim();
    let t_upper = t.to_uppercase();
    for fn_prefix in ["LOWER(", "UPPER(", "TRIM("] {
        if t_upper.starts_with(fn_prefix) && t.ends_with(')') {
            t = t[fn_prefix.len()..t.len() - 1].trim();
            break;
        }
    }
    let cleaned = clean_ident(t);
    if table.get_col(&cleaned).is_some() {
        return cleaned;
    }
    for col in &table.columns {
        if col.name.eq_ignore_ascii_case(&cleaned) {
            return col.name.clone();
        }
    }
    // Suffix match for nested singleton/dot-path columns (e.g. `autoscaler_maxNodes` or `maxNodes` -> `cluster_autoscaler_maxNodes`)
    let suffix = format!("_{}", cleaned.to_lowercase());
    let matches: Vec<&str> = table
        .columns
        .iter()
        .filter(|c| c.name.to_lowercase().ends_with(&suffix))
        .map(|c| c.name.as_str())
        .collect();
    if matches.len() == 1 {
        return matches[0].to_string();
    }
    cleaned
}

fn eval_where_bitmap(table: &ColumnarTable, where_str: &str) -> Result<BitVec64, String> {
    // Support OR groups of AND clauses
    let or_parts = split_by_keyword_outside_quotes(where_str, " OR ");
    if or_parts.len() > 1 {
        let mut union_mask = BitVec64::new_empty(table.row_count);
        for part in or_parts {
            let sub = eval_and_clauses(table, &part)?;
            union_mask = union_mask.or(&sub);
        }
        return Ok(union_mask);
    }
    eval_and_clauses(table, where_str)
}

fn eval_and_clauses(table: &ColumnarTable, and_str: &str) -> Result<BitVec64, String> {
    let raw_clauses = split_by_keyword_outside_quotes(and_str, " AND ");
    let mut clauses = Vec::with_capacity(raw_clauses.len());
    let mut i = 0;
    while i < raw_clauses.len() {
        let c = &raw_clauses[i];
        let upper = c.to_uppercase();
        if let Some(b_pos) = upper.find(" BETWEEN ") {
            if i + 1 < raw_clauses.len() {
                let col_part = c[..b_pos].trim();
                let low_part = c[b_pos + 9..].trim();
                let high_part = raw_clauses[i + 1].trim();
                clauses.push(format!("{} >= {}", col_part, low_part));
                clauses.push(format!("{} <= {}", col_part, high_part));
                i += 2;
                continue;
            }
        }
        clauses.push(c.clone());
        i += 1;
    }

    let mut mask = BitVec64::new_all_ones(table.row_count);
    for clause in clauses {
        let sub_mask = eval_single_predicate(table, clause.trim())?;
        mask.and_in_place(&sub_mask);
    }
    Ok(mask)
}

fn split_by_keyword_outside_quotes(s: &str, kw_upper: &str) -> Vec<String> {
    let upper = s.to_uppercase();
    let bytes = upper.as_bytes();
    let mut out = Vec::new();
    let mut in_single = false;
    let mut last = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'\'' && (i == 0 || bytes[i - 1] != b'\\') {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if !in_single && upper[i..].starts_with(kw_upper) {
            out.push(s[last..i].to_string());
            i += kw_upper.len();
            last = i;
        } else {
            i += 1;
        }
    }
    out.push(s[last..].to_string());
    out
}

fn eval_single_predicate(table: &ColumnarTable, pred: &str) -> Result<BitVec64, String> {
    let p_trim = pred.trim().trim_start_matches('(').trim_end_matches(')').trim();
    let upper = p_trim.to_uppercase();

    // 1. Check `IS NOT NULL` / `IS NULL`
    if let Some(pos) = upper.find(" IS NOT NULL") {
        let col_name = resolve_col_name(table, &p_trim[..pos]);
        let col = table
            .get_col(&col_name)
            .ok_or_else(|| format!("Unknown column: {}", col_name))?;
        return Ok(match &col.store {
            ColumnStore::Int(c) => c.valid.clone(),
            ColumnStore::Float(c) => c.valid.clone(),
            ColumnStore::Bool(c) => c.valid.clone(),
            ColumnStore::DictStr(c) => c.valid.clone(),
        });
    }
    if let Some(pos) = upper.find(" IS NULL") {
        let col_name = resolve_col_name(table, &p_trim[..pos]);
        let col = table
            .get_col(&col_name)
            .ok_or_else(|| format!("Unknown column: {}", col_name))?;
        let all = BitVec64::new_all_ones(table.row_count);
        return Ok(match &col.store {
            ColumnStore::Int(c) => c.valid.inverted_with_validity(&all),
            ColumnStore::Float(c) => c.valid.inverted_with_validity(&all),
            ColumnStore::Bool(c) => c.valid.inverted_with_validity(&all),
            ColumnStore::DictStr(c) => c.valid.inverted_with_validity(&all),
        });
    }

    // 2. Check `IS TRUE` / `IS FALSE`
    if let Some(pos) = upper.find(" IS TRUE") {
        let col_name = resolve_col_name(table, &p_trim[..pos]);
        if let Some(col) = table.get_col(&col_name) {
            if let ColumnStore::Bool(bc) = &col.store {
                return Ok(bc.filter_bool(true));
            }
        }
    }
    if let Some(pos) = upper.find(" IS FALSE") {
        let col_name = resolve_col_name(table, &p_trim[..pos]);
        if let Some(col) = table.get_col(&col_name) {
            if let ColumnStore::Bool(bc) = &col.store {
                return Ok(bc.filter_bool(false));
            }
        }
    }

    // 3. Check `NOT IN (...)` and `IN (...)`
    for (in_kw, is_not_in) in [(" NOT IN ", true), (" IN ", false)] {
        if let Some(pos) = upper.find(in_kw) {
            let col_name = resolve_col_name(table, &p_trim[..pos]);
            let Some(col) = table.get_col(&col_name) else {
                return Ok(BitVec64::new_all_ones(table.row_count));
            };
            let list_raw = p_trim[pos + in_kw.len()..]
                .trim()
                .trim_start_matches('(')
                .trim_end_matches(')');
            let items = split_top_level_commas(list_raw);
            let mut union_bm = BitVec64::new_empty(table.row_count);
            for item in items {
                let val_str = item.trim().trim_matches('\'').trim_matches('"');
                let hit_bm = match &col.store {
                    ColumnStore::DictStr(dc) => dc.filter_eq_case_insensitive(val_str),
                    ColumnStore::Int(ic) => {
                        let v = val_str.parse::<f64>().unwrap_or(0.0);
                        ic.filter_cmp(CmpOp::Eq, v)
                    }
                    ColumnStore::Float(fc) => {
                        let v = val_str.parse::<f64>().unwrap_or(0.0);
                        fc.filter_cmp(CmpOp::Eq, v)
                    }
                    ColumnStore::Bool(bc) => {
                        let b = matches!(val_str.to_lowercase().as_str(), "true" | "1" | "yes");
                        bc.filter_bool(b)
                    }
                };
                union_bm = union_bm.or(&hit_bm);
            }
            if is_not_in {
                let valid = match &col.store {
                    ColumnStore::Int(c) => &c.valid,
                    ColumnStore::Float(c) => &c.valid,
                    ColumnStore::Bool(c) => &c.valid,
                    ColumnStore::DictStr(c) => &c.valid,
                };
                union_bm = union_bm.inverted_with_validity(valid);
            }
            return Ok(union_bm);
        }
    }

    // 4. Check `ILIKE` / `LIKE`
    for kw in [" ILIKE ", " LIKE "] {
        if let Some(pos) = upper.find(kw) {
            let col_name = resolve_col_name(table, &p_trim[..pos]);
            let pat_raw = p_trim[pos + kw.len()..]
                .trim()
                .trim_matches('\'')
                .trim_matches('"')
                .trim_matches('%')
                .to_lowercase();
            let col = table
                .get_col(&col_name)
                .ok_or_else(|| format!("Unknown column: {}", col_name))?;
            if let ColumnStore::DictStr(dc) = &col.store {
                return Ok(dc.filter_like_case_insensitive(&pat_raw));
            }
        }
    }

    // 4. Standard comparison operators (`>=`, `<=`, `!=`, `<>`, `=`, `>`, `<`)
    for (op_str, op_enum, is_neq) in [
        (">=", CmpOp::Gte, false),
        ("<=", CmpOp::Lte, false),
        ("!=", CmpOp::Eq, true),
        ("<>", CmpOp::Eq, true),
        ("=", CmpOp::Eq, false),
        (">", CmpOp::Gt, false),
        ("<", CmpOp::Lt, false),
    ] {
        if let Some(pos) = p_trim.find(op_str) {
            let col_name = resolve_col_name(table, &p_trim[..pos]);
            let rhs_raw = p_trim[pos + op_str.len()..]
                .trim()
                .trim_matches('\'')
                .trim_matches('"');

            let Some(col) = table.get_col(&col_name) else {
                // Ignore predicates belonging to the other table during a JOIN
                return Ok(BitVec64::new_all_ones(table.row_count));
            };

            let mut res = match &col.store {
                ColumnStore::Bool(bc) => {
                    let target = matches!(
                        rhs_raw.to_lowercase().as_str(),
                        "true" | "1" | "yes"
                    );
                    bc.filter_bool(target)
                }
                ColumnStore::Int(ic) => {
                    let val = rhs_raw.parse::<f64>().unwrap_or(0.0);
                    ic.filter_cmp(op_enum, val)
                }
                ColumnStore::Float(fc) => {
                    let val = rhs_raw.parse::<f64>().unwrap_or(0.0);
                    fc.filter_cmp(op_enum, val)
                }
                ColumnStore::DictStr(dc) => dc.filter_eq_case_insensitive(rhs_raw),
            };

            if is_neq {
                let valid = match &col.store {
                    ColumnStore::Int(c) => &c.valid,
                    ColumnStore::Float(c) => &c.valid,
                    ColumnStore::Bool(c) => &c.valid,
                    ColumnStore::DictStr(c) => &c.valid,
                };
                res = res.inverted_with_validity(valid);
            }
            return Ok(res);
        }
    }

    Ok(BitVec64::new_all_ones(table.row_count))
}

fn is_aggregate_expr(expr: &str) -> bool {
    let u = expr.to_uppercase();
    u.starts_with("COUNT(")
        || u.starts_with("SUM(")
        || u.starts_with("AVG(")
        || u.starts_with("MIN(")
        || u.starts_with("MAX(")
        || u.starts_with("ROUND(")
}

fn eval_agg_projections(
    table: &ColumnarTable,
    mask: &BitVec64,
    projections: &[ProjectionItem],
) -> Map<String, Value> {
    let mut row = Map::new();
    for proj in projections {
        let val = eval_single_agg_expr(table, mask, &proj.expr);
        row.insert(proj.alias.clone(), val);
    }
    row
}

fn eval_single_agg_expr(table: &ColumnarTable, mask: &BitVec64, expr: &str) -> Value {
    let u = expr.trim().to_uppercase();
    if u.starts_with("ROUND(") && u.ends_with(')') {
        let inner = &expr.trim()[6..expr.trim().len() - 1];
        let parts = split_top_level_commas(inner);
        if let Some(first) = parts.first() {
            let v = eval_single_agg_expr(table, mask, first);
            if let Some(f) = v.as_f64() {
                let rounded = (f * 100.0).round() / 100.0;
                return serde_json::Number::from_f64(rounded)
                    .map(Value::Number)
                    .unwrap_or(Value::Null);
            }
            return v;
        }
    }

    if u.starts_with("COUNT(") && u.ends_with(')') {
        let inner = expr.trim()[6..expr.trim().len() - 1].trim();
        if inner == "*" {
            return Value::Number((mask.count_ones() as i64).into());
        }
        let is_distinct = inner.to_uppercase().starts_with("DISTINCT ");
        let col_raw = if is_distinct { &inner[9..] } else { inner };
        let col_name = resolve_col_name(table, col_raw);
        if let Some(col) = table.get_col(&col_name) {
            if is_distinct {
                let mut set = std::collections::HashSet::new();
                for idx in mask.iter_ones() {
                    if let Some(s) = col.store.get_scalar_string(idx, false) {
                        set.insert(s);
                    }
                }
                return Value::Number((set.len() as i64).into());
            }
            let cnt = mask
                .iter_ones()
                .into_iter()
                .filter(|&idx| col.store.get_json_value(idx).is_some())
                .count();
            return Value::Number((cnt as i64).into());
        }
        return Value::Number((mask.count_ones() as i64).into());
    }

    for (prefix, mode) in [("SUM(", 0), ("AVG(", 1), ("MIN(", 2), ("MAX(", 3)] {
        if u.starts_with(prefix) && u.ends_with(')') {
            let inner = expr.trim()[prefix.len()..expr.trim().len() - 1].trim();
            let col_name = resolve_col_name(table, inner);
            if let Some(col) = table.get_col(&col_name) {
                match &col.store {
                    ColumnStore::Int(ic) => {
                        if let Some((_, sum, avg, mn, mx)) = ic.aggregate_masked(mask) {
                            let res = match mode {
                                0 => Value::Number((sum.round() as i64).into()),
                                1 => serde_json::Number::from_f64(avg)
                                    .map(Value::Number)
                                    .unwrap_or(Value::Null),
                                2 => Value::Number((mn.round() as i64).into()),
                                _ => Value::Number((mx.round() as i64).into()),
                            };
                            return res;
                        }
                    }
                    ColumnStore::Float(fc) => {
                        if let Some((_, sum, avg, mn, mx)) = fc.aggregate_masked(mask) {
                            let target = match mode {
                                0 => sum,
                                1 => avg,
                                2 => mn,
                                _ => mx,
                            };
                            let rounded = (target * 100.0).round() / 100.0;
                            return serde_json::Number::from_f64(rounded)
                                .map(Value::Number)
                                .unwrap_or(Value::Null);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    Value::Null
}

fn execute_group_by(
    table: &ColumnarTable,
    mask: &BitVec64,
    group_col_names: &[String],
    parsed: &ParsedSelect,
) -> Result<Value, String> {
    let mut group_cols = Vec::with_capacity(group_col_names.len());
    for gname in group_col_names {
        let col = table
            .get_col(gname)
            .ok_or_else(|| format!("GROUP BY column not found: {}", gname))?;
        group_cols.push((gname.as_str(), col));
    }

    let mut groups: HashMap<Vec<String>, BitVec64> = HashMap::new();
    let mut group_order: Vec<Vec<String>> = Vec::new();

    for idx in mask.iter_ones() {
        let mut key_tuple = Vec::with_capacity(group_cols.len());
        let mut all_present = true;
        for &(_, col) in &group_cols {
            if let Some(val_str) = col.store.get_scalar_string(idx, false) {
                key_tuple.push(val_str);
            } else {
                all_present = false;
                break;
            }
        }
        if all_present {
            let entry = groups.entry(key_tuple.clone()).or_insert_with(|| {
                group_order.push(key_tuple);
                BitVec64::new_empty(table.row_count)
            });
            entry.set(idx);
        }
    }

    let mut out_rows = Vec::with_capacity(group_order.len());
    for g_tuple in group_order {
        let g_mask = &groups[&g_tuple];
        let mut obj = Map::new();
        for proj in &parsed.projections {
            let resolved_proj = resolve_col_name(table, &proj.expr);
            if let Some(pos) = group_col_names.iter().position(|g| g == &resolved_proj) {
                let raw_val = &g_tuple[pos];
                let json_val = match &group_cols[pos].1.store {
                    ColumnStore::Bool(_) => Value::Bool(raw_val == "true"),
                    ColumnStore::Int(_) => raw_val
                        .parse::<i64>()
                        .map(|n| Value::Number(n.into()))
                        .unwrap_or_else(|_| Value::String(raw_val.clone())),
                    _ => Value::String(raw_val.clone()),
                };
                obj.insert(proj.alias.clone(), json_val);
            } else {
                obj.insert(
                    proj.alias.clone(),
                    eval_single_agg_expr(table, g_mask, &proj.expr),
                );
            }
        }
        out_rows.push(obj);
    }

    if let Some((order_col_raw, desc)) = &parsed.order_by {
        let order_col = clean_ident(order_col_raw);
        out_rows.sort_by(|a, b| {
            let va = a.get(&order_col).or_else(|| a.get(order_col_raw));
            let vb = b.get(&order_col).or_else(|| b.get(order_col_raw));
            let cmp = compare_json_vals(va, vb);
            if *desc { cmp.reverse() } else { cmp }
        });
    }

    if let Some(lim) = parsed.limit {
        out_rows.truncate(lim);
    }

    Ok(Value::Array(out_rows.into_iter().map(Value::Object).collect()))
}

fn execute_csr_join_sql(
    parent: &ColumnarTable,
    child: &ColumnarTable,
    parsed: &ParsedSelect,
) -> Result<Value, String> {
    let (parent_mask, child_mask) = match &parsed.where_clause {
        Some(w) => (
            eval_where_bitmap(parent, w)?,
            eval_where_bitmap(child, w)?,
        ),
        None => (
            BitVec64::new_all_ones(parent.row_count),
            BitVec64::new_all_ones(child.row_count),
        ),
    };

    let mut joined_child_mask = BitVec64::new_empty(child.row_count);
    let mut joined_parent_mask = BitVec64::new_empty(parent.row_count);
    let mut distinct_parents = std::collections::HashSet::new();

    for c_idx in child_mask.iter_ones() {
        let Some(&p_id_u32) = child.parent_row_ids.get(c_idx) else {
            continue;
        };
        let p_idx = p_id_u32 as usize;
        if parent_mask.get(p_idx) {
            joined_child_mask.set(c_idx);
            joined_parent_mask.set(p_idx);
            distinct_parents.insert(p_idx);
        }
    }

    let has_agg = parsed.projections.iter().any(|p| is_aggregate_expr(&p.expr));
    if !has_agg {
        let is_star = parsed.projections.len() == 1 && parsed.projections[0].expr == "*";
        let mut rows_out: Vec<Map<String, Value>> = Vec::new();
        for c_idx in joined_child_mask.iter_ones() {
            let p_idx = child.parent_row_ids[c_idx] as usize;
            if is_star {
                let mut row_map = parent.reconstruct_flat_row_json(p_idx, false);
                for (k, v) in child.reconstruct_flat_row_json(c_idx, false) {
                    row_map.insert(k, v);
                }
                rows_out.push(row_map);
            } else {
                let mut obj = Map::new();
                for proj in &parsed.projections {
                    let c_col = resolve_col_name(child, &proj.expr);
                    let p_col = resolve_col_name(parent, &proj.expr);
                    let val = child
                        .get_col(&c_col)
                        .and_then(|c| c.store.get_json_value(c_idx))
                        .or_else(|| {
                            parent
                                .get_col(&p_col)
                                .and_then(|c| c.store.get_json_value(p_idx))
                        })
                        .unwrap_or(Value::Null);
                    obj.insert(proj.alias.clone(), val);
                }
                rows_out.push(obj);
            }
        }
        if let Some((order_col_raw, desc)) = &parsed.order_by {
            let order_col = clean_ident(order_col_raw);
            rows_out.sort_by(|a, b| {
                let va = a.get(&order_col).or_else(|| a.get(order_col_raw));
                let vb = b.get(&order_col).or_else(|| b.get(order_col_raw));
                let cmp = compare_json_vals(va, vb);
                if *desc { cmp.reverse() } else { cmp }
            });
        }
        if let Some(off) = parsed.offset {
            if off < rows_out.len() {
                rows_out.drain(0..off);
            } else {
                rows_out.clear();
            }
        }
        if let Some(lim) = parsed.limit {
            rows_out.truncate(lim);
        }
        return Ok(Value::Array(rows_out.into_iter().map(Value::Object).collect()));
    }

    let mut obj = Map::new();
    for proj in &parsed.projections {
        let u = proj.expr.to_uppercase();
        if u.starts_with("COUNT(DISTINCT ") {
            obj.insert(
                proj.alias.clone(),
                Value::Number((distinct_parents.len() as i64).into()),
            );
        } else if u == "COUNT(*)" {
            obj.insert(
                proj.alias.clone(),
                Value::Number((joined_child_mask.count_ones() as i64).into()),
            );
        } else {
            let mut v = eval_single_agg_expr(child, &joined_child_mask, &proj.expr);
            if v.is_null() {
                v = eval_single_agg_expr(parent, &joined_parent_mask, &proj.expr);
            }
            obj.insert(proj.alias.clone(), v);
        }
    }

    Ok(Value::Array(vec![Value::Object(obj)]))
}

fn compare_json_vals(a: Option<&Value>, b: Option<&Value>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(Value::Number(na)), Some(Value::Number(nb))) => na
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&nb.as_f64().unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal),
        (Some(Value::String(sa)), Some(Value::String(sb))) => sa.cmp(sb),
        _ => std::cmp::Ordering::Equal,
    }
}
