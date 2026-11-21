use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::sync::{Arc, RwLock};
use serde_json::{json, Value};
use crate::engine::JsonOlapEngine;
use crate::query_exec::execute_sql;
use crate::snapshot::load_json_or_jolap;

pub fn parse_json_or_jsonl(raw: &str) -> Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Value::Object(serde_json::Map::new());
    }
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return v;
    }
    let records: Vec<Value> = trimmed
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    let mut map = serde_json::Map::new();
    map.insert("records".to_string(), Value::Array(records));
    Value::Object(map)
}

#[derive(Clone)]
pub struct RustMcpServer {
    pub sessions: Arc<RwLock<HashMap<String, Arc<JsonOlapEngine>>>>,
}

impl RustMcpServer {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Thread-safe concurrent lookup/load: acquires a brief write lock only when
    /// loading a new dataset, and returns a lock-free `Arc<JsonOlapEngine>` for
    /// parallel read-only `olap_sql` / `olap_ask` queries across threads.
    pub fn get_or_load(
        &self,
        alias: &str,
        file_path: Option<&str>,
        raw_json: Option<&str>,
    ) -> Result<Arc<JsonOlapEngine>, String> {
        if let Some(fp) = file_path {
            let eng = Arc::new(load_json_or_jolap(fp)?);
            let mut guard = self.sessions.write().map_err(|e| e.to_string())?;
            guard.insert(alias.to_string(), Arc::clone(&eng));
            return Ok(eng);
        }

        if let Some(rj) = raw_json {
            let data = parse_json_or_jsonl(rj);
            let eng = Arc::new(JsonOlapEngine::from_json(&data));
            let mut guard = self.sessions.write().map_err(|e| e.to_string())?;
            guard.insert(alias.to_string(), Arc::clone(&eng));
            return Ok(eng);
        }

        let guard = self.sessions.read().map_err(|e| e.to_string())?;
        guard.get(alias).cloned().ok_or_else(|| {
            format!(
                "No dataset loaded under alias '{}'. Pass filePath or json, or call olap_load first.",
                alias
            )
        })
    }

    pub fn handle_request(&self, req: &Value) -> Option<Value> {
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id_opt = req.get("id");

        if method == "notifications/initialized" || id_opt.is_none() {
            return None;
        }
        let id = id_opt.unwrap().clone();

        if method == "initialize" {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": "jsonbit-rust-mcp",
                        "version": "1.0.0"
                    },
                    "instructions": "PREFERRED TOOL FOR .json, .jsonl, AND .jsonbit DATA FILES: Whenever the user asks to inspect, search, filter, count, sum, aggregate, join, or summarize .json, .jsonl, or .jsonbit files, ALWAYS call olap_load FIRST instead of using Read/cat/head or writing python/jq bash scripts. olap_load returns a ~250-token 3NF schema + precomputed aggregates; follow up with olap_sql for exact queries (including GROUP BY, LIKE, JOIN) or olap_digest for full-row qualitative summaries."
                }
            }));
        }

        if method == "ping" {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {}
            }));
        }

        if method == "tools/list" {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [
                        {
                            "name": "olap_load",
                            "description": "USE FIRST on any .json, .jsonl, or .jsonbit file instead of reading the raw file or running python/jq. Instantly indexes nested data into RAM and returns a ~250-token jsonbit Semantic Cube Card with exact table schemas, precomputed sums/averages/min/max, and category counts.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "filePath": { "type": "string", "description": "Path to .json, .jsonl, or .jsonbit file" },
                                    "json": { "type": "string", "description": "Raw JSON or JSONL payload" },
                                    "alias": { "type": "string", "description": "Session alias (default: 'default')" }
                                }
                            }
                        },
                        {
                            "name": "olap_ask",
                            "description": "Execute a natural-language analytical, lookup, filter, or 1-to-Many CSR join question against the BitVec64 indices in ~8 microseconds.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "question": { "type": "string", "description": "Natural-language question" },
                                    "filePath": { "type": "string", "description": "Optional file path to load" },
                                    "alias": { "type": "string", "description": "Session alias (default: 'default')" }
                                },
                                "required": ["question"]
                            }
                        },
                        {
                            "name": "olap_sql",
                            "description": "Execute a read-only SQL SELECT query directly against the Pure-Rust Bit-Packed Columnar store (supports WHERE AND/OR, IN, BETWEEN, LIKE, GROUP BY, ORDER BY, LIMIT, JSON dot-paths like customer.name or error.retryable, and parent-child CSR JOINs like `SELECT SUM(qty) FROM orders JOIN orders_items WHERE orders.status = 'shipped' AND orders_items.sku = 'SKU-USB-C'`).",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "sql": { "type": "string", "description": "Read-only SQL SELECT query" },
                                    "filePath": { "type": "string", "description": "Optional file path to load" },
                                    "alias": { "type": "string", "description": "Session alias (default: 'default')" }
                                },
                                "required": ["sql"]
                            }
                        },
                        {
                            "name": "olap_digest",
                            "description": "Generate a 1-Turn Factorized 3NF Holistic Digest containing 100% of all rows and fields plus the OLAP statistical header, using 35-70% fewer tokens than TOON for qualitative/holistic analysis.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "filePath": { "type": "string", "description": "Path to .json or .jsonl file" },
                                    "json": { "type": "string", "description": "Raw JSON or JSONL payload" },
                                    "alias": { "type": "string", "description": "Session alias (default: 'default')" }
                                }
                            }
                        },
                        {
                            "name": "olap_rehydrate",
                            "description": "Reconstruct the original nested JSON document from the Bit-Packed 3NF Columnar store (Codd Lossless-Join rehydration).",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "alias": { "type": "string", "description": "Session alias (default: 'default')" }
                                }
                            }
                        }
                    ]
                }
            }));
        }

        if method == "tools/call" {
            let params = req.get("params").and_then(|p| p.as_object());
            let tool_name = params
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let args = params
                .and_then(|p| p.get("arguments"))
                .and_then(|a| a.as_object());

            let alias = args
                .and_then(|a| a.get("alias"))
                .and_then(|v| v.as_str())
                .unwrap_or("default");
            let file_path = args.and_then(|a| a.get("filePath")).and_then(|v| v.as_str());
            let raw_json = args.and_then(|a| a.get("json")).and_then(|v| v.as_str());

            let outcome: Result<String, String> = match tool_name {
                "olap_load" => self
                    .get_or_load(alias, file_path, raw_json)
                    .map(|eng| eng.data_card()),
                "olap_digest" => self
                    .get_or_load(alias, file_path, raw_json)
                    .map(|eng| eng.holistic_digest()),
                "olap_ask" => {
                    let q = args
                        .and_then(|a| a.get("question"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    self.get_or_load(alias, file_path, raw_json)
                        .map(|eng| eng.query_slice(q).context)
                }
                "olap_sql" => {
                    let sql_q = args
                        .and_then(|a| a.get("sql"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    self.get_or_load(alias, file_path, raw_json)
                        .and_then(|eng| execute_sql(&eng, sql_q))
                        .map(|val| serde_json::to_string_pretty(&val).unwrap_or_default())
                }
                "olap_rehydrate" => self.get_or_load(alias, file_path, raw_json).map(|eng| {
                    serde_json::to_string_pretty(&eng.rehydrate()).unwrap_or_default()
                }),
                other => Err(format!("Unknown tool: {}", other)),
            };

            return match outcome {
                Ok(text) => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": text }]
                    }
                })),
                Err(err_msg) => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "isError": true,
                        "content": [{ "type": "text", "text": format!("Error: {}", err_msg) }]
                    }
                })),
            };
        }

        Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("Method not found: {}", method) }
        }))
    }
}

pub fn run_stdio_mcp_server() {
    let server = RustMcpServer::new();
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let Ok(l) = line else { break };
        let trimmed = l.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(req) => {
                if let Some(res) = server.handle_request(&req) {
                    let _ = writeln!(stdout, "{}", serde_json::to_string(&res).unwrap());
                    let _ = stdout.flush();
                }
            }
            Err(e) => {
                let err = json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32700, "message": format!("Parse error: {}", e) }
                });
                let _ = writeln!(stdout, "{}", serde_json::to_string(&err).unwrap());
                let _ = stdout.flush();
            }
        }
    }
}
