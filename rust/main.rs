use std::env;
use std::fs;
use std::io::{self, Read};
use std::time::Instant;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use jsonbit::{
    deserialize_snapshot, execute_sql, load_json_or_jolap, run_stdio_mcp_server,
    save_snapshot_file, serialize_snapshot, JsonOlapEngine,
};

#[derive(Deserialize)]
struct BatchBenchmarkInput {
    datasets: Vec<BatchDataset>,
    eval_datasets: Vec<BatchDataset>,
    questions: Vec<BatchQuestion>,
}

#[derive(Deserialize)]
struct BatchDataset {
    name: String,
    data: Value,
}

#[derive(Deserialize)]
struct BatchQuestion {
    id: String,
    dataset: String,
    prompt: String,
    #[serde(default)]
    sql: Option<String>,
}

#[derive(Serialize)]
struct BatchDatasetResult {
    name: String,
    raw_json_bytes: usize,
    compressed_bytes: usize,
    jolap_bytes: usize,
    ingest_us: u128,
    jolap_load_us: u128,
    card_text: String,
    digest_text: String,
    lossless_rehydrate: bool,
}

#[derive(Serialize)]
struct BatchQuestionResult {
    id: String,
    answer: String,
    context: String,
    latency_us: u128,
    sql_result: Option<Value>,
    sql_latency_us: u128,
}

#[derive(Serialize)]
struct BatchBenchmarkOutput {
    dataset_results: Vec<BatchDatasetResult>,
    question_results: Vec<BatchQuestionResult>,
}

fn print_help() {
    eprintln!(
        r#"
jsonbit (v1.0.0) — Pure-Rust Auto-3NF Bit-Packed Columnar & BitVec64 Engine

Usage:
  jsonbit card      <file.json|.jsonl|.jsonbit>
  jsonbit digest    <file.json|.jsonl|.jsonbit>
  jsonbit ask       <file.json|.jsonl|.jsonbit> "<natural language question>"
  jsonbit answer    <file.json|.jsonl|.jsonbit> "<natural language question>"
  jsonbit sql       <file.json|.jsonl|.jsonbit> "<SELECT ...>"
  jsonbit pack      <input.json|.jsonl> <output.jsonbit>
  jsonbit rehydrate <file.json|.jsonl|.jsonbit>
  jsonbit mcp       (Starts stdio JSON-RPC 2.0 MCP Server for OpenCode / Claude Code / Codex / Cursor)
"#
    );
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 || args[1] == "--help" || args[1] == "-h" {
        print_help();
        return;
    }
    if args[1] == "--version" || args[1] == "-V" {
        println!("jsonbit {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let cmd = args[1].as_str();

    if cmd == "mcp" {
        run_stdio_mcp_server();
        return;
    }

    if cmd == "eval-batch" {
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf).expect("Failed to read stdin");
        let input: BatchBenchmarkInput =
            serde_json::from_str(&buf).expect("Failed to parse BatchBenchmarkInput");

        let mut dataset_results = Vec::with_capacity(input.datasets.len());
        for ds in &input.datasets {
            let raw_json_bytes = serde_json::to_string(&ds.data).unwrap_or_default().len();
            let t0 = Instant::now();
            let eng = JsonOlapEngine::from_json(&ds.data);
            let ingest_us = t0.elapsed().as_micros();
            let compressed_bytes = eng.compressed_byte_size();
            let snap_bytes = serialize_snapshot(&eng);
            let jolap_bytes = snap_bytes.len();
            let t_snap = Instant::now();
            let _ = deserialize_snapshot(&snap_bytes);
            let jolap_load_us = t_snap.elapsed().as_micros();
            let card_text = eng.data_card();
            let digest_text = eng.holistic_digest();
            let rehydrated = eng.rehydrate();
            let lossless_rehydrate = rehydrated == ds.data;

            dataset_results.push(BatchDatasetResult {
                name: ds.name.clone(),
                raw_json_bytes,
                compressed_bytes,
                jolap_bytes,
                ingest_us,
                jolap_load_us,
                card_text,
                digest_text,
                lossless_rehydrate,
            });
        }

        let mut eval_engines = std::collections::HashMap::new();
        for eds in &input.eval_datasets {
            eval_engines.insert(eds.name.clone(), JsonOlapEngine::from_json(&eds.data));
        }

        let mut question_results = Vec::with_capacity(input.questions.len());
        for q in &input.questions {
            if let Some(eng) = eval_engines.get(&q.dataset) {
                let slice = eng.query_slice(&q.prompt);
                let (sql_result, sql_latency_us) = if let Some(sql_str) = &q.sql {
                    let t_sql = Instant::now();
                    let res = execute_sql(eng, sql_str).ok();
                    (res, t_sql.elapsed().as_micros())
                } else {
                    (None, 0)
                };
                question_results.push(BatchQuestionResult {
                    id: q.id.clone(),
                    answer: slice.answer,
                    context: slice.context,
                    latency_us: slice.latency_us,
                    sql_result,
                    sql_latency_us,
                });
            }
        }

        let out = BatchBenchmarkOutput {
            dataset_results,
            question_results,
        };
        println!("{}", serde_json::to_string(&out).unwrap());
        return;
    }

    if args.len() < 3 {
        print_help();
        std::process::exit(1);
    }

    let file_path = &args[2];
    let t0 = Instant::now();
    let engine = load_json_or_jolap(file_path).unwrap_or_else(|e| {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    });
    let ingest_us = t0.elapsed().as_micros();

    match cmd {
        "pack" => {
            let out_path = args.get(3).unwrap_or_else(|| {
                eprintln!("Usage: jsonbit pack <input.json> <output.jsonbit>");
                std::process::exit(1);
            });
            let raw_bytes = fs::metadata(file_path).map(|m| m.len() as usize).unwrap_or(0);
            let written = save_snapshot_file(&engine, out_path).unwrap_or_else(|e| {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            });
            let saved_pct = if raw_bytes > 0 {
                (1.0 - (written as f64 / raw_bytes as f64)) * 100.0
            } else {
                0.0
            };
            println!(
                "Packed {} ({} bytes) -> {} ({} bytes, {:.1}% smaller)",
                file_path, raw_bytes, out_path, written, saved_pct
            );
        }
        "card" => {
            println!("{}", engine.data_card());
            let raw_bytes = fs::metadata(file_path).map(|m| m.len() as usize).unwrap_or(0);
            let comp_bytes = engine.compressed_byte_size();
            let ratio = if raw_bytes > 0 {
                (1.0 - (comp_bytes as f64 / raw_bytes as f64)) * 100.0
            } else {
                0.0
            };
            eprintln!(
                "\n--- Rust Bit-Packed Storage Stats ---\nInput File Size:      {:>10} bytes\nBit-Packed 3NF Size:  {:>10} bytes ({:.1}% smaller)\nLoad/Ingest Latency:  {:>10} µs",
                raw_bytes, comp_bytes, ratio, ingest_us
            );
        }
        "digest" => {
            println!("{}", engine.holistic_digest());
        }
        "ask" => {
            let question = args.get(3).map(|s| s.as_str()).unwrap_or("");
            let res = engine.query_slice(question);
            println!("{}", res.context);
            eprintln!("\n[Rust BitVec64 Execution Latency: {} µs]", res.latency_us);
        }
        "answer" => {
            let question = args.get(3).map(|s| s.as_str()).unwrap_or("");
            println!("{}", engine.answer_scalar(question));
        }
        "sql" => {
            let sql_q = args.get(3).map(|s| s.as_str()).unwrap_or("");
            let q_t0 = Instant::now();
            match execute_sql(&engine, sql_q) {
                Ok(val) => {
                    let q_us = q_t0.elapsed().as_micros();
                    println!("{}", serde_json::to_string_pretty(&val).unwrap());
                    eprintln!("\n[Rust BitVec64 SQL Execution Latency: {} µs]", q_us);
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        "rehydrate" => {
            println!("{}", serde_json::to_string_pretty(&engine.rehydrate()).unwrap());
        }
        _ => {
            print_help();
            std::process::exit(1);
        }
    }
}
