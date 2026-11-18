# Agent Instructions for `jsonbit` (Claude Code, Codex, Cursor, Windsurf)

Whenever you need to inspect, analyze, filter, aggregate, or query any `.json`, `.jsonl`, or `.jsonbit` file larger than 2 KB, **NEVER dump the raw JSON file into context**. Instead, use `jsonbit` (via MCP tools or the native Rust CLI) to save **96%+ of tokens** and get **100% exact mathematical answers in ~6 microseconds**:

## 1. MCP Tools (Automatically Mounted via `.mcp.json` / `opencode.json`)
- `olap_load({ filePath })`: Normalizes any nested JSON/JSONL file into Pure-Rust 3NF Bit-Packed Columnar tables and returns a ~250-token Semantic Cube Card.
- `olap_digest({ filePath })`: Generates a 1-Turn Factorized 3NF Holistic Digest (100% of rows + pivot legend) for qualitative/narrative summaries.
- `olap_ask({ filePath, question })`: Executes natural-language lookups, aggregations, multi-condition filters, and 1-to-Many CSR joins on `BitVec64` indices.
- `olap_sql({ filePath, sql })`: Executes read-only `SELECT ... WHERE ... GROUP BY ... ORDER BY ... LIMIT ...` queries directly on the Rust Bit-Packed store (supports JSON dot-paths like `customer.name` and `error.retryable`).
- `olap_rehydrate({ alias })`: Reconstructs the exact nested JSON document from the 3NF tables.

## 2. Direct Rust CLI Usage (For Codex / Bash Tool Agents)
```bash
# 1. Inspect 3NF Cube Card (~250 tokens instead of 150,000 tokens)
./target/release/jsonbit card <file.json|.jsonl|.jsonbit>

# 2. Emit 1-Turn Factorized Holistic Digest (100% of rows, ~40-70% smaller than TOON)
./target/release/jsonbit digest <file.json|.jsonl|.jsonbit>

# 3. Ask an analytical / filter question in ~15 µs
./target/release/jsonbit ask <file.json|.jsonl|.jsonbit> "How many log entries for /api/users have status >= 500?"

# 4. Execute deterministic SQL directly on Rust Bit-Packed columns
./target/release/jsonbit sql <file.json|.jsonl|.jsonbit> "SELECT endpoint, COUNT(*) AS cnt FROM records WHERE statusCode >= 500 GROUP BY endpoint"

# 5. Pack JSON into a compact binary .jsonbit file
./target/release/jsonbit pack input.json output.jsonbit
```
