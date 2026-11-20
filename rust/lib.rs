pub mod bitmap;
pub mod column;
pub mod engine;
pub mod mcp;
pub mod normalizer;
pub mod query_exec;
pub mod snapshot;

pub use bitmap::BitVec64;
pub use column::{
    BoolBitmapColumn, CmpOp, ColumnStore, DictStringColumn, FloatColumn, ForIntColumn,
};
pub use engine::{JsonOlapEngine, SliceOutput};
pub type JsonBitEngine = JsonOlapEngine;
pub use mcp::{parse_json_or_jsonl, run_stdio_mcp_server, RustMcpServer};
pub use normalizer::{normalize_json_to_3nf, rehydrate_from_3nf, ColumnarTable};
pub use query_exec::execute_sql;
pub use snapshot::{
    deserialize_snapshot, load_json_or_jolap, load_snapshot_file, save_snapshot_file,
    serialize_snapshot,
};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_bit_packing_and_for_int_column() {
        let vals = vec![Some(200), Some(503), Some(404), None, Some(200)];
        let col = ForIntColumn::from_options(&vals);
        assert_eq!(col.min_val, 200);
        assert_eq!(col.max_val, 503);
        assert_eq!(col.bit_width, 9);
        assert_eq!(col.get(0), Some(200));
        assert_eq!(col.get(1), Some(503));
        assert_eq!(col.get(2), Some(404));
        assert_eq!(col.get(3), None);
        assert_eq!(col.get(4), Some(200));

        let gte_500 = col.filter_cmp(CmpOp::Gte, 500.0);
        assert_eq!(gte_500.count_ones(), 1);
        assert!(gte_500.get(1));
    }

    #[test]
    fn test_codd_lossless_3nf_and_csr_child_join() {
        let data = json!({
            "orders": [
                {
                    "orderId": "ORD-0001",
                    "customer": { "id": 1, "name": "Alice" },
                    "items": [
                        { "sku": "SKU-1", "name": "Wireless Mouse", "quantity": 3, "price": 50.25 },
                        { "sku": "SKU-2", "name": "USB Cable", "quantity": 2, "price": 10.5 }
                    ],
                    "total": 171.75,
                    "status": "shipped"
                },
                {
                    "orderId": "ORD-0002",
                    "customer": { "id": 2, "name": "Bob" },
                    "items": [
                        { "sku": "SKU-1", "name": "Wireless Mouse", "quantity": 4, "price": 50.25 }
                    ],
                    "total": 201.5,
                    "status": "pending"
                }
            ]
        });

        let eng = JsonOlapEngine::from_json(&data);
        assert_eq!(eng.rehydrate(), data);
        assert_eq!(
            eng.answer_scalar("What is the total quantity of Wireless Mouse ordered across all orders?"),
            "7"
        );
        assert_eq!(
            eng.answer_scalar("How many shipped orders include a USB Cable?"),
            "1"
        );

        // Test Pure-Rust SQL execution with JSON dot-paths (`customer.name`)
        let sql_res = execute_sql(
            &eng,
            "SELECT orderId, customer.name AS cname, total FROM orders WHERE status = 'shipped'",
        )
        .unwrap();
        assert_eq!(
            sql_res,
            json!([{ "orderId": "ORD-0001", "cname": "Alice", "total": 171.75 }])
        );

        // Test `.jolap` binary snapshot round-trip
        let snap_bytes = serialize_snapshot(&eng);
        let restored_eng = deserialize_snapshot(&snap_bytes).unwrap();
        assert_eq!(restored_eng.rehydrate(), data);
        assert_eq!(
            restored_eng.answer_scalar("How many shipped orders include a USB Cable?"),
            "1"
        );
    }

    #[test]
    fn test_rust_mcp_server_and_concurrent_queries() {
        let mcp = RustMcpServer::new();
        let init = mcp
            .handle_request(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
            .unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "jsonbit-rust-mcp");

        let jsonl = "{\"id\":1,\"endpoint\":\"/api/users\",\"statusCode\":503,\"msg\":\"Connection timeout to replica db-01\",\"error\":{\"retryable\":true}}\n{\"id\":2,\"endpoint\":\"/api/orders\",\"statusCode\":200,\"msg\":\"Request completed successfully\"}";
        let _ = mcp.handle_request(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "olap_load",
                "arguments": { "json": jsonl, "alias": "logs" }
            }
        }));

        // Spawn 8 concurrent worker threads querying the same loaded cube simultaneously
        std::thread::scope(|s| {
            for t_id in 0..8 {
                let server_clone = mcp.clone();
                s.spawn(move || {
                    let sql_resp = server_clone
                        .handle_request(&json!({
                            "jsonrpc": "2.0",
                            "id": 100 + t_id,
                            "method": "tools/call",
                            "params": {
                                "name": "olap_sql",
                                "arguments": {
                                    "alias": "logs",
                                    "sql": "SELECT endpoint, statusCode FROM records WHERE error.retryable = true AND msg LIKE '%timeout%'"
                                }
                            }
                        }))
                        .unwrap();
                    let text = sql_resp["result"]["content"][0]["text"].as_str().unwrap();
                    assert!(text.contains("/api/users"));
                    assert!(text.contains("503"));
                });
            }
        });
    }

    #[test]
    fn test_colliding_keys_explicit_singleton_nulls_and_large_u64() {
        // 1. Field collision in singleton: {"a-b": 1, "a_b": 2}
        let colliding_singleton = json!({ "a-b": 1, "a_b": 2 });
        let eng1 = JsonBitEngine::from_json(&colliding_singleton);
        assert_eq!(eng1.rehydrate(), colliding_singleton);
        let snap1 = serialize_snapshot(&eng1);
        assert_eq!(deserialize_snapshot(&snap1).unwrap().rehydrate(), colliding_singleton);

        // 2. Field collision in array of objects + SQL disambiguation
        let colliding_rows = json!([
            { "a-b": 10, "a_b": 20 },
            { "a-b": 30, "a_b": 40 }
        ]);
        let eng2 = JsonBitEngine::from_json(&colliding_rows);
        assert_eq!(eng2.rehydrate(), colliding_rows);
        let snap2 = serialize_snapshot(&eng2);
        let restored2 = deserialize_snapshot(&snap2).unwrap();
        assert_eq!(restored2.rehydrate(), colliding_rows);
        let sql2 = execute_sql(&restored2, "SELECT a_b, a_b_2 FROM records ORDER BY a_b ASC").unwrap();
        assert_eq!(sql2, json!([{ "a_b": 10, "a_b_2": 20 }, { "a_b": 30, "a_b_2": 40 }]));

        // 3. Explicit null keys in singleton (top-level and nested)
        let singleton_nulls = json!({ "x": null, "nested": { "y": null, "z": 42 } });
        let eng3 = JsonBitEngine::from_json(&singleton_nulls);
        assert_eq!(eng3.rehydrate(), singleton_nulls);
        let snap3 = serialize_snapshot(&eng3);
        assert_eq!(deserialize_snapshot(&snap3).unwrap().rehydrate(), singleton_nulls);

        // 4. Unsigned 64-bit integers above i64::MAX (must not drop or lose precision)
        let large_u64 = json!([
            { "id": 1, "big": 18446744073709551615u64 },
            { "id": 2, "big": 9223372036854775808u64 }
        ]);
        let eng4 = JsonBitEngine::from_json(&large_u64);
        assert_eq!(eng4.rehydrate(), large_u64);
        let snap4 = serialize_snapshot(&eng4);
        assert_eq!(deserialize_snapshot(&snap4).unwrap().rehydrate(), large_u64);

        let singleton_u64 = json!({ "max_u64": 18446744073709551615u64, "empty": null });
        let eng5 = JsonBitEngine::from_json(&singleton_u64);
        assert_eq!(eng5.rehydrate(), singleton_u64);
        let snap5 = serialize_snapshot(&eng5);
        assert_eq!(deserialize_snapshot(&snap5).unwrap().rehydrate(), singleton_u64);
    }

    #[test]
    fn test_digest_escaping_and_edge_cases() {
        // Small payload (micro-card path, <= 8 rows): commas, pipes, quotes, backslashes, newlines, Unicode
        let small = json!([
            {
                "id": 1,
                "note": "New York, NY | \"HQ\" \\ line1\nline2",
                "alias": "$0",
                "empty_str": "",
                "explicit_null": null,
                "unicode": "東京 🚀 café"
            },
            {
                "id": 2,
                "note": "plain",
                "alias": "normal",
                "empty_str": "x",
                "unicode": "München"
            }
        ]);
        let eng_small = JsonBitEngine::from_json(&small);
        assert_eq!(eng_small.rehydrate(), small);
        let small_digest = eng_small.holistic_digest();
        assert!(small_digest.contains(r#""New York, NY | \"HQ\" \\ line1\nline2""#));
        assert!(!small_digest.contains("line1\nline2"));

        // Multi-row nested payload (> 8 rows): tests @dict quoting, literal `$0` escaping, pipe escaping in child rows
        let mut orders = Vec::new();
        for i in 0..10 {
            orders.push(json!({
                "orderId": format!("ORD-{:03}", i + 1),
                "note": if i == 0 { "a, b | \"c\" \\ d\ne" } else if i == 1 { "$0" } else { "Standard Shipping Label" },
                "items": [
                    { "sku": format!("SKU-{}", i), "label": if i == 0 { "Pipe | & Comma, \"Item\"" } else { "Cable" } }
                ]
            }));
        }
        let multi = json!({ "orders": orders });
        let eng_multi = JsonBitEngine::from_json(&multi);
        assert_eq!(eng_multi.rehydrate(), multi);
        let multi_snap = serialize_snapshot(&eng_multi);
        assert_eq!(deserialize_snapshot(&multi_snap).unwrap().rehydrate(), multi);

        let multi_digest = eng_multi.holistic_digest();
        assert!(multi_digest.contains(r#"$0="Standard Shipping Label""#));
        assert!(multi_digest.contains(r#""a, b | \"c\" \\ d\ne""#));
        assert!(multi_digest.contains(r#""$0""#));
        assert!(multi_digest.contains(r#""Pipe | & Comma, \"Item\"""#));
    }
}
