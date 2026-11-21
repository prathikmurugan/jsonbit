# Contributing to `jsonbit`

Thank you for your interest in contributing to `jsonbit`! Whether you are fixing a bug, improving compression or SQL coverage, adding tests, or clarifying documentation, we welcome your help.

Please review our [Code of Conduct](./CODE_OF_CONDUCT.md) before participating.

---

## Core Design Principles

Before opening a large PR, please keep `jsonbit`'s core invariants in mind:

1. **100% Lossless Round-Trip:** `rehydrate(from_json(x)) == x` and `rehydrate(deserialize(serialize(from_json(x)))) == x` must hold for all valid JSON inputs — including explicit nulls, empty arrays, colliding sanitized keys (e.g. `a-b` vs `a_b`), Unicode strings, and `u64 > i64::MAX`.
2. **General-Purpose Scope:** `jsonbit` is a general-purpose JSON/JSONL analytical & compaction engine. Avoid domain-specific heuristics (e.g. hardcoded security, log, or vertical-specific schemas).
3. **Zero External Runtime Dependencies:** The Rust core binary (`rust/`) depends only on `serde` and `serde_json`. Keep the binary fast, self-contained, and warning-free.
4. **Honest, Reproducible Benchmarks:** Any performance or token-reduction claim must be backed by directly measured outputs against `target/release/jsonbit`.

---

## Project Layout

```text
rust/
  bitmap.rs      # 64-bit word-aligned BitVec64 & POPCNT operations
  column.rs      # ForIntColumn, DictStringColumn, BoolBitmapColumn, FloatColumn
  normalizer.rs  # Auto-3NF normalizer, CSR 1-to-Many indexer, lossless rehydrator
  digest.rs      # 7-stage BPE-aware factorized holistic digest generator
  engine.rs      # JsonOlapEngine, data_card(), bitmap query_slice()
  query_exec.rs  # Read-only SQL parser & BitVec64 / CSR JOIN executor
  snapshot.rs    # Binary .jsonbit serializer & deserializer
  mcp.rs         # Concurrent stdio JSON-RPC 2.0 MCP server
  main.rs        # CLI entrypoint
src/             # TypeScript / Node.js SDK wrapper (@jsonbit/core)
python/jsonbit/  # Python SDK wrapper (jsonbit)
test/            # Vitest integration tests
benchmarks/      # Workloads, adversarial stress tests, and benchmark scripts
```

---

## Local Development Setup

### Prerequisites
- **Rust** `1.75+` (`cargo`, `rustc`)
- **Node.js** `20+` (`npm`) for TypeScript SDK tests & benchmarks
- **Python** `3.10+` (optional, for testing the Python SDK)

### Build & Run All Tests

```bash
# 1. Build the optimized Rust binary (must compile with 0 warnings)
cargo build --release

# 2. Run Rust unit tests
cargo test --release

# 3. Install Node dev dependencies and run Vitest integration tests
npm ci
npm test

# 4. Run the 15-case adversarial stress-test suite
npx tsx benchmarks/stress-test-all-cases.ts
```

---

## Pull Request Workflow

1. **Fork and branch** from `main` using a descriptive branch name (e.g. `fix/sql-like-escaping` or `feat/digest-rle`).
2. **Keep changes focused** — one logical change per PR.
3. **Add tests** covering your change and any edge cases (nulls, escaping, collisions, empty inputs).
4. **Verify locally** that all checks pass before opening the PR:
   - `cargo build --release` (`0` compiler warnings)
   - `cargo test --release`
   - `npm test`
   - `npx tsx benchmarks/stress-test-all-cases.ts`
5. **Open a Pull Request** targeting `main` with a clear summary of what changed, why, and how it was verified.

---

## Reporting Bugs & Suggesting Features

- **Bugs:** Open a [GitHub Issue](https://github.com/prathikmurugan/jsonbit/issues) with a minimal reproducing JSON/JSONL input, the exact `jsonbit` command or SQL query, the expected output, and the actual output.
- **Security Issues:** Please do not file public issues for security vulnerabilities. Report them privately via [GitHub Security Advisories](https://github.com/prathikmurugan/jsonbit/security/advisories/new).
